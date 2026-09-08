# 0027 - rmcp 3.2.0 client API surface (MCP bridge)
status: accepted
tags: mcp, rmcp, client, api
summary: rmcp 3.2.0 CLIENT API pinned: serve_client with () handler, Peer clone trick, stdio/http transports, non-exhaustive CallToolRequestParams

## Context
comrade-tool-mcp (crates/comrade-tool-mcp) implements an MCP client on rmcp 3.2.0 with features client, transport-child-process, transport-streamable-http-client-reqwest, auth, reqwest. Pinned client API for future sessions and delegates.

## Decision
1. Connect: `RunningService<RoleClient, ()> = rmcp::service::serve_client((), transport).await?` (handler () works; handshake inside). Imports rmcp::service::{serve_client, Peer, RoleClient, RunningService}. RunningService derefs to Peer and has no Clone; `let peer: Peer<RoleClient> = running.clone()` resolves through Deref to Peer::clone. Keep RunningService alive in an Arc or drop closes the connection. 2. stdio: TokioChildProcess::new(tokio Command with piped stdio + kill_on_drop). For in-process tests use rmcp::transport::async_rw::AsyncRwTransport::new_client(read, write) over a tokio duplex. 3. HTTP: StreamableHttpClientTransportConfig fields uri: Arc<str> (url.to_string().into()), auth_header: Option<String> (value for the Authorization header), custom_headers: HashMap<http::HeaderName, http::HeaderValue>. StreamableHttpClientTransport::from_config(cfg) returns the transport synchronously (no await/Result). 4. list: peer.list_tools(Default::default()) -> .tools: Vec<Tool>; Tool.name/description are Cow<'_,str>; schema via t.schema_as_json_value(). 5. call: CallToolRequestParams is non_exhaustive — CallToolRequestParams::new(name).with_arguments(serde_json::Map); result CallToolResult { content: Vec<ContentBlock>, is_error: Option<bool> }. 6. Do not write helpers naming IntoTransport<RoleClient,E,A>; inline serve_client so E/A infer.
VERIFY: cargo check/test -p comrade-tool-mcp green.

## Consequences
rmcp pulls its own reqwest 0.13 + aws-lc-rs; our crate separately uses workspace reqwest 0.12 for OIDC HTTP. Registry source is not reachable from project-scoped tools; rustdoc HTML under target/doc/src/rmcp/ has each source line on its own HTML line for reading. HTTP transport e2e vs a real streamable-http server still untested.

