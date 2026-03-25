use super::*;

impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Tachi — memory + Hub for AI agents. Provides hybrid search, memory storage, skill registry, and MCP server proxy.")
    }

    fn list_tools(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let mut tools = self.tool_router.list_all();

            // Add proxy tools from registered MCP servers
            let proxy_snapshot = self
                .proxy_tools
                .lock()
                .map(|proxy| proxy.clone())
                .unwrap_or_default();
            for (server_name, server_tools) in proxy_snapshot {
                let cap_id = format!("mcp:{server_name}");
                let cap = match self.get_capability(&cap_id) {
                    Ok(cap) if cap.enabled => cap,
                    _ => continue,
                };

                let cap_def = serde_json::from_str::<serde_json::Value>(&cap.definition)
                    .unwrap_or_else(|_| json!({}));
                let exposure_mode =
                    resolve_mcp_tool_exposure(&cap_def, self.mcp_tool_exposure_mode);
                if exposure_mode == McpToolExposureMode::Gateway {
                    continue;
                }

                let filtered_tools =
                    filter_mcp_tools_by_permissions(&cap_def, server_tools.clone());

                for tool in filtered_tools {
                    let mut proxied = tool.clone();
                    proxied.name =
                        std::borrow::Cow::Owned(format!("{}__{}", server_name, tool.name));
                    tools.push(proxied);
                }
            }
            // Add skill tools
            if let Ok(skill_defs) = self.skill_tool_defs.lock() {
                tools.extend(skill_defs.values().cloned());
            }

            Ok(rmcp::model::ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        params: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let name = params.name.as_ref();

            // ─── Phantom Tools: cache invalidation on write ops ──────────
            if CACHE_INVALIDATING_TOOLS.contains(&name) {
                if let Ok(mut cache) = self.tool_cache.lock() {
                    cache.clear();
                }
            }

            // ─── Phantom Tools: check cache for read-only tools ──────────
            let is_cacheable = CACHEABLE_TOOLS.contains(&name);
            let cache_key = if is_cacheable {
                let args_str = params
                    .arguments
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                let key = stable_hash(&format!("{}{}", name, args_str));

                // Check cache
                if let Ok(cache) = self.tool_cache.lock() {
                    if let Some(cached) = cache.get(&key) {
                        if cached.created_at.elapsed() < TOOL_CACHE_TTL {
                            self.cache_hits
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return Ok(cached.result.clone());
                        }
                    }
                }
                self.cache_misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(key)
            } else {
                None
            };

            // ─── Dispatch to handler ─────────────────────────────────────
            // Save tool name and arguments for DLQ capture on failure
            let tool_name_owned = name.to_string();
            let tool_args_for_dlq = params.arguments.clone();

            let result = {
                // 1. Native tools first (highest priority)
                if self.tool_router.has_route(name) {
                    let context =
                        rmcp::handler::server::tool::ToolCallContext::new(self, params, context);
                    self.tool_router.call(context).await
                }
                // 2. Skill tools (tachi_skill_*)
                else if self
                    .skill_tools
                    .lock()
                    .map(|map| map.contains_key(name))
                    .unwrap_or(false)
                {
                    self.call_skill_tool(name, params.arguments).await
                }
                // 3. Proxy tools (server__tool pattern)
                else if let Some((server_name, tool_name)) = name.split_once("__") {
                    let exposure_mode = self.proxy_tool_exposure_mode_for_server(server_name)?;
                    if exposure_mode == McpToolExposureMode::Gateway {
                        Err(rmcp::ErrorData::invalid_params(
                            format!(
                                "Direct proxy tools are disabled for '{}'; use hub_call(server_id='mcp:{}', tool_name='{}')",
                                server_name, server_name, tool_name
                            ),
                            None,
                        ))
                    } else {
                        self.proxy_call_internal(server_name, tool_name, params.arguments)
                            .await
                    }
                } else {
                    Err(rmcp::ErrorData::invalid_params("tool not found", None))
                }
            };

            // ─── Dead Letter Queue: capture failures ─────────────────────
            // Skip DLQ for DLQ tools themselves and ghost_* tools
            let is_dlq_exempt = tool_name_owned.starts_with("dlq_")
                || tool_name_owned.starts_with("ghost_")
                || tool_name_owned == "get_pipeline_status";

            if let Err(ref err) = result {
                if !is_dlq_exempt {
                    let error_str = format!("{}", err);
                    let category = categorize_error(&error_str);
                    let should_auto_retry = category == "timeout" || category == "internal";

                    let dl = DeadLetter {
                        id: uuid::Uuid::new_v4().to_string(),
                        tool_name: tool_name_owned.clone(),
                        arguments: tool_args_for_dlq.clone(),
                        error: error_str.clone(),
                        error_category: category.clone(),
                        timestamp: Utc::now().to_rfc3339(),
                        retry_count: 0,
                        max_retries: if should_auto_retry { 1 } else { 3 },
                        status: "pending".to_string(),
                    };

                    let dl_id = dl.id.clone();
                    {
                        let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                        dlq.push_back(dl);
                        // Enforce ring buffer max
                        while dlq.len() > DLQ_MAX_ENTRIES {
                            dlq.pop_front();
                        }
                    }

                    // Auto-retry once for timeout/internal errors
                    if should_auto_retry {
                        // Brief delay before retry
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        // Retry via shared dispatch helper
                        let retry_result = self
                            .retry_dispatch(&tool_name_owned, tool_args_for_dlq)
                            .await;

                        {
                            let mut dlq =
                                self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(dl) = dlq.iter_mut().find(|dl| dl.id == dl_id) {
                                dl.retry_count = 1;
                                if retry_result.is_ok() {
                                    dl.status = "resolved".to_string();
                                } else {
                                    dl.status = "abandoned".to_string();
                                    if let Err(ref e) = retry_result {
                                        dl.error = format!("{e}");
                                    }
                                }
                            }
                        }

                        if retry_result.is_ok() {
                            // Cache the retry result if applicable
                            if let (Some(key), Ok(ref res)) = (&cache_key, &retry_result) {
                                if let Ok(mut cache) = self.tool_cache.lock() {
                                    cache.insert(
                                        key.clone(),
                                        CachedResult {
                                            result: res.clone(),
                                            created_at: Instant::now(),
                                        },
                                    );
                                }
                            }
                            return retry_result;
                        }
                    }
                }
            }

            // ─── Phantom Tools: store result in cache ────────────────────
            if let (Some(key), Ok(ref res)) = (&cache_key, &result) {
                if let Ok(mut cache) = self.tool_cache.lock() {
                    cache.insert(
                        key.clone(),
                        CachedResult {
                            result: res.clone(),
                            created_at: Instant::now(),
                        },
                    );
                }
            }

            result
        }
    }
}
