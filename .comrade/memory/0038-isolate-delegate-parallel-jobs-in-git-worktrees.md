# 0038 - Isolate delegate_parallel jobs in git worktrees
status: superseded
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
Parallel delegates can safely edit overlapping files when isolated. New schema field `isolate` on delegate_parallel jobs. Worktrees live under `.comrade/worktrees/` (should be git-ignored). Follow-up: an explicit merge/apply step and cleaning stale worktrees.


## Note
merged into #0023
