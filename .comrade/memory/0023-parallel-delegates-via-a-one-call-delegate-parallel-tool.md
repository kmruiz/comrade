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

