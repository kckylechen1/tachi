use super::*;

impl MemoryServer {
    fn open_read_store(db_path: &PathBuf, label: &str) -> Result<MemoryStore, String> {
        let db_str = db_path.to_str().ok_or_else(|| {
            format!(
                "{} DB path contains invalid UTF-8: {}",
                label,
                db_path.display()
            )
        })?;
        MemoryStore::open(db_str).map_err(|e| format!("open {label} read store: {e}"))
    }

    pub(super) fn with_global_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let _gate = write_or_recover(&self.global_rw_gate, "global_rw_gate");
        let mut store = lock_or_recover(&self.global_store, "global_store");
        f(&mut store)
    }

    pub(super) fn with_global_store_read<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let _gate = read_or_recover(&self.global_rw_gate, "global_rw_gate");
        let mut store = Self::open_read_store(self.global_db_path.as_ref(), "global")?;
        f(&mut store)
    }

    pub(super) fn with_project_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let store_arc = self
            .project_store
            .as_ref()
            .ok_or_else(|| "No project database available (not in a git repository)".to_string())?;
        let gate = self
            .project_rw_gate
            .as_ref()
            .ok_or_else(|| "No project lock available".to_string())?;
        let _gate = write_or_recover(gate, "project_rw_gate");
        let mut store = lock_or_recover(store_arc, "project_store");
        f(&mut store)
    }

    pub(super) fn with_project_store_read<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let db_path = self
            .project_db_path
            .as_ref()
            .ok_or_else(|| "No project database available (not in a git repository)".to_string())?;
        let gate = self
            .project_rw_gate
            .as_ref()
            .ok_or_else(|| "No project lock available".to_string())?;
        let _gate = read_or_recover(gate, "project_rw_gate");
        let mut store = Self::open_read_store(db_path.as_ref(), "project")?;
        f(&mut store)
    }

    pub(super) fn with_store_for_scope<T>(
        &self,
        scope: DbScope,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        match scope {
            DbScope::Global => self.with_global_store(f),
            DbScope::Project => self.with_project_store(f),
        }
    }

    pub(super) fn resolve_write_scope(&self, requested: &str) -> (DbScope, Option<String>) {
        if requested == "global" {
            (DbScope::Global, None)
        } else if self.project_db_path.is_some() {
            (DbScope::Project, None)
        } else {
            (
                DbScope::Global,
                Some("No project DB available; saved to global".to_string()),
            )
        }
    }

    pub(super) fn get_capability(&self, cap_id: &str) -> Result<HubCapability, rmcp::ErrorData> {
        let mut found = None;
        if self.project_db_path.is_some() {
            found = self
                .with_project_store_read(|store| {
                    store
                        .hub_get(cap_id)
                        .map_err(|e| format!("hub get project: {e}"))
                })
                .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
        }
        if found.is_none() {
            found = self
                .with_global_store_read(|store| {
                    store
                        .hub_get(cap_id)
                        .map_err(|e| format!("hub get global: {e}"))
                })
                .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
        }
        found.ok_or_else(|| {
            rmcp::ErrorData::invalid_params(format!("Capability '{cap_id}' not found"), None)
        })
    }

    pub(super) fn proxy_tool_exposure_mode_for_server(
        &self,
        server_name: &str,
    ) -> Result<McpToolExposureMode, rmcp::ErrorData> {
        let cap_id = format!("mcp:{server_name}");
        let cap = self.get_capability(&cap_id)?;
        let def: serde_json::Value = serde_json::from_str(&cap.definition).map_err(|e| {
            rmcp::ErrorData::internal_error(
                format!("bad definition for capability '{cap_id}': {e}"),
                None,
            )
        })?;
        Ok(resolve_mcp_tool_exposure(&def, self.mcp_tool_exposure_mode))
    }

    pub(super) fn register_skill_tool(&self, cap: &HubCapability) -> Result<String, String> {
        let _ = self.unregister_skill_tool(&cap.id);
        let (tool_name, tool) = build_skill_tool_from_cap(cap)?;
        lock_or_recover(&self.skill_tools, "skill_tools").insert(tool_name.clone(), cap.id.clone());
        lock_or_recover(&self.skill_tool_defs, "skill_tool_defs").insert(tool_name.clone(), tool);
        Ok(tool_name)
    }

    pub(super) fn unregister_skill_tool(&self, skill_id: &str) -> Result<Option<String>, String> {
        let removed_tool_name = {
            let mut map = lock_or_recover(&self.skill_tools, "skill_tools");
            let tool_name = map
                .iter()
                .find(|(_, id)| id.as_str() == skill_id)
                .map(|(name, _)| name.clone());
            if let Some(ref name) = tool_name {
                map.remove(name);
            }
            tool_name
        };

        if let Some(ref name) = removed_tool_name {
            lock_or_recover(&self.skill_tool_defs, "skill_tool_defs").remove(name);
        }

        Ok(removed_tool_name)
    }

    pub(super) async fn call_skill_tool(
        &self,
        tool_name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let skill_id = self
            .skill_tools
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("WARNING: mutex poisoned: skill_tools; recovering with inner state");
                e.into_inner()
            })
            .get(tool_name)
            .cloned()
            .ok_or_else(|| {
                rmcp::ErrorData::invalid_params(
                    format!("Skill tool '{}' not found", tool_name),
                    None,
                )
            })?;

        let cap = self.get_capability(&skill_id)?;
        let def: Value = serde_json::from_str(&cap.definition).map_err(|e| {
            rmcp::ErrorData::invalid_params(format!("Invalid skill definition JSON: {e}"), None)
        })?;

        let args = arguments.unwrap_or_default();
        let args_value = Value::Object(args.clone());
        let args_json = serde_json::to_string_pretty(&args_value)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("serialize args: {e}"), None))?;

        let mut prompt = def
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| def.get("template").and_then(|v| v.as_str()))
            .unwrap_or("{{args_json}}")
            .to_string();
        prompt = prompt.replace("{{args_json}}", &args_json);
        prompt = prompt.replace("{{args}}", &args_json);
        for (k, v) in &args {
            let key = format!("{{{{{k}}}}}");
            prompt = prompt.replace(&key, &value_to_template_text(v));
        }
        if prompt.contains("{{input}}") {
            let input = args
                .get("input")
                .map(value_to_template_text)
                .unwrap_or_else(|| args_json.clone());
            prompt = prompt.replace("{{input}}", &input);
        }

        let output = if let Some(mock_response) = def.get("mock_response").and_then(|v| v.as_str())
        {
            mock_response.to_string()
        } else {
            let system = def
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("You are executing a reusable skill. Follow the instruction and produce the result.");
            let model = def.get("model").and_then(|v| v.as_str());
            let temperature = def
                .get("temperature")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.2) as f32;
            let max_tokens = def
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(1200) as u32;
            self.llm
                .call_llm(system, &prompt, model, temperature, max_tokens)
                .await
                .map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("skill execution failed: {e}"), None)
                })?
        };

        make_text_tool_result(&json!({
            "skill_id": skill_id,
            "tool_name": tool_name,
            "output": output
        }))
    }

    pub(super) async fn retry_dispatch(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let args_obj = arguments.map(|m| m.into_iter().collect::<rmcp::model::JsonObject>());

        if self
            .skill_tools
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("WARNING: mutex poisoned: skill_tools; recovering with inner state");
                e.into_inner()
            })
            .contains_key(tool_name)
        {
            return self.call_skill_tool(tool_name, args_obj).await;
        }

        if let Some((server_name, remote_tool)) = tool_name.split_once("__") {
            let exposure_mode = self.proxy_tool_exposure_mode_for_server(server_name)?;
            if exposure_mode == McpToolExposureMode::Gateway {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Direct proxy tool '{}' is disabled by tool_exposure=gateway for '{}'. Retry via hub_call.",
                        tool_name, server_name
                    ),
                    None,
                ));
            }
            return self
                .proxy_call_internal(server_name, remote_tool, args_obj)
                .await;
        }

        Err(rmcp::ErrorData::invalid_params(
            format!(
                "Native tool '{}' cannot be retried via DLQ — retry the MCP call directly",
                tool_name
            ),
            None,
        ))
    }
}
