//! Connect to an MCP server (stdio child process or streamable HTTP) and
//! expose every tool it advertises as a local [`comrade_tool::Tool`].

use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use comrade_core::{McpServerCfg, McpTransport};
use comrade_tool::Tool;
use rmcp::service::{Peer, RoleClient, RunningService};
use serde_json::Value;
use tokio::process::Command;

use crate::tool::{McpToolAdapter, RemoteCall};

/// Name-prefixed local adapters for every tool a server advertises.
pub async fn connect_one(cfg: &McpServerCfg) -> Result<Vec<Box<dyn Tool>>> {
    let session = ServerSession::connect(cfg).await?;
    session
        .list_tools()
        .await
        .with_context(|| format!("tools/list failed for MCP server {}", cfg.name))
}

/// Connect every configured server, skipping (with a warning on stderr) any
/// that fails so one dead server never takes the whole session down.
pub async fn connect_all(cfgs: &[McpServerCfg]) -> Vec<Box<dyn Tool>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    for cfg in cfgs {
        match connect_one(cfg).await {
            Ok(tools) => {
                eprintln!(
                    "[comrade] MCP server {}: {} tool(s) connected",
                    cfg.name,
                    tools.len()
                );
                out.extend(tools);
            }
            Err(e) => eprintln!(
                "[comrade] MCP server {}: skipped ({:#})",
                cfg.name,
                e.root_cause()
            ),
        }
    }
    out
}

/// A live MCP session: the running service keeps the transport and spawned
/// tasks alive for as long as any adapter clone is around.
struct ServerSession {
    name: String,
    /// Keep-alive: dropped only when the last tool adapter drops.
    _running: Arc<RunningService<RoleClient, ()>>,
    /// Cheap outbound RPC handle (a clone of the running service's peer).
    peer: Peer<RoleClient>,
}

impl ServerSession {
    async fn connect(cfg: &McpServerCfg) -> Result<Self> {
        let name = cfg.name.clone();
        let (running, peer) = match &cfg.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
            } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    let resolved = comrade_core::expand_env_value(v, &|n| std::env::var(n).ok());
                    cmd.env(k, resolved);
                }
                cmd.stdin(std::process::Stdio::piped());
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                cmd.kill_on_drop(true);
                let transport =
                    rmcp::transport::child_process::TokioChildProcess::new(cmd).with_context(
                        || format!("failed to spawn MCP server command {command:?}"),
                    )?;
                let running =
                    rmcp::service::serve_client((), transport).await.context(
                        format!("MCP handshake with {name} failed"),
                    )?;
                let peer: Peer<RoleClient> = running.clone();
                (running, peer)
            }
            McpTransport::Http { url } => {
                let config = crate::auth::http_transport_config(url, cfg.auth.as_ref()).await?;
                let transport =
                    rmcp::transport::streamable_http_client::StreamableHttpClientTransport::from_config(config);
                let running =
                    rmcp::service::serve_client((), transport).await.context(
                        format!("MCP HTTP handshake with {name} failed"),
                    )?;
                let peer: Peer<RoleClient> = running.clone();
                (running, peer)
            }
        };
        Ok(Self {
            name,
            _running: Arc::new(running),
            peer,
        })
    }

    /// Fetch the advertised tools and wrap each in a local adapter.
    async fn list_tools(&self) -> Result<Vec<Box<dyn Tool>>> {
        let listed = self.peer.list_tools(Default::default()).await?;
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Box<dyn Tool>> = Vec::new();
        for t in listed.tools {
            let local = crate::tool::mcp_tool_name(&self.name, &t.name);
            if !seen.insert(local) {
                eprintln!(
                    "[comrade] MCP server {}: skipping duplicate tool name {:?}",
                    self.name, t.name
                );
                continue;
            }
            let call = PeerCall {
                peer: self.peer.clone(),
                tool: t.name.to_string(),
            };
            out.push(Box::new(McpToolAdapter::new(
                &self.name,
                &t.name,
                t.description.as_deref(),
                t.schema_as_json_value(),
                Box::new(call),
            )));
        }
        Ok(out)
    }
}

/// Calls `tools/call` on a shared peer for one remote tool and renders the
/// returned content blocks into the text the agent sees.
struct PeerCall {
    peer: Peer<RoleClient>,
    tool: String,
}

#[async_trait]
impl RemoteCall for PeerCall {
    async fn call(&self, args: Value) -> Result<String> {
        let arguments = args.as_object().cloned().unwrap_or_default();
        let req = rmcp::model::CallToolRequestParams::new(self.tool.clone())
            .with_arguments(arguments);
        let resp = self.peer.call_tool(req).await?;
        render_call_tool_result(resp)
    }
}

/// Best-effort text rendering of an MCP tool result. Text content blocks are
/// concatenated; anything else is noted rather than silently dropped. Tool
/// errors (`is_error`) surface as a distinguishable anyhow error.
fn render_call_tool_result(resp: rmcp::model::CallToolResult) -> Result<String> {
    let is_error = resp.is_error.unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    for block in &resp.content {
        match block {
            rmcp::model::ContentBlock::Text(t) => parts.push(t.text.clone()),
            other => parts.push(format!("<{other:?}>")),
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Ok(format!(
            "(MCP tool returned no text content; {} block(s))",
            resp.content.len()
        ));
    }
    if is_error {
        Err(anyhow!("MCP tool error:\n{text}"))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comrade_tool::ToolSpec;
    use rmcp::transport::async_rw::AsyncRwTransport;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    /// A hand-rolled MCP *server* loop (raw JSON-RPC, no SDK) running over one
    /// half of an in-memory duplex stream, while the rmcp client talks over the
    /// other half. Mirrors the fixture server used by the stdio path.
    async fn run_fixture_server(stream: tokio::io::DuplexStream) {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(&mut reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
                continue;
            };
            let id = msg.get("id").cloned();
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            let reply: Option<Value> = match method {
                "initialize" => Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": params
                            .get("protocolVersion")
                            .cloned()
                            .unwrap_or_else(|| json!("2025-03-26")),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fixture", "version": "0.0.1" }
                    }
                })),
                "notifications/initialized" | "notifications/cancelled" => None,
                "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
                "tools/list" => Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [
                        {
                            "name": "echo",
                            "description": "Echo the message back",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "message": { "type": "string" } },
                                "required": ["message"]
                            }
                        },
                        {
                            "name": "add",
                            "description": "Add two integers",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "a": { "type": "integer" },
                                    "b": { "type": "integer" }
                                },
                                "required": ["a", "b"]
                            }
                        },
                        {
                            "name": "always_error",
                            "description": "Always fails with isError",
                            "inputSchema": { "type": "object", "properties": {} }
                        }
                    ] }
                })),
                "tools/call" => {
                    let name = params["name"].as_str().unwrap_or_default();
                    let args =
                        params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    match name {
                        "echo" => Some(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "content": [{
                                "type": "text",
                                "text": format!("echo: {}", args["message"].as_str().unwrap_or(""))
                            }] }
                        })),
                        "add" => Some(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "content": [{
                                "type": "text",
                                "text": (args["a"].as_i64().unwrap_or(0)
                                    + args["b"].as_i64().unwrap_or(0))
                                    .to_string()
                            }] }
                        })),
                        "always_error" => Some(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": "boom" }],
                                "isError": true
                            }
                        })),
                        _ => Some(json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32601, "message": "unknown tool" }
                        })),
                    }
                }
                _ => Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                })),
            };
            if let Some(reply) = reply {
                let bytes = format!("{}\n", serde_json::to_string(&reply).unwrap());
                writer.write_all(bytes.as_bytes()).await.ok();
                writer.flush().await.ok();
            }
        }
    }

    async fn connect_fixture() -> Result<ServerSession> {
        let (client_side, server_side) = duplex(64 * 1024);
        tokio::spawn(run_fixture_server(server_side));
        let (client_read, client_write) = tokio::io::split(client_side);
        let transport = AsyncRwTransport::new_client(client_read, client_write);
        let running = rmcp::service::serve_client((), transport)
            .await
            .context("handshake with fixture server failed")?;
        let peer: Peer<RoleClient> = running.clone();
        Ok(ServerSession {
            name: "fixture".into(),
            _running: Arc::new(running),
            peer,
        })
    }

    #[tokio::test]
    async fn connects_lists_and_calls_tools() -> Result<()> {
        let session = connect_fixture().await?;
        let tools = session.list_tools().await?;
        let specs: Vec<&ToolSpec> = tools.iter().map(|t| t.spec()).collect();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["mcp_fixture_echo", "mcp_fixture_add", "mcp_fixture_always_error"]
        );
        assert!(specs[0].description.contains("MCP server `fixture`"));
        assert!(specs[0].json_schema["properties"]["message"].is_object());

        let call = PeerCall { peer: session.peer.clone(), tool: "echo".into() };
        assert_eq!(call.call(json!({ "message": "hi" })).await?, "echo: hi");

        let call = PeerCall { peer: session.peer.clone(), tool: "add".into() };
        assert_eq!(call.call(json!({ "a": 2, "b": 40 })).await?, "42");
        Ok(())
    }

    #[tokio::test]
    async fn tool_errors_surface_as_errors() -> Result<()> {
        let session = connect_fixture().await?;
        let call = PeerCall { peer: session.peer.clone(), tool: "always_error".into() };
        let err = call.call(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("MCP tool error"), "got: {err}");
        assert!(err.to_string().contains("boom"));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_tool_fails() -> Result<()> {
        let session = connect_fixture().await?;
        let call = PeerCall { peer: session.peer.clone(), tool: "nope".into() };
        assert!(call.call(json!({})).await.is_err());
        Ok(())
    }
}
