# 0038 - MCP tools modal is now type-to-search; Esc-only close
status: accepted
tags: comrade-tui, tui.rs, mcp, modal, keybindings
summary: McpServersView modal: removed the `filtering` mode toggle — typing always filters live, Esc is the only close key (q/ctrl-g no longer close), Enter toggles rows.

## Context
User request on the M-x list-mcp-servers modal (McpServersView in crates/comrade-tui/src/tui.rs, decision #35/#37): stop requiring '/' to enter the filter; typing must filter immediately. And close with Escape only, not q. Supersedes the '/'|'f' filter entry and Esc/q/ctrl-g close keys from decision #37 point 4.

## Decision
1. Dropped the `filtering: bool` field from McpServersView (struct ~line 362, init in list_mcp_servers ~1288, test helper ~5855). 2. handle_mcp_key (~1404): Esc closes the modal (the ONLY close key — no q/ctrl-g arm); Backspace pops the filter; ctrl-u clears it; any Char(c) without ctrl/alt pushes into the filter; Enter activates the row (space no longer activates, it types); Tab/Up/Down/PgUp/PgDn/Left/Right/ctrl-p/ctrl-n navigate and collapse as before; mcp_clamp_sel() runs after every key. 3. draw_mcp_servers (~5124): single hint line "typing filters by server or tool name · ↑/↓ or ctrl-p/n select · enter toggles · tab collapses a group · esc closes"; the filter line always renders highlighted with a trailing cursor `|` (no more mode-dependent styling). Verification: `cargo test -p comrade-tui` green (90 tests).

## Consequences
Pressing 'q', 'f', or '/' while the modal is open now types those chars into the filter instead of closing/entering filter mode. Tool toggling is Enter (or space now inserts a space into the query). Esc semantics is now unambiguous: it always closes the modal.

