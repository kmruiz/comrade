# 0013 - TUI sessions: file-backed switch/save/load/fork with Ctrl-x chords
status: accepted
date: 2026-09-13
tags: tui, session, persistence, keybindings
summary: Sessions own their plan+chat; the app keeps an in-memory open-session registry of snapshots, saves/loads to a user-picked file path, and exposes Ctrl-x C-b/C-s/C-f/C-w plus M-x counterparts.

## Context
The TUI had a single implicit session (comrade_core::AgentSession = title/status/plan/delegated/finished, plus a ContextManager rolling history and the visible Vec<Msg> chat). The human asked for emacs-like sessions: each session has its own plan and chat, switched/saved/loaded/forked with Ctrl-x b/s/f/w and equivalent M-x commands. Nothing was persisted to disk before.

## Decision
Model sessions as a whole-session snapshot (new module crates/comrade-tui/src/session_store.rs: SessionFile {version,title,status,plan,delegated,finished,chat,section_collapsed,ctx_*,history,rollup,evicted} + save/load/default_path). The App keeps `open_sessions: Vec<OpenSession>` + `active`: the active session's state lives in the App's own fields, every other slot holds a Box<SessionFile> snapshot. Ctrl-x is treated as a prefix key; C-b switch, C-s save, C-f load, C-w fork; PathPrompt/SessionPick overlays do the rest. Save/load prompt for a file path every time (find-file semantics, chosen by the human). Loading/forks restore a fresh AgentSession via AgentSession::restore and a ContextManager via from_parts. serde Serialize+Deserialize derives were added to Msg/MsgKind/ToolCard/TestFail (ToolCard.started skipped), PlanStep/PlanStatus, and ChatMessage/Role (ToolCallMsg got a manual Deserialize matching its manual Serialize).

## Rationale
Snapshotting into a serializable struct keeps one representation for save, load, fork and switch (fork = clone the snapshot), so the four commands share one code path and the on-disk format is testable in isolation. Prompting for a path each time matches emacs find-file and the human's explicit choice. Holding snapshots (not live sessions) for background sessions avoids running four AgentSessions/ContextManagers at once.

## Alternatives considered
(a) Hold several fully-live AgentSession/ContextManager pairs and switch between them: heavier, more state to keep in sync. (b) A fixed project-local store dir (e.g. .comrade/sessions/): rejected by the human in favour of prompting for a path. (c) Persist only the visible chat, not the model history: loses resume fidelity, so history+rollup+evicted are persisted too.

## Scope
Covers the TUI session lifecycle only. Does not add per-session model/autonomy, does not auto-save, and does not change the headless runner (which still gets one throwaway session).

## Impact
New dependency serde (derive) on comrade-tui; new serde derives across comrade-tool/comrade-core message types. Switch/save/load/fork refuse while a run is in flight. Transient UI state (stream/search/selection/scroll) is reset on switch. Follow-ups: optional auto-save, session listing/completion in the M-x palette, and persisting per-session model choice.


## Note
New-session is non-destructive (emacs C-x b <new-name> / scratch-buffer): M-x new-session now stashes the current session into its slot and opens a fresh empty slot as the active session, instead of wiping the current one in place. Added M-x kill-session (Ctrl-x C-k) to close the active session and activate a neighbour; it refuses to close the only open session. The session switcher title is refreshed on AgentEvent::TitleChanged so slots show the live title.

## Note
Superseded in part by ADR 0015: new-session/switch/load/kill are no longer refused while a run is in flight, and a background session now keeps a live LiveState (not only a serialized snapshot) so its run continues off-screen. Save/fork of the active session still refuse while its run is in flight. The file-backed save/load/fork model and the Ctrl-x chords from this ADR remain.

## Note
The Ctrl-x C-s / C-f path minibuffer now does emacs find-file style Tab completion (crates/comrade-tui/src/tui.rs): KeyCode::Tab in handle_path_prompt_key calls the free fn complete_path(input, base=self.root), which splits the typed path into (dir_part, prefix) via split_dir_prefix, reads that directory (read_dir_entries, dirs marked with a trailing `/`), and extends the name with complete_names — a single match completes fully (dir => trailing `/`), several matches extend to their longest common prefix (longest_common_prefix, reusing common_prefix from the M-x palette). `~` is expanded (expand_tilde) and a path with no directory part is read relative to the project root. Candidates are stored in PathPrompt.matches and shown by draw_path_matches (a popup modelled on draw_mx_list); they are cleared on the next Char/Backspace edit. All logic is pure/testable (complete_names/split_dir_prefix/complete_path unit tests).

## Merged from #0015 - Sessions keep live state; runs continue in the background
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


## Note
Follow-up implemented: dialogs/asks are now attributed to their owning session. Each session gets its own TuiUserIo (built in App::make_ctx_base with the session id; stored asks_tx on App), so PendingAsk and Dialog carry `session: u64`. A session whose run is blocked on a user dialog is shown as "waiting": the mode-line session-count label buckets open sessions into a mutually-exclusive running/blocked/idle partition via fn session_status_marker (waiting takes precedence over running), and the Ctrl-x C-b switcher appends "  [waiting]" instead of "  [running]".

## Note
Rollup of the session architecture: this ADR made sessions file-backed (switch/save/load/fork); #0015 gives each open session a live state so runs continue in the background and events are id-tagged per session. Body preserved under "Merged from".
