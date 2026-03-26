use super::*;

const MCP_PRESERVED_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_RUNTIME_DIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
];

fn apply_sanitized_child_env(cmd: &mut tokio::process::Command, env_map: &HashMap<String, String>) {
    cmd.env_clear();
    for var in MCP_PRESERVED_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    for (k, v) in env_map {
        cmd.env(k, v);
    }
}

fn parse_string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

fn apply_env_allowlist(
    env_map: HashMap<String, String>,
    allowlist: &[String],
) -> HashMap<String, String> {
    if allowlist.is_empty() {
        return env_map;
    }
    let allowed: std::collections::HashSet<&str> = allowlist.iter().map(String::as_str).collect();
    env_map
        .into_iter()
        .filter(|(k, _)| allowed.contains(k.as_str()))
        .collect()
}

fn normalize_path(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

fn path_within_roots(path: &std::path::Path, roots: &[String]) -> bool {
    if roots.is_empty() {
        return true;
    }
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    roots.iter().any(|root| {
        let root_path = normalize_path(root);
        candidate.starts_with(&root_path)
    })
}

fn resolve_env_map(def: &serde_json::Value) -> Result<HashMap<String, String>, String> {
    let mut result = HashMap::new();
    if let Some(obj) = def.get("env").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                let resolved = if val.starts_with("${") && val.ends_with('}') {
                    let var_name = &val[2..val.len() - 1];
                    std::env::var(var_name).map_err(|_| {
                        format!(
                            "Environment variable '{}' not set (required by MCP server)",
                            var_name
                        )
                    })?
                } else {
                    val.to_string()
                };
                result.insert(k.clone(), resolved);
            } else {
                return Err(format!(
                    "Invalid env value type for key '{}': expected string",
                    k
                ));
            }
        }
    } else if def.get("env").is_some() {
        return Err("Invalid env field: expected object".to_string());
    }
    Ok(result)
}

impl MemoryServer {
    pub(super) fn clear_proxy_tools(&self, server_name: &str) {
        let mut tools = lock_or_recover(&self.proxy_tools, "proxy_tools");
        tools.remove(server_name);
    }

    pub(super) fn cache_proxy_tools(&self, server_name: &str, tools: Vec<rmcp::model::Tool>) {
        lock_or_recover(&self.proxy_tools, "proxy_tools").insert(server_name.to_string(), tools);
    }

    pub(super) async fn connect_mcp_service(
        &self,
        capability_id: &str,
        def: &serde_json::Value,
        timeout: Duration,
    ) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>, String> {
        let policy = self.get_sandbox_policy_for_capability(capability_id);
        let policy_enabled = policy
            .as_ref()
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !policy_enabled {
            return Err(format!(
                "Sandbox policy disabled capability '{}'",
                capability_id
            ));
        }

        let policy_runtime = policy
            .as_ref()
            .and_then(|v| v.get("runtime_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("process");
        if policy_runtime != "process" && policy_runtime != "wasm" {
            return Err(format!(
                "Invalid sandbox runtime_type '{}' for '{}'",
                policy_runtime, capability_id
            ));
        }

        let policy_startup_ms = policy
            .as_ref()
            .and_then(|v| v.get("max_startup_ms"))
            .and_then(|v| v.as_u64())
            .unwrap_or(timeout.as_millis() as u64)
            .max(1);
        let effective_timeout = Duration::from_millis(
            std::cmp::min(timeout.as_millis() as u64, policy_startup_ms).max(1),
        );

        let transport_type = match def.get("transport") {
            Some(v) => match v.as_str() {
                Some(raw) => raw,
                None => {
                    eprintln!(
                        "[mcp] Invalid 'transport' field type; expected string, defaulting to 'stdio'"
                    );
                    "stdio"
                }
            },
            None => "stdio",
        };
        match transport_type {
            "stdio" => {
                if policy_runtime == "wasm" {
                    return Err(format!(
                        "Capability '{}' requires runtime_type=wasm but stdio transport was requested",
                        capability_id
                    ));
                }

                let command = def["command"]
                    .as_str()
                    .ok_or_else(|| "missing command".to_string())?;
                let args: Vec<String> = match def.get("args") {
                    Some(v) if v.is_null() => Vec::new(),
                    Some(v) => {
                        let array = v
                            .as_array()
                            .ok_or_else(|| "invalid args: expected string array".to_string())?;
                        let mut parsed = Vec::with_capacity(array.len());
                        for (idx, item) in array.iter().enumerate() {
                            let value = item.as_str().ok_or_else(|| {
                                format!("invalid args[{idx}]: expected string value")
                            })?;
                            parsed.push(value.to_string());
                        }
                        parsed
                    }
                    None => Vec::new(),
                };
                let env_map = resolve_env_map(def)?;
                let env_allowlist = parse_string_array(
                    policy
                        .as_ref()
                        .and_then(|v| v.get("env_allowlist")),
                );
                let env_map = apply_env_allowlist(env_map, &env_allowlist);

                let cwd_roots = parse_string_array(
                    policy
                        .as_ref()
                        .and_then(|v| v.get("cwd_roots")),
                );
                let cwd = def.get("cwd").and_then(|v| v.as_str());
                if !cwd_roots.is_empty() {
                    let cwd_str = cwd.ok_or_else(|| {
                        format!(
                            "Sandbox policy for '{}' requires cwd within allowed roots, but definition has no cwd",
                            capability_id
                        )
                    })?;
                    let cwd_path = normalize_path(cwd_str);
                    if !path_within_roots(&cwd_path, &cwd_roots) {
                        return Err(format!(
                            "Sandbox policy denied cwd '{}' for '{}'",
                            cwd_path.display(),
                            capability_id
                        ));
                    }
                }

                let mut cmd = tokio::process::Command::new(command);
                cmd.args(&args);
                cmd.kill_on_drop(true);
                apply_sanitized_child_env(&mut cmd, &env_map);
                if let Some(cwd_str) = cwd {
                    cmd.current_dir(normalize_path(cwd_str));
                }

                let transport = rmcp::transport::TokioChildProcess::new(cmd)
                    .map_err(|e| format!("spawn failed: {e}"))?;
                tokio::time::timeout(effective_timeout, rmcp::ServiceExt::serve((), transport))
                    .await
                    .map_err(|_| {
                        format!(
                            "MCP handshake timed out after {}ms",
                            effective_timeout.as_millis()
                        )
                    })?
                    .map_err(|e| format!("MCP handshake failed: {e}"))
            }
            "sse" | "http" | "streamable-http" => {
                let url = def["url"]
                    .as_str()
                    .ok_or_else(|| "missing url for SSE".to_string())?;
                let transport = StreamableHttpClientTransport::from_uri(url);
                tokio::time::timeout(effective_timeout, rmcp::ServiceExt::serve((), transport))
                    .await
                    .map_err(|_| {
                        format!(
                            "SSE handshake timed out after {}ms",
                            effective_timeout.as_millis()
                        )
                    })?
                    .map_err(|e| format!("SSE handshake failed: {e}"))
            }
            other => Err(format!("unsupported transport: {other}")),
        }
    }

    pub(super) async fn discover_mcp_tools(
        &self,
        capability_id: &str,
        def: &serde_json::Value,
    ) -> Result<Vec<rmcp::model::Tool>, String> {
        let client = self
            .connect_mcp_service(capability_id, def, self.mcp_discovery_timeout)
            .await?;
        let list_result =
            tokio::time::timeout(self.mcp_discovery_timeout, client.peer().list_all_tools()).await;
        let cancel_result = client.cancel().await;

        match list_result {
            Ok(Ok(tools)) => {
                let _ = cancel_result;
                Ok(tools)
            }
            Ok(Err(e)) => Err(format!("list_tools failed: {e}")),
            Err(_) => Err(format!(
                "list_tools timed out after {}ms",
                self.mcp_discovery_timeout.as_millis()
            )),
        }
    }
}
