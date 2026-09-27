# 0083 - Proactive sensors: polled config sensors that notify, act, and queue changes
status: accepted
date: 2026-09-27
tags: sensors, proactive, config, tools, tui, comrade-tool-project
summary: Polled config sensors watch a shell command OR any registered tool on an interval and, on a change, notify and (mode=auto) open a session to handle it; received changes land in a sensors queue, and a session a sensor opened is dropped when its run finishes.

## Context
Comrade can be proactive: poll an external source and react without the human asking. This rollup consolidates the whole sensors feature - the polling model, tool-capable sensors, and the queue/session lifecycle.

## Decision
`[[sensors]]` config entries poll a shell command or any registered tool (MCP/skill/built-in) on an interval. The first poll only sets a baseline; each later change notifies the human and, with `mode = "auto"`, opens a session to act on it. Every received change is queued in the "sensors" panel (start entries with the `sensors-*` M-x commands), and a session a sensor opened is closed - and its temp file deleted - as soon as its run finishes, so recurring runs do not stack up. The merged sections below preserve the full detail.

## Merged: Proactive mode: polled config sensors that notify and act on changes
## Context
The human asked for a "proactive mode": config.toml should be able to tell the agent to listen to external sources "every now and then" (JIRA tickets, GitHub issues, …), detect changes and notify the user. Each "sensor" is configurable to be proactive (auto: the agent handles it) or ask (notify and let the human decide). The agent may delegate the listening work to a delegate so the main context is not bloated, and when an input is tackled it should open a new session stored in a temporary file.

## Decision
Add a `[[sensors]]` list to the config: `SensorCfg { name, command, interval_secs (default 300, floored to 10), mode (ask|auto, default ask), prompt (optional), enabled (default true) }` and a `SensorMode` enum (re-exported from comrade-core, added to the named-list merge so user+project `.comrade.toml` sensors merge by `name`). New module `crates/comrade-tui/src/proactive.rs`: one tokio task per ENABLED sensor polls `bash -c <command>` every `interval_secs`; the first successful stdout is only a baseline; a later run whose output differs is diffed with a pure `line_delta` (trimmed, non-empty lines, order-preserving) and emits `SensorEvent::Changed { name, mode, prompt, delta, raw }` (a whitespace/blank-line-only change moves the baseline without emitting); a non-zero exit emits `SensorEvent::Error`. `SensorRuntime::start(&[SensorCfg]) -> (SensorRuntime, UnboundedReceiver<SensorEvent>)`; the App holds the runtime (aborted on Drop) plus a `sender()` so the channel never closes. The TUI starts the runtime in `tui::run` and drains `sensor_rx` in the main `select!`: on Changed it posts a notification row (a meta summary plus the added/removed lines); `mode=auto` opens a new session titled `sensor: <name>`, sets `session_file` to a `std::env::temp_dir()` path, writes the initial session snapshot there, seeds the prompt with the change and calls `start_run`; `mode=ask` opens a `UserPrompt::Confirm` dialog (reusing the Dialog/oneshot machinery) whose `diff` shows a bounded preview of the raw output, and only on "yes" does a background task feed a `SensorEvent::Confirmed` back through the same channel to open the session.

## Rationale
Diffing a shell command's stdout is source-agnostic: the same mechanism watches `gh issue list`, a JIRA CLI, `curl`, or a file, with no per-source code. One tokio task per sensor isolates each sensor's interval and failure mode with no central scheduler bookkeeping. Reusing the existing Dialog/oneshot confirm path means the ask mode needs no new UI. Backing the new session with a temp file matches the request and reuses `session_store`.

## Alternatives considered
(a) A single tick in the App's event loop that polls all sensors vs one task per sensor — rejected: per-sensor tasks keep independent intervals and cannot stall each other. (b) Structured JIRA/GitHub API clients vs generic command diffing — rejected: an SDK per source is far more code and coupling than a command whose stdout is diffed. (c) A new bespoke modal vs reusing `UserPrompt::Confirm`/`Dialog` — rejected: the confirm dialog already handles yes/no and a diff body. (d) Persisting sessions to a fixed .comrade/sessions/ dir vs `std::env::temp_dir()` — the request explicitly asked for a temporary file. (e) Emitting on any raw difference vs on a non-empty line delta — rejected: whitespace churn would spam notifications.

## Scope
Covers the `[[sensors]]` schema and parsing/merge, the `proactive` module (polling + change detection), the TUI wiring (notification, ask/auto, session-on-input), and the README docs. It is NOT a general event bus: sensors are poll-only shell commands with a single interval, no structured parsing, no retry/backoff beyond the interval, and no persistence of "seen" changes across restarts (the baseline is in-memory for the life of the TUI session). It does not change the headless runner.

## Impact
New module `crates/comrade-tui/src/proactive.rs`; `Config` gains a `sensors` field; `SensorCfg`/`SensorMode` are part of comrade-core's public API; README gains a "Proactive mode (sensors)" section. A sensor's `command` runs arbitrary shell via `bash -c` on a timer, so a repo-supplied `.comrade.toml` sensor is a trust boundary like `[hooks]` — a future guard could require confirmation before honouring repo-supplied sensors. `auto` mode starts a run without asking; its autonomy is governed by the sensor's own `mode`, independent of `[security].autonomy`. Follow-ups: per-sensor "seen" state if persistence across restarts is wanted; richer structured sources (e.g. a JSON-mode diff).


## Note
Superseded in part by ADR 0054: a received change is no longer acted on inline. The ask-mode Confirm dialog was removed in favour of a sensors queue panel (M-x sensors-priority-up/down, sensors-discard, sensors-start), and a sensor may now poll a registered tool (MCP/skill/built-in) as well as a `bash -c` command.

## Merged: Tool-capable sensors and a sensors queue panel
## Context
ADR 0052 added `[[sensors]]` that poll a `bash -c` command and, on an ask change, opened a Confirm dialog; on an auto change, opened a session immediately. Two follow-ups were requested: (1) a sensors queue panel under the model panel showing the requests received, with M-x commands to change their priority or discard them (like the plan); (2) sensors should be able to poll not only a shell command but any registered tool — an MCP tool (e.g. listing a sprint's JIRA tickets), a skill, or a built-in — instead of a shell.

## Decision
Sensors gain two capabilities. (A) A sensor polls either a shell `command` (as before) or a registered `tool` with optional JSON `args` (`tool` wins when both are set). `SensorRuntime::start(&[SensorCfg], Arc<ToolRegistry>, ToolContext)` builds, per sensor, a probe behind a small `Probe` trait — `CommandProbe` (bash) or `ToolProbe` (invoke `tools.get(name)` with `args` and watch the returned string). The unified result then flows through the unchanged `line_delta` change detection, so source-agnostic diffing is preserved. (B) Received changes are no longer acted on inline; `SensorEvent::Changed` now ENQUEUES a `SensorEntry` on `App.sensor_queue` (oldest/highest-priority first). The `ask`-mode Confirm dialog is removed — an ask request simply waits in the queue. A new ' sensors ' panel is rendered in the right column between the model panel and the plan (`[stats_h, sensors_h, Min(0), jobs_h]`), highlighting the selected row. M-x commands manage it: `sensors-next`/`sensors-previous` (selection), `sensors-priority-up`/`sensors-priority-down` (reorder), `sensors-discard` (drop), `sensors-start` (tackle now). `auto` entries are pumped automatically once the app is idle (`App::pump_sensor_queue`, called each loop iteration, guarded by `self.running`); starting an entry removes it and opens the temp-file session as before. Sensor tool calls run with a context derived from `app.ctx_base` but `auto_approve = true`, `events = NoopEvents`, and `steer`/`compact`/`stop` cleared, so polling is unattended and never spams the chat.

## Rationale
Behind a `Probe` trait, the polling loop and change detection stay identical whatever the source, and the loop stays unit-testable with a fake probe. Invoking the same `ToolRegistry` the agent uses means MCP/skill/builtin sources work with no new code. A queue is the natural model for 'requests received' and gives the human one place to prioritise, discard or start — reusing the plan panel's idioms.

## Alternatives considered
(a) Keep sensors bash-only and require users to wrap MCP calls in a shell command — rejected: the harness already registers MCP/skill tools, and shelling out to an MCP client duplicates the connection. (b) Implement a bespoke JIRA/GitHub client — rejected: source-specific code the generic tool-invocation path already covers. (c) Keep the per-change ask Confirm dialog and add the queue alongside — rejected: two competing surfaces; the queue subsumes the dialog. (d) A per-sensor 'seen' model with the priority on the sensor rather than the request — rejected: the user wants to prioritise/discard individual received requests. (e) Give sensor tool calls the active session's event stream so calls show in the chat — rejected: polling would spam the transcript; sensor invocations use NoopEvents.

## Scope
The sensor poll sources (command or tool), the `proactive` runtime, and the TUI sensors queue (panel, selection, M-x priority/discard/start, auto-pump). It does NOT add structured parsing of tool results (the string is still diffed line by line), per-sensor persistence, or a headless-runner queue. It supersedes ADR 0052's ask-mode dialog.

## Impact
New public surface: `SensorCfg.tool` / `SensorCfg.args`, `SensorCfg::tool_name` / `is_pollable`. Sensors can now drive real integrations through MCP. A sensors queue panel and six new M-x commands; `ask` no longer pops a dialog (the queue replaces it). The SensorRuntime signature changed. A tool-based sensor runs whatever tool it names, unattended, on a timer — the same trust boundary as `[hooks]`/sensor commands; a repo `.comrade.toml` can declare one. Follow-ups: keyboard bindings for the queue commands; persisting the queue; per-entry 'seen' state.

## Merged: Drop sensor-opened sessions when their run finishes
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

