//! Adapter bridging a remote MCP tool to the local [`comrade_tool::Tool`]
//! contract used by the agent loop.

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde_json::{json, Value};

/// What the MCP connection executes when the agent invokes one remote tool.
///
/// Implemented in `connect` where the live MCP peer lives; the adapter only
/// forwards the validated argument object and renders whatever string the
/// connection returns.
#[async_trait]
pub trait RemoteCall: Send + Sync {
    /// Run `tools/call` for this tool with `args` (already validated against
    /// the advertised schema) and return the rendered result text.
    async fn call(&self, args: Value) -> Result<String>;
}

/// A [`comrade_tool::Tool`] that proxies to an MCP server tool.
pub struct McpToolAdapter {
    spec: ToolSpec,
    call: Box<dyn RemoteCall>,
}

impl McpToolAdapter {
    /// Build an adapter for `tool_name` on `server`.
    ///
    /// * The local tool name becomes `mcp_<server>_<tool>` (sanitised).
    /// * `description` and `input_schema` come from the server's `tools/list`
    ///   advertisement; a missing/empty schema falls back to an empty object.
    pub fn new(
        server: &str,
        tool_name: &str,
        description: Option<&str>,
        input_schema: Value,
        call: Box<dyn RemoteCall>,
    ) -> Self {
        let name = mcp_tool_name(server, tool_name);
        let description = match description {
            Some(d) if !d.trim().is_empty() => format!("[MCP server `{server}`] {d}"),
            _ => format!("MCP tool `{tool_name}` exposed by server `{server}`."),
        };
        let json_schema = if input_schema.is_object() {
            input_schema
        } else {
            json!({ "type": "object", "properties": {} })
        };
        Self {
            spec: ToolSpec {
                name,
                description,
                json_schema,
            },
            call,
        }
    }

    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        self.call.call(args).await
    }
}

/// Map an MCP server/tool pair onto a safe snake_case local tool name:
/// `mcp_<server>_<tool>`. Only `[A-Za-z0-9_]` survives; everything else
/// becomes `_`, runs of `_` collapse, and the result is lowercased so the
/// name is a stable single token for the ReAct/native protocols.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp_{}_{}", sanitise(server), sanitise(tool))
}

fn sanitise(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 1);
    let mut prev_underscore = false;
    for ch in raw.chars() {
        let keep = ch.is_ascii_alphanumeric();
        if keep {
            out.extend(ch.to_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    if out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(server: &str, tool: &str) -> String {
        mcp_tool_name(server, tool)
    }

    #[test]
    fn prefixes_and_sanitises() {
        assert_eq!(name("github", "list_repos"), "mcp_github_list_repos");
        assert_eq!(
            name("My Server", "do.The.Thing!"),
            "mcp_my_server_do_the_thing"
        );
        assert_eq!(name("filesystem", "read"), "mcp_filesystem_read");
    }

    #[test]
    fn empty_parts_never_yield_empty_token() {
        assert_eq!(name("", ""), "mcp_tool_tool");
        assert_eq!(name("srv", "@@@"), "mcp_srv_tool");
    }

    #[test]
    fn falls_back_to_empty_object_schema() {
        let t = McpToolAdapter::new("srv", "t", Some("desc"), json!(42), Box::new(Stub));
        assert_eq!(
            t.spec().json_schema,
            json!({ "type": "object", "properties": {} })
        );
    }

    #[test]
    fn keeps_object_schema_when_present() {
        let t = McpToolAdapter::new(
            "srv",
            "t",
            None,
            json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
            Box::new(Stub),
        );
        assert!(t.spec().json_schema.as_object().unwrap().contains_key("properties"));
    }

    struct Stub;
    #[async_trait]
    impl RemoteCall for Stub {
        async fn call(&self, _args: Value) -> Result<String> {
            Ok("stub".into())
        }
    }
}
