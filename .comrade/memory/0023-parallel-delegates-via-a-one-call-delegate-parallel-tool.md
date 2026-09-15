# 0023 - Parallel delegates via a one-call `delegate_parallel` tool
status: accepted
date: 2026-09-13
tags: delegates, concurrency, tools
summary: Add a `delegate_parallel` tool that runs up to 8 independent delegate jobs concurrently in one call (works under native and ReAct), gating each job's approval up front; delegates may not call it.

## Context
The native agent loop already ran a batch that is entirely `delegate` calls concurrently (agent.rs `parallel_delegates` + `join_all`), but the ReAct protocol issues one action per turn, so it could never fan delegates out, and the lead had to know to emit several calls in one message.

## Decision
Add a separate `delegate_parallel` tool (delegate.rs, `DelegateParallelTool`, `TOOL_NAME_PARALLEL`) taking `jobs: [{model, task, context}]` (1..=8). It validates every job's model, enforces each delegate's `approval` policy UP FRONT (one denial aborts before anything runs), then runs all jobs with `futures_util::future::join_all` and returns every reply labelled by job/model. It never touches the plan (plan steps stay single-model via `delegate` `step`). It is registered in `build_tools` (comrade-tui), treated as mutating in the loop tracker, and added to `DENIED_FOR_DELEGATES` so a delegate cannot fan out recursively. The existing native pure-delegate batch parallelism is left as-is.

## Rationale
A dedicated tool gives one-call fan-out that works regardless of protocol, keeps the existing `delegate`/`step` semantics untouched, and puts the concurrency limit and approval handling in one place.

## Alternatives considered
Extending the `delegate` schema with a `jobs` array was rejected: the tool already has a `step` mode, and the loop pre-flight (approval gating, step note handling) would have to special-case batches. Relying solely on the existing multi-tool-call parallelism was rejected because ReAct issues one action per turn.

## Scope
Ad-hoc parallel delegation only. Plan-step delegation, fix rounds and per-step `working:` notes remain the single-target `delegate` tool's job.

## Impact
The lead can fan out reviews/checks in one call under both protocols; jobs share the repo, so the description warns against pointing two jobs at the same files. Adds one tool to the surface (and one entry to the deny list).


## Merged from #0038 - Isolate delegate_parallel jobs in git worktrees
status: accepted
date: 2026-09-13
tags: delegate, worktree, git, roadmap
summary: delegate_parallel jobs can set isolate=true to run in their own detached git worktree (<repo>/.comrade/worktrees/<id>), keeping overlapping edits safe; changed worktrees are kept for review, unchanged ones removed.

## Context
`delegate_parallel` runs several delegate sub-agents at once but they share the workspace, so the tool could only warn the parent not to point two jobs at the same files. The roadmap asked for git worktree isolation (D2) to make parallel edits safe.

## Decision
Add `comrade_core::Worktree` (create/remove/has_changes) that runs `git worktree add --detach <repo>/.comrade/worktrees/<id> HEAD`. Each `delegate_parallel` job gains an optional `isolate: bool`; when set and the project is a git repo, the job runs with a cloned ToolContext whose project_root/cwd (and system prompt root) point at a fresh worktree. After the batch, a job that made changes keeps its worktree and its path is reported for review (`git -C <path> diff`); a job with no changes has its worktree removed. Non-git projects are refused with a clear error.

## Rationale
Worktrees share the object store (cheap) while giving each job an independent working tree, and leaving the result in place lets a human/agent review the diff before merging.

## Alternatives considered
Serialize writes with a mutex/keyed lock — rejected: does not stop two jobs editing the same files, only interleaving. Copy the whole tree per job — rejected: slow and loses git history. Auto-merge/auto-commit the worktree result — rejected for now: merging is the parent agent's call, so we leave the worktree for review.

## Scope
delegate_parallel job isolation only.

## Impact
Parallel delegates can safely edit overlapping files when isolated. New schema field `isolate` on delegate_parallel jobs. Worktrees live under `.comrade/worktrees/<id>/` (should be git-ignored). Follow-up: an explicit merge/apply step and cleaning stale worktrees.

## Note
Rollup of parallel delegation: this ADR adds delegate_parallel (up to 8 concurrent jobs); #0038 adds isolate=true so jobs run in detached git worktrees. Body preserved under "Merged from".

## Note
Amended by the new ADR on isolating by default: the `isolate` default is now true (opt out with isolate=false), non-git projects fall back to the shared workspace instead of erroring, and the deferred "explicit merge/apply step" is now mandatory in the tech-lead prompt (`git apply --3way`).
