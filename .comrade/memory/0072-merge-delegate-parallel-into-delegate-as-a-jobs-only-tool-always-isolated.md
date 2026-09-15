# 0072 - Merge delegate_parallel into `delegate` as a jobs-only tool, always isolated
status: accepted
date: 2026-09-15
tags: delegate, worktree, git, tools, prompt
summary: The separate `delegate_parallel` tool is gone: `delegate` now takes either `step` (one plan step) or `jobs` (1..=8 ad-hoc tasks), the ad-hoc `model`/`task` shorthand is dropped, and jobs are ALWAYS isolated in a git worktree with no `isolate` flag.

## Context
ADR #0023 added `delegate_parallel` beside `delegate` with an opt-in `isolate`, and #0070 flipped isolation to default-on (opt out with `isolate=false`). Two tools for one concept meant the lead had to know which to reach for, the tool surface advertised a redundant entry, and the `isolate=false` escape hatch existed purely so a single task could avoid a worktree. The user asked for one merged tool, parallel by default, with the isolation flag removed.

A design fork was put to the user: keep a top-level `model`+`task` for a single un-isolated task alongside `jobs`, or make `jobs` the only ad-hoc shape. The user chose jobs-only.

DELEGATE AVAILABILITY NOTE: the only configured delegate (mistralai/ministral-3-3b) immediately overflows its context window in this repo (system prompt + tool specs alone exceed it; 0 compactions), so it returned no edits three times in a row and the change was completed by the lead model. This is an environment limitation, not a property of the change.

## Decision
`delegate` is the single tool. Its args are `step` (+ optional `model`, `feedback`) XOR `jobs`; passing both bails. `jobs` is an array of 1..=8 `{model, task, context?}` objects. Top-level `model`/`task`/`context` for ad-hoc work are REMOVED - a single ad-hoc task is a one-element `jobs` array. Every job is ALWAYS isolated in its own git worktree (`crates/comrade-core/src/delegate/parallel.rs`, `run_jobs`); there is no `isolate` field in the Rust `Job` struct nor in the JSON schema. A non-git project still falls back to the shared workspace with a notice (the one case where "always isolated" cannot hold). `DelegateParallelTool`, `TOOL_NAME_PARALLEL` and `pub use parallel::*` are deleted; `DENIED_FOR_DELEGATES`, `MUTATING_TOOLS` and `PROGRESS_TOOLS` keep only `delegate`. If EVERY job in a batch fails, `run_jobs` returns `Err` (so the single-job case behaves like the old `delegate`); a partial batch returns `Ok` with per-job `FAILED` lines.

## Rationale
One tool with one schema is simpler for the model to choose and keeps the approval gate, worktree merge recipe and job reporting in a single code path. Dropping the singular shorthand removes a second call shape the lead would have to learn, and removing `isolate` removes a knob whose only use was defeating the safety the tool exists to provide - jobs that share a workspace clobber each other, which was the original motivation for isolation. Failing the whole batch when nothing succeeded preserves the old single-delegate error contract for the common case without forcing a multi-job batch to abort on one bad job.

## Alternatives considered
Keep `delegate_parallel` as a separate tool (rejected: the duplicate surface is exactly what the user asked to merge). Keep `model`+`task` as a single un-isolated task (rejected by the user: one ad-hoc shape only, always isolated). Accept `isolate` as an ignored/legacy field (rejected: dead config that silently misleads). Return `Err` whenever ANY job fails (rejected: a batch of independent jobs should not be aborted by one failure). Auto-merge worktrees inside the tool (still rejected, per #0070: conflicts need the lead's judgement).

## Scope
The `delegate` tool's argument surface, the jobs runner and its isolation behaviour, the tool registration/deny lists, the tech-lead prompt wording, and the delegate tests. Does NOT change plan-step semantics (fix rounds, `working:` notes, approval policy), the worktree implementation itself, `ask_advise`, or the native loop's multi-`delegate`-call batch parallelism (agent.rs still runs a batch that is entirely `delegate` calls concurrently).

## Impact
Anything that emitted `delegate` with `model`/`task`, or called `delegate_parallel`, must switch to `jobs`. Delegation is strictly more isolated than before: there is no way to opt out, so trivial single-file edits now incur a worktree that the lead must merge back (`git -C <wt> add -A && git -C <wt> diff --cached --binary | git -C <repo> apply --3way`, then `git worktree remove --force <wt>`). Supersedes the tool-surface parts of #0023/#0038/#0070; the isolation-by-default behaviour of #0070 is retained but is no longer optional. Tests: `parallel_delegates_run_every_job_and_merge_replies`, `isolated_parallel_job_runs_in_its_own_worktree`, `parallel_jobs_isolate_by_default`, `parallel_jobs_share_when_not_a_git_repo`, `parallel_delegates_reject_unknown_model_and_empty_jobs`, `no_delegates_yields_no_delegate_tool`, and `agent::tests::parallel_delegate_calls_in_one_turn_run_concurrently` (re-scripted to emit `jobs` arguments). Verified with `cargo test --workspace`.

