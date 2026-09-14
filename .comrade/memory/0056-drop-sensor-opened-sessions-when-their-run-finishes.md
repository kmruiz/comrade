# 0056 - Drop sensor-opened sessions when their run finishes
status: accepted
date: 2026-09-14
summary: A session opened by a proactive sensor is closed — and its temp file deleted — as soon as its run finishes, so recurring sensor runs do not accumulate in memory or on disk.

## Context
ADR 0054 handles every sensor change by opening a new session (`App::start_sensor_session`), titled `sensor: <name>` and backed by a `std::env::temp_dir()` file. Nothing ever removed those sessions from `App.open_sessions` or deleted their files, so each handled change left a session (its chat, history, metrics) resident for the life of the app and a temp file on disk. The human reported "sensors are kept in memory after they are done".

## Decision
Track which sessions a sensor opened with a new `OpenSession.sensor: bool` flag (set in `start_sensor_session`, `false` everywhere else). When a run ends — `AgentEvent::RunEnd` in `App::on_agent_event_for` — close a sensor-opened session: remove it from `open_sessions` (via a shared `close_session_at`) and delete its backing file (`close_finished_sensor_session`). The close happens only after the event is applied and, for a background session, after its swapped-in live state is put back, so the swap is never corrupted. Human-opened sessions are untouched. `kill_session` now calls the same `close_session_at`.

## Rationale
The session's chat/history/metrics are the memory that actually grows, so dropping the file alone is not enough; closing the session frees both. Hooking `RunEnd` at the single dispatch point (`on_agent_event_for`) covers the active and the parked (background) sensor session with one code path, while keeping the removal outside the live-state swap keeps the invariant intact. A per-session flag is the smallest change that distinguishes sensor sessions from human ones.

## Alternatives considered
["Delete only the temp file and keep the session open — rejected: the session's in-memory chat/history is the leak the user reported.", "Sweep idle sensor sessions lazily on app idle — rejected: nothing links a session to its sensor after opening, and a sweep is less obvious than closing at RunEnd.", "Close the session from inside `on_agent_event` — rejected: during a background event the session's LiveState is swapped into the App, so removing it there would corrupt the swap."]

## Scope
The sensor-session lifecycle in `crates/comrade-tui/src/tui.rs` (the `OpenSession.sensor` flag, `close_session_at`, `close_finished_sensor_session`, and the `on_agent_event_for` wiring). Does NOT cover persisting the sensors queue, per-sensor "seen" state across restarts, or the headless runner. Amends ADR 0054's session-per-change model.

## Impact
Recurring proactive runs are now bounded in memory and disk: `open_sessions` no longer grows one entry per handled change and temp files no longer leak. A finished sensor session disappears from the session switcher and cannot be re-saved/loaded (its temp file is gone) — intended, since it is machine-generated scratch. Covered by three unit tests in `crates/comrade-tui/src/tui.rs` (active sensor drop + temp-file removal, normal session survives, parked sensor session drop).

