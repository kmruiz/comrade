# 0032 - Remove the verify-then-commit gate on git_commit
status: superseded
date: 2026-09-13
tags: agent-loop, git, guard
summary: Removed the agent-loop gate that refused `git_commit` until the last change was backed by a green test run, since it misfired on formatter-originated edits; the prompts still advise verifying before committing.

## Context
The agent loop had a 'verify-then-commit monitor': it set a `verified_after_change` flag to false on any code-changing tool (fs_edit, fs_write_file, ts_rename, pom_format_code, shell, delegate) and only back to true on a `pom_run_tests`/`pom_run_task` output containing `test result: ok.`, then hard-refused any `git_commit` while the flag was false (both the ReAct and native dispatch paths, in crates/comrade-core/src/agent.rs). In practice it misfired: a formatter-originated edit (pom_format_code) or any output whose green signal did not match the exact string left the flag false and blocked commits the user considered legitimate.

## Decision
Remove the gate entirely from the agent loop (crates/comrade-core/src/agent.rs): the `git_commit && !verified_after_change` refusals on both dispatch paths, the `verified_after_change` state threaded through `run_calls`/`run_native_calls`, and the `CODE_CHANGES` / `update_verify_state` / `verify_guard_message` helpers with their tests. `git_commit` no longer inspects whether tests were run.

## Rationale
The gate was a heuristic over tool names and output strings that produced false positives (notably on formatter-driven edits) without reliably catching unverified commits; the prompts already carry the verification guidance, so the hard refusal added friction for no dependable safety.

## Alternatives considered
(a) Keep the gate but also treat `pom_format_code` (and other whitespace-only actions) as non-invalidating — rejected: whack-a-mole on a heuristic, and it was still wrong about which outputs count as green. (b) Keep the gate only for non-formatting changes — rejected: extra complexity for little value. (c) Make the gate advisory (a hint) instead of a hard refusal — possible, but the working-style/protocol prompts already instruct running tests to green before committing, so the reminder is redundant.

## Scope
Covers the verify-then-commit enforcement inside the agent loop only. Does NOT change the `git_commit` tool itself, the approval gating of other tools, the read guard, or the loop tracker.

## Impact
The model can commit without a preceding green test run in the same turn; it must judge for itself when the suite is green. The false refusals disappear. The guidance to verify before committing remains in the prompts (crates/comrade-core/prompts/*), so the behaviour is unchanged when the model follows its instructions. `git_commit`'s own argument validation (git_diff/git_status advice, path scoping) is untouched.


## Note
merged into #0047
