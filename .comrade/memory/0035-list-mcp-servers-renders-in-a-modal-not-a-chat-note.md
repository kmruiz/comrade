# 0035 - list-mcp-servers renders in a modal, not a chat note
status: accepted
tags: comrade-tui, modal, mcp, tui.rs, ux
summary: M-x list-mcp-servers opens a centered scrollable modal (McpServersView) instead of pushing a chat Meta message

## Context
In crates/comrade-tui/src/tui.rs, the palette-only command list-mcp-servers used to push_meta a formatted list. Per user request it was converted to look like the Ctrl-A assign-model-to-step overlay.

## Decision
1. list_mcp_servers() (&mut self) now sets self.mcp_view = Some(McpServersView{..}); it still push_meta("no MCP servers configured") when cfg.mcp.servers is empty. 2. Row content is built by the pure fn mcp_server_rows(&[comrade_core::McpServerCfg]) -> Vec<McpServerRow> (name + detail lines; stdio shows command+args, http the url). 3. Modal state structs McpServerRow/McpServersView live near ModelPick (~line 330); App field mcp_view: Option<McpServersView> next to pick. 4. Rendering: draw_mcp_servers() (near draw_model_pick) — centered bordered popup titled ' MCP servers ', scroll clamped to content at draw time. 5. Keys handled in handle_mcp_key(): esc/enter/q(no-mod) close; up/down, pgup/pgdn, ctrl-p/n scroll. Dispatch added in handle_event() right after the pick block. Test: mcp_server_rows_renders_transports in mod tests.

## Consequences
The M-x handler runs with app.mx already None (handle_mx_key clears it before run_command), so no overlay conflict. If more read-only info modals appear, generalize McpServersView into a shared scrollable-rows popup.

