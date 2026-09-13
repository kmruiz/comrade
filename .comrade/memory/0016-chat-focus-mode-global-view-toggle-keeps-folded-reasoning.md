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

