# 0054 - Tool-capable sensors and a sensors queue panel
status: accepted
date: 2026-09-14
tags: proactive, sensors, mcp, tui, config
summary: Sensors can now poll any registered tool (MCP/skill/built-in) as well as a shell command, and every received change lands in a ' sensors ' queue panel that M-x commands reorder, discard or start (`auto` entries pump when idle).

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

