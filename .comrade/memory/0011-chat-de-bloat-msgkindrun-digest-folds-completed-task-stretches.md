# 0011 - Chat de-bloat: MsgKind::Run digest folds completed task stretches
status: accepted
tags: comrade-tui, tui, chat-layout, MsgKind, run-folding
summary: TUI chat folds each completed stretch of Tool/Reasoning/Failure/Meta between spoken messages into one MsgKind::Run digest row (children kept, unfold in place on Tab/search). Read tools render dimmed.

## Context
User complaint: the chat window is bloated by all the task/tool rows, useful but hard to follow. Fix implemented in crates/comrade-tui/src/tui.rs only. All indices (search matches, sel, msg_ranges, row_msg, mouse) are keyed to flat Vec<Msg> chat positions, so folding is a "replace span with one digest Msg" transform: digest holds the originals in Msg.children and unfold_run() splices them back in place — keeping per-card Tab/copy/search working without nested index machinery.

## Decision
1. MsgKind::Run + Msg.children: created by fold_completed_runs(chat)->replaced spans. A foldable span = maximal run of kinds {Tool, Reasoning, Failure, Meta} that contains >=1 Tool/Failure; "error:" prefixed Meta notes are boundaries and stay visible. Pure meta/reasoning-only spans never fold. 2. Fold points (App::fold_completed) at prose boundaries in on_agent_event: AgentEvent::User, FinalAnswer (before pushing the assistant msg), RunEnd, and delegate reply in on_delegate_result. Skipped while a Ctrl-F search is open. Children get their bodies auto-collapsed at fold time (stale open diffs close). 3. Tab/mouse on the digest row calls toggle_tool->expand_run->unfold_run and anchors sel on the first child. 4. msg_searchable/msg_matches recurse into children, so Ctrl-F finds text inside folded runs; goto_search_match unfolds the digest then jumps to the first child match >= anchor. 5. Layout: layout_run() emits one clickable header row "> task run · <author> · N calls ✓/✗ · <non-read actions>". 6. Visual tier: is_read_tool() names (read/list/rgrep/find/project_model/structural_map...) render dimmed in layout_tool and are excluded from the digest's action list.

## Consequences
Verify with: cargo test (workspace) and cargo check -p comrade-tui. Tests live in the tui.rs tests module under "run folding". Known trade-offs: (a) folding happens at prose boundaries, so a digest the user unfolds stays unfolded until the next boundary (next User/FinalAnswer/RunEnd) re-folds it; (b) auto-fold is skipped while a Ctrl-F search is open (fold_completed returns early), but goto_search_match still unfolds on demand; (c) clippy fails repo-wide for pre-existing errors in comrade-tool-syntax/src/engine.rs ("loop never actually loops"), unrelated to this change. If per-child keyboard drill inside an open digest is ever needed, revisit with a RowTarget enum rather than growing more flat-index hacks.

