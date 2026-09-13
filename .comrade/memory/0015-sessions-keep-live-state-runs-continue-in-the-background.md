# 0015 - Sessions keep live state; runs continue in the background
status: accepted
date: 2026-09-13
tags: tui, session, concurrency, events
summary: Each open session owns a LiveState (swappable into the App); events are id-tagged per session, so new/switch/load/kill work mid-run and a parked session keeps running.

## Context
ADR 0013 modelled background sessions as serialized SessionFile snapshots and refused new-session/switch/save/load/fork/kill while a run was in flight. The human reported they could not open a new session while tasks were in flight, and asked that it work anyway. The blocker was structural: the App kept the active session's fields directly and a single untyped AgentEvent channel, so an in-flight run's events could not be routed away from a session the user switched to.

## Decision
Give every session (a) a stable u64 id and (b) its own bounded run-facing event channel relayed, tagged with that id, into the App's single central unbounded queue (`TaggedEvent = (u64, AgentEvent)`, `spawn_tagged_relay` in main.rs). Keep the active session's fields in the App as before, but add a `LiveState` struct holding every per-session field (session Arc, ctx_base, history, run_tx, stop, run_handle, running, steer_tx, queued_prompt, run_cancelled, chat, section_collapsed, chat_epoch, chat_rows_cache, stream, ctx_* , activity, session_file, sel, scroll_top, follow, was_at_bottom, search). `App::swap_live` mem::swaps those fields with a LiveState, so a session can be parked/activated in O(1) moves. `OpenSession { id, title, file, live: Option<Box<LiveState>> }`; invariant: the active slot's `live` is None. `on_agent_event_for(id, ev)` applies an event to the active session directly, or swaps a background session in, applies, and swaps back (handling_bg tracks the slot so refresh_active_slot writes to the right one). SessionFile serialization stays only for disk save/load. The "run in flight" refusals are removed for new-session/switch/load/kill and for the load prompt; save and fork still refuse when the ACTIVE session runs because they snapshot its rolling history, whose mutex the run holds.

## Rationale
Tagged, per-session event channels + a swappable per-session state struct is the minimum change that lets a run keep running while its session is off-screen: events stay attributed, and applying a background event reuses every existing handler unchanged (no rewrite of the ~hundreds of self.chat/self.session references). Holding several live AgentSession/ContextManager pairs is cheap (Arc + mutex), reversing the snapshot-only constraint of ADR 0013 as far as required.

## Alternatives considered
(a) Defer the new-session request until the run ends: does not give a usable session now. (b) Refactor App to hold `live: LiveState` for every session and rename all `self.chat` accesses: enormous, risky churn. (c) Tag events by extending AgentEvent in comrade-core: invasive and leaks UI routing into the core. (d) Per-session channels polled by a dynamic tokio::select set: awkward; a single tagged queue is simpler.

## Scope
Comrade TUI session lifecycle and event routing (crates/comrade-tui: tui.rs, main.rs). Does not change headless mode (still one throwaway session via main.rs::new_session) or the on-disk session format.

## Impact
New/switch/load/kill now work while runs are in flight; a parked session keeps streaming into its own chat and shows a [running] marker in the session switcher. Save/fork of the active session still require it to be idle. Approvals for a background run surface in the shared dialog and their meta note lands in the active chat (follow-up: attribute dialogs to the owning session). Cancel (Esc) cancels the active session's run only. Adds `LiveState`, `swap_live`, `on_agent_event_for`, `spawn_tagged_relay`, `session_bundle`, `build_app`; supersedes the snapshot-only and in-flight-refusal parts of ADR 0013.

