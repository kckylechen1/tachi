//! CLI client glue: forward CLI subcommand invocations to a running Tachi
//! daemon when one is detected, otherwise fall back to a transient in-process
//! `MemoryServer` so we use the same code path as the MCP tool handlers.
//!
//! Detection strategy:
//!   1. Read `~/.tachi/daemon.pid` (written by `tachi --daemon` on startup).
//!   2. Confirm liveness with a quick TCP connect to the recorded port.
//!   3. If both succeed, forward via MCP `tools/call` over streamable HTTP.
//!   4. Otherwise, build an in-process `MemoryServer` and call the handler
//!      directly.
//!
//! All write paths (remember, wiki_write, extract_facts) use this so they go
//! through capture gate, provenance, auto-link, and enrichment exactly the
//! same way as the MCP tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;
use serde_json::Value;

use crate::MemoryServer;

const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const DAEMON_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Discovery info for a running Tachi daemon.
#[derive(Debug, Clone)]
pub(crate) struct DaemonInfo {
    pub url: String,
    #[allow(dead_code)]
    pub pid: u32,
    #[allow(dead_code)]
    pub port: u16,
}

/// Look up `~/.tachi/daemon.pid` and verify the daemon is actually listening.
/// Returns `None` if the file is missing, malformed, or the port is closed.
pub(crate) async fn detect_daemon(app_home: &Path) -> Option<DaemonInfo> {
    let pid_path = app_home.join("daemon.pid");
    let raw = tokio::fs::read_to_string(&pid_path).await.ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;

    let pid = parsed.get("pid").and_then(|v| v.as_u64())? as u32;
    let port = parsed.get("port").and_then(|v| v.as_u64())? as u16;
    let url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}/mcp"));

    // Quick TCP probe so we don't hang the CLI on a stale pid file.
    let addr = format!("127.0.0.1:{port}");
    let probe =
        tokio::time::timeout(DAEMON_PROBE_TIMEOUT, tokio::net::TcpStream::connect(&addr)).await;

    match probe {
        Ok(Ok(_)) => Some(DaemonInfo { url, pid, port }),
        _ => None,
    }
}

/// Call a tool over MCP-streamable-HTTP against a known daemon URL.
/// Returns the first text content block from the tool result.
pub(crate) async fn call_daemon_tool(
    info: &DaemonInfo,
    tool_name: &str,
    arguments: serde_json::Map<String, Value>,
) -> Result<String, String> {
    let transport_config = StreamableHttpClientTransportConfig::with_uri(info.url.clone());
    let transport = StreamableHttpClientTransport::from_config(transport_config);
    let client = ServiceExt::serve((), transport)
        .await
        .map_err(|e| format!("daemon handshake failed at {}: {e}", info.url))?;

    let mut params = CallToolRequestParams::new(tool_name.to_string());
    if !arguments.is_empty() {
        params = params.with_arguments(arguments);
    }

    let peer = client.peer().clone();
    let result = tokio::time::timeout(DAEMON_CALL_TIMEOUT, peer.call_tool(params))
        .await
        .map_err(|_| {
            format!(
                "daemon call '{tool_name}' timed out after {:?}",
                DAEMON_CALL_TIMEOUT
            )
        })?
        .map_err(|e| format!("daemon call '{tool_name}' failed: {e}"))?;

    // Best-effort cancel of the client session; ignore errors.
    let _ = client.cancel().await;

    if result.is_error.unwrap_or(false) {
        let err_text =
            first_text_block(&result.content).unwrap_or_else(|| "<no error text>".to_string());
        return Err(format!(
            "daemon tool '{tool_name}' returned error: {err_text}"
        ));
    }

    Ok(first_text_block(&result.content).unwrap_or_else(|| "{}".to_string()))
}

fn first_text_block(blocks: &[rmcp::model::Annotated<RawContent>]) -> Option<String> {
    blocks.iter().find_map(|c| match &c.raw {
        RawContent::Text(t) => Some(t.text.clone()),
        _ => None,
    })
}

/// Build a transient in-process `MemoryServer` for one-shot CLI use.
/// Uses the same constructor as the daemon, so capture gate, enrichment,
/// auto-link, etc. all run identically.
pub(crate) fn build_in_process_server(
    global_db: &PathBuf,
    project_db: Option<&PathBuf>,
) -> Result<MemoryServer, Box<dyn std::error::Error>> {
    let server = MemoryServer::new(global_db.clone(), project_db.cloned())?;
    Ok(server)
}
