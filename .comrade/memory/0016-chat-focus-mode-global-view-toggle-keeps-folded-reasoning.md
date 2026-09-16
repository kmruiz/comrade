# 0016 - Chat focus mode: global view toggle, keeps folded reasoning
status: accepted
date: 2026-09-13
tags: tui, chat, ui
summary: Focus mode (M-f) is a global chat view toggle that hides tool/failure/meta/digest rows while keeping the conversation and any reasoning folded inside digests.

## Context
Users wanted a "focus mode" that filters the chat down to the conversation (their messages, model replies, delegate advisories, reasoning, questions/answers) and hides tool calls so a long agent transcript stays readable.

## Decision
Focus mode is a GLOBAL App-level view toggle (field `focus_mode`), not stored in LiveState, not swapped per session and not persisted to the session file. It keeps MsgKind::User/Assistant/Delegate/Reasoning; drops MsgKind::Tool/Failure/Meta and a folded MsgKind::Run digest's summary row; a folded Run is still rendered as the Reasoning children inside it (user explicitly asked to keep folded reasoning). Toggled by M-f and by the M-x command `focus-mode`. Row layout is filtered in `layout_chat_rows` via the `focus_visible(&Msg)` predicate, `ChatRowsCache` gained a `focus` field for invalidation, and `chat_visible`/`step_visible`/`collect_matches` honour focus so selection and search never land on hidden rows.

## Rationale
Making it a global App field (like auto_accept) avoids touching LiveState, swap_live, snapshots and save/load; keying the row cache on `focus` handles per-session caches correctly without bumping chat_epoch. Keeping the reasoning inside folded digests satisfies "see the reasonings" since completed stretches fold reasoning into Run digests.

## Alternatives considered
(a) Per-session persisted flag in LiveState/SessionFile — rejected as unnecessary scope. (b) Hiding Run digests entirely — rejected: it would hide the folded reasoning the user wants to see. (c) Deriving visibility only at draw time without touching navigation/search — rejected: selection and Ctrl-S would jump to invisible rows.

## Scope
Covers the TUI chat view only. Does not change the underlying chat model, message folding, session persistence, or the headless runner.

## Impact
Toggling M-f re-filters rows, re-anchors the selection, and refreshes search; a "focus mode on/off" Meta note is pushed (hidden while on, since Meta is hidden in focus mode). No merge/UX impact on existing keybindings.


## Note
Follow-up (spinner): focus mode hides tool cards, so a run that streams no reasoning looked frozen. `draw_chat` now reserves the last chat row for a bottom-of-chat activity line (`activity_line`), showing a rotating Braille spinner (`spinner_glyph`, shared with `plan_glyph`) plus the running tool name or "working…". It is shown only when `app.focus_mode` and the active session status is `running` (`activity_spinner_visible`), i.e. hidden outside focus mode and while paused on a user dialog. Animated by the existing 100 ms `spin` interval guarded by `app.any_running()`. Chat rows area is reduced by one row (`rows_rect`/`view_h`), and `chat_rect` now points at that reduced rect so clicks on the spinner row are ignored.

## Note
Default-on (2026-09-14): focus mode is now ON when the app starts — `build_app` sets `focus_mode: true` (crates/comrade-tui/src/tui.rs). The rationale: the chat should open as pure conversation; users who want the tool noise toggle it off with M-f / M-x focus-mode. The mode line (bottom bar) now carries a focus-state segment right after the auto/ask token: "focus mode enabled" in bold light-green when on, "focus mode disabled" dim gray when off. `test_app()` inherits the new default, so `activity_spinner_shows_only_for_a_running_focus_mode_chat` sets `focus_mode = false` explicitly to keep testing the off case.

## Note
Diagrams survive focus mode (2026-10-05): with focus mode default-on, the `show_diagram` result — a picture the model drew *for the human*, not tool noise — was invisible in the normal startup view, both as a standalone `MsgKind::Tool` row (dropped by `focus_visible`) and, usually, as a child of a folded `MsgKind::Run` digest (whose focus branch re-rendered only its `Reasoning` children). crates/comrade-tui/src/tui.rs now keys a diagram off the result's own `DIAGRAM_OPEN`/`DIAGRAM_CLOSE` sentinel block via a new `diagram_block(&Msg) -> Option<&str>` helper rather than off the tool name, so any tool whose result embeds a diagram block is covered: `focus_visible` keeps such a Tool row (and a Run holding one), the Run focus branch calls the new `layout_diagram` for diagram children alongside its Reasoning children, and a standalone diagram Tool row renders through `layout_diagram` instead of `layout_tool` (no header, no args, no status mark). `layout_diagram` reuses `result_rows`, so the art is verbatim and never rewrapped — the same rendering the open card uses. Everything else in focus mode is unchanged: tool cards without a diagram, failure blocks and Meta notes stay hidden, and `chat_visible`/`step_visible`/`collect_matches` follow automatically because they all go through `focus_visible`.
