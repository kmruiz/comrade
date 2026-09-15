# 0060 - Spawned task commands never inherit stdin
status: accepted
date: 2026-09-15
tags: tasks, bugfix, hang, small-model
summary: Commands spawned by the project task runner get stdin = /dev/null so a stdin-reading command (a stray `cat`) cannot freeze the runner past its timeout.

## Context
A smoke run hung for the full 900s external timeout (exit 124). The delegate had emitted `shell {"command":"cat"}` (and other stdin-reading commands); the child inherited the runner's stdin, blocked forever reading it, and the per-task timeout never fired because it only watches a child that is running, not one parked on stdin.

## Decision
Every command the project task runner spawns (crates/comrade-tool-project/src/tasks.rs::exec, used by pom_run_task / pom_run_tests / pom_check / shell / run_bg) gets `Stdio::null()` on stdin, so a command that reads stdin sees EOF immediately instead of blocking. TaskOutput gained #[derive(Debug)] so a failure can be inspected.

## Rationale
The agent runs unattended; nothing can ever answer a prompt on stdin, so inheriting it can only ever block. EOF is the correct, terminating behaviour.

## Alternatives considered
(1) Rely on the timeout alone - rejected: measured, the timeout never fired because the child that read stdin blocked the whole runner. (2) Give the request channel a timeout - rejected: same, the blocking child never returns control to be timed out.

## Scope
All spawned project/shell tasks. Not the TUI's own stdin handling.

## Impact
A stray `cat`/`read`/interactive tool now returns instantly instead of freezing the whole session. Pinned by `mod hang_tests`: `a_command_reading_stdin_does_not_freeze_the_runner` and `a_hanging_command_is_killed_by_the_timeout`.

