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
