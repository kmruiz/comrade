# 0024 - Background jobs: detached processes with a shared BgHub in comrade-tool-project
status: accepted
date: 2026-09-13
tags: tools, process, concurrency
summary: Add background-process tools (`run_bg`/`bg_status`/`bg_tail`/`bg_kill`) in comrade-tool-project: detached `bash -c` jobs with bounded captured output, killable via a CancellationToken, approval-gated on start and denied to delegates.

## Context
Every tool call was synchronous: a slow `pom_run_tests`/`cargo build` blocked the whole turn. The agent needed to start long-running commands and keep working.

## Decision
Add `run_bg`, `bg_status`, `bg_tail`, `bg_kill` to `comrade-tool-project` (`crates/comrade-tool-project/src/bg.rs`). `BgHub` (jobs map + counter) is created once per `all()` call and shared by the four tools via `Arc`. A job spawns `bash -c <command>` in the project root with piped stdio and `kill_on_drop(true)`; two reader tasks append stdout/stderr to a bounded buffer (200 KB, trimmed from the front on a char boundary), and a monitor task awaits `child.wait()` or a per-job `CancellationToken` (on cancel it `start_kill`s then reaps). `run_bg` is approval-gated (`APPROVAL_GATED_TOOLS`) and calls `ctx.confirm`; `run_bg`/`bg_kill` are mutating for the loop tracker; all four are denied to delegates.

## Rationale
Detached tokio tasks with a CancellationToken are the idiomatic way to own a child process and still be able to kill it; keeping the tools in comrade-tool-project avoids new crate wiring while the `Arc<BgHub>` on the tool structs gives the four tools shared state.

## Alternatives considered
A separate `comrade-tool-bg` crate was rejected to avoid new workspace wiring; the `shell` tool with `&` was rejected because output could not be polled. Holding the child in a shared Mutex (blocking kill) was rejected in favour of a `CancellationToken` + monitor task.

## Scope
Covers starting, polling and killing detached shell jobs. Does not add streaming of job output into the chat as it arrives, nor persistence of jobs across sessions.

## Impact
Long jobs survive across turns and die with the app (kill_on_drop). Main/delegate/advise registries each own an independent BgHub, so jobs started by one are not visible to another. Index dirs and job state are in-memory only (no persistence across restarts).

