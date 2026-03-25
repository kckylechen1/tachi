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
        def: &serde_json::Value,
        timeout: Duration,
    ) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>, String> {
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

                let mut cmd = tokio::process::Command::new(command);
                cmd.args(&args);
                cmd.kill_on_drop(true);
                apply_sanitized_child_env(&mut cmd, &env_map);

                let transport = rmcp::transport::TokioChildProcess::new(cmd)
                    .map_err(|e| format!("spawn failed: {e}"))?;
                tokio::time::timeout(timeout, rmcp::ServiceExt::serve((), transport))
                    .await
                    .map_err(|_| {
                        format!("MCP handshake timed out after {}ms", timeout.as_millis())
                    })?
                    .map_err(|e| format!("MCP handshake failed: {e}"))
            }
            "sse" | "http" | "streamable-http" => {
                let url = def["url"]
                    .as_str()
                    .ok_or_else(|| "missing url for SSE".to_string())?;
                let transport = StreamableHttpClientTransport::from_uri(url);
                tokio::time::timeout(timeout, rmcp::ServiceExt::serve((), transport))
                    .await
                    .map_err(|_| {
                        format!("SSE handshake timed out after {}ms", timeout.as_millis())
                    })?
                    .map_err(|e| format!("SSE handshake failed: {e}"))
            }
            other => Err(format!("unsupported transport: {other}")),
        }
    }

    pub(super) async fn discover_mcp_tools(
        &self,
        def: &serde_json::Value,
    ) -> Result<Vec<rmcp::model::Tool>, String> {
        let client = self
            .connect_mcp_service(def, self.mcp_discovery_timeout)
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
