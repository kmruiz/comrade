# 0043 - TUI: background-jobs panel + M-x stop-background-job via a BgJobs handle
status: accepted
date: 2026-09-13
tags: tui, background-jobs, tools
summary: Expose the background-job registry via a `BgJobs` handle and add a TUI jobs panel under the plan plus M-x stop-background-job.

## Context
Background jobs (`run_bg`/`bg_status`/`bg_tail`/`bg_kill`) existed only as model-facing tools in comrade-tool-project, with a private `BgHub` created inside `bg::tools()` and shared by those four tools. Nothing outside the tools could see or stop a job. The human asked for a small section under the plan listing the running background jobs, plus an M-x `stop-background-job` that stops one picked from a list.

## Decision
Expose the shared job registry to observers through a cheap, cloneable `comrade_tool_project::BgJobs` handle (methods `list`, `running`, `kill`, returning a `BgJobInfo` snapshot), and return it from a new `comrade_tool_project::all_with_jobs()` (plain `all()` still returns just the tools, for the delegate registry). The TUI carries the handle on `Deps`/`App` and: (1) draws a bordered "background jobs" panel BELOW the plan, sized to the running jobs and hidden when none run (up to 4 rows, then "+N more"); (2) offers M-x `stop-background-job` (palette-only), which opens an overlay picker snapshotting the running jobs; Enter stops the selected job via `BgJobs::kill`, esc/ctrl-g cancels.

## Rationale
A shared handle keeps a single source of truth (the same hub the tools mutate) and stays cheap to clone/read per frame. Doing it in comrade-tool-project (not a global) keeps the security/registry model intact and keeps tests isolated (each test builds its own handle).

## Alternatives considered
(a) A process-wide global bg registry (like SecurityPolicy) - rejected: global mutable state is awkward for tests and the tools already share a per-registry hub. (b) Have the TUI call the bg tools (bg_status/bg_kill) to list/kill - rejected: the TUI has no model-facing tool-call path, and reading a Tool's output string is fragile. (c) Put the jobs panel in the left column with the chat - rejected: the human asked for it below the plan.

## Scope
Covers observing and stopping background jobs from the TUI. Does NOT add new bg tools, change how jobs are started/captured, stream job output in the panel, or list finished jobs.

## Impact
The human can watch and stop background jobs without the model. Small scope: a handle type, one extra TUI panel, one M-x command + overlay. Follow-ups: show recently finished jobs (with status) for a moment; a keybinding for the picker; tail a selected job's output in the panel. The panel resizes the plan area (rows shrink when jobs run).

