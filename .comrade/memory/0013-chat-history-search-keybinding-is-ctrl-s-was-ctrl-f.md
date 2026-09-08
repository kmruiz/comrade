# 0013 - Chat-history search keybinding is Ctrl-S (was Ctrl-F)
status: accepted
tags: comrade-tui, keybinding, search
summary: comrade-tui search over chat history is opened/toggled with Ctrl-S since commit ebf0990; Ctrl-F is unbound.

## Context
User asked to rebind the incremental chat-history search (Search struct in crates/comrade-tui/src/tui.rs) from Ctrl-F to Ctrl-S. Ctrl-S opens the search bar when closed and closes it when open (same toggle role Ctrl-F had); Esc also closes.

## Decision
Search now binds to Ctrl-S: open site in handle_event guards KeyCode::Char('s')+CONTROL (was 'f'); handle_search_key closes on Char('s')+CONTROL (was 'f'); MxCommand::SearchChat key hint is now "C-s"; all Ctrl-F comments updated. There is no Ctrl-S conflict in the TUI (no save binding exists).

## Consequences
If Ctrl-S ever needs to mean something else (terminal XON flow control is disabled in raw mode by crossterm), pick a different key. Tests: cargo test -p comrade-tui (63 pass) covers search behavior; no unit test presses the actual binding keys.

