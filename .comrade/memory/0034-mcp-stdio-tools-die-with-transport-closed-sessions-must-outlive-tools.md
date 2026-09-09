# 0034 - MCP stdio tools die with "Transport closed": sessions must outlive tools
status: accepted
tags: mcp, rmcp, comrade-tool-mcp, transport-closed, bugfix
summary: Debugging js-code-sandbox MCP server failing with "Transport closed": connect_one dropped the ServerSession (sole Arc<RunningService>) right after tools/list, killing the stdio child (kill_on_drop); tools kept only a bare Peer. Fixed by PeerCall holding its own Arc keep-alive clone.

## Context
User's configured MCP server (js-code-sandbox via `npx node-code-sandbox-mcp`, ~/.config/comrade/config.toml [[mcp.servers]]) registered its tools at startup but every tool call returned "Transport closed". The npx server runs fine standalone (verified with `timeout 30 npx -y node-code-sandbox-mcp </dev/null`). Root cause was in crates/comrade-tool-mcp/src/connect.rs, NOT the server.

## Decision
1. Symptom: tools for an MCP server appear in the session (startup handshake + tools/list succeeded) but each invocation returns "Transport closed". This pattern = the connection was dropped after registration. 2. In crates/comrade-tool-mcp/src/connect.rs, `connect_one` created a `ServerSession` (holding `Arc<RunningService<RoleClient,()>>` + peer), listed tools, then returned and DROPPED the session. Returned `McpToolAdapter`s held only `PeerCall { peer, tool }` (a bare Peer clone). rmcp closes the transport when RunningService drops (see decision #27), and the stdio path sets kill_on_drop(true), so the npx child died right after tools/list. 3. Fix (commit 6febf7d): `PeerCall` now carries `_keepalive: Arc<RunningService<RoleClient,()>>`; `ServerSession::call(&self, tool)` builds a PeerCall cloning `self._running`; `list_tools` uses `self.call(&t.name)`. Any tool adapter therefore keeps the connection (and child) alive until the last adapter drops. 4. Regression test `tool_outlives_the_dropped_session` (duplex fixture): list tools / build a call, drop the session, then call the tool - must still succeed. VERIFY: cargo test -p comrade-tool-mcp --lib (12 passed) and cargo check (workspace green).

## Consequences
Future MCP wiring must never drop the RunningService while any adapter clone lives: give each tool its own Arc clone. To debug "Transport closed" for a configured server: (1) check the [[mcp.servers]] entry's command/args in ~/.config/comrade/config.toml (or $COMRADE_CONFIG), (2) run the command standalone (`timeout 30 npx -y <pkg> </dev/null`) to prove the server itself starts, (3) then suspect the client keep-alive. Startup stderr prints "[comrade] MCP server <name>: N tool(s) connected" or "skipped (cause)" from connect_all.

