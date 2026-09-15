# 0070 - Isolate parallel delegates in git worktrees by default and merge them when done
status: accepted
date: 2026-09-15
tags: delegate, worktree, git, prompt
summary: delegate_parallel jobs now isolate into a git worktree by default (isolate=false to share, non-git projects fall back to sharing), and the tech-lead prompt MUST merge each kept worktree back with `git apply --3way` before verify/commit.

## Context
ADR #0023/#0038 added delegate_parallel with an opt-in `isolate: bool` and deliberately left isolation opt-in and merging deferred ("an explicit merge/apply step"). In practice parallel delegate jobs shared the workspace and clobbered each other's files, and a tech-lead model was not reliably isolating or integrating worktrees. The follow-up left in #0038 was never done.

## Decision
delegate_parallel jobs isolate into their own git worktree BY DEFAULT: the Rust field uses `#[serde(default = "default_isolate")]` (true) and the JSON schema `isolate` default is true. A job may still pass `isolate = false` to share the workspace. Because isolation needs a git repo, parallel.rs detects this up front via `Worktree::isolation_available(&ctx.project_root)` and, when the project is not a git repo, runs jobs in the shared workspace with a notice instead of failing. The tech-lead prompt (crates/comrade-core/prompts/delegate-by-default.md) now states jobs isolate by default and that the lead MUST merge every kept worktree back before verifying or committing, using `git -C <wt> add -A && git -C <wt> diff --cached --binary | git -C <repo> apply --3way` then `git -C <repo> worktree remove --force <wt>`. The tool's output prints that recipe for each kept worktree.

## Rationale
Isolating by default makes concurrent parallel edits safe without the lead having to remember to opt in, and the shared object store keeps a worktree cheap. `git apply --3way` merges a worktree's uncommitted changes (including new/untracked files) back into the repo without creating commits or dangling objects, and fails safely on conflicts for the lead to resolve. Detecting non-git up front keeps parallel delegation working everywhere rather than erroring.

## Alternatives considered
Keep isolation opt-in (rejected: jobs clobbered each other). Auto-merge/auto-commit inside the tool (rejected: conflicts need the lead's judgement and merging is the lead's call). Commit in the worktree then `git cherry-pick -n` (rejected: leaves orphan commits and is noisier). Keep bailing on non-git projects (rejected: would break all parallel delegation outside git).

## Scope
The delegate_parallel isolation default, the non-git fallback, the tool's output hint and the tech-lead prompt's isolation/merge guidance. Does not change single `delegate`/plan-step delegation, and does not add an automatic merge to the tool.

## Impact
Parallel delegate jobs are isolated and therefore safe by default; the lead now carries a mandatory merge step before verify/commit, and non-git projects silently share the workspace (with a notice). Worktrees live under .comrade/worktrees/ (already git-ignored). Tests: parallel_jobs_isolate_by_default, parallel_jobs_share_when_not_a_git_repo, and the extended react.rs delegation-prompt test.

