# 0012 - M-x command palette (Alt+X) in comrade-tui
status: accepted
tags: comrade-tui, tui, keybindings, M-x, command-palette, emacs
summary: Alt+X M-x palette in comrade-tui: 19 named commands (each existing keybinding), type-to-narrow completion, Enter runs, hint "you can run this command with <keys>" shown in the palette row when a binding exists

## Context
Feature added in crates/comrade-tui/src/tui.rs: an Emacs-style M-x palette so every keybinding has a descriptive command name, typeable and runnable, which then announces its keybinding in the palette row.

## Decision
1. Press Alt+X (KeyModifiers::ALT + 'x', handled in handle_event before chat editing but AFTER pick/search/dialog overlays) to open Mx::open().\n2. Typing narrows by substring of command name; ↑/↓ or Ctrl+P/N move; Tab completes; Enter runs the highlighted MxCommand; Esc / C-g / Alt+X close. Unhandled Ctrl/Alt chords dismiss the palette and fall through to normal dispatch (MxKeyOutcome::Closed).\n3. MxCommand enum (name/keys/desc) lists 19 commands incl. quit, cancel-run, toggle-auto-accept, search-chat-history, assign-model-to-step, copy, move-block/user up/down, word/line cursor editing. Keep MxCommand::ALL sorted and add new bindings there too.\n4. After Enter runs a command that HAS a keybinding, the palette stays open in hint mode (Mx.done) showing "you can run this command with <keys>"; the next key dismisses. Commands that open their own overlay (search/pick) skip the hint.\n5. run_command(&mut self, cmd) -> bool mirrors the original key-handler logic (quit returns true). Ctrl-Space handler was refactored into App::toggle_auto_accept() shared by both paths.\n6. Draw path: when app.mx.is_some() the prompt row (rows[1]) renders "M-x <query>_ ..." and draw_mx_list() draws the completion popup above it; search bar and mx bar share the prompt row.\n\nVERIFY: cargo test (workspace) green; cargo check green; `cargo clippy --manifest-path crates/comrade-tui/Cargo.toml --no-deps` shows no warnings in the mx code (pre-existing warnings remain at old line ranges).


## Note
Tab is now emacs-style prefix completion: Mx::complete() extends the query to the longest common prefix of all current matches (a single match completes to its full name; an empty query or no shared prefix leaves the query untouched so the full task list stays on screen). The old behaviour — Tab blindly replacing the query with the *highlighted* command's name — was removed; Enter still runs the highlighted command. Free function common_prefix(a,b) + tests mx_complete_fills_shared_prefix / common_prefix_shared in tui.rs tests.
