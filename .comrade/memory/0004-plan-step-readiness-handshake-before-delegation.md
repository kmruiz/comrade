# 0004 - Plan-step readiness handshake before delegation
status: superseded
date: 2026-09-09
tags: delegate, plan, ask_advise, readiness, plan-status
summary: ask_advise gained a step= readiness mode that marks a delegate-assigned plan step `ready` when its delegate confirms the context suffices; set_step_context lets the lead feed missing context back in.

## Context
Delegated agents sometimes lacked context because the tech-lead wrote the step context it thought important, not what the delegate actually needed. Implemented a readiness handshake: before a delegate-assigned plan step is picked up, the step's own delegate is consulted (read-only) about whether the step's goal/verification/context suffice.

## Decision
Extend the ask_advise tool with an optional `step` argument: with `step` = a plan-step id, the step's OWN delegate (model must match or be omitted) is consulted about context readiness. The advisor replies with a final verdict line; an explicit `VERDICT: READY` marks the step PlanStatus::Ready (a new state between pending and in_progress); `VERDICT: NEEDS_MORE: ...` leaves it pending with an "awaiting context: ..." note. The lead enriches the step via a new set_step_context tool (pending/ready/blocked only; replacing the context of a ready step drops it back to pending) and re-asks until ready. This is a SOFT gate: the delegate tool still runs a step from pending; `ready` is informational, encouraged by prompts (delegate-by-default.md, delegation-lead.md, advise-system.md, delegate tool description).

## Rationale
Reuses the existing ask_advise consult machinery (read-only advisor sub-agent, approval policy, parallel batching of multiple tool calls in one message) instead of a new parallel scheduler. The delegate of a single task is the right judge of its own context needs, and asking per-step keeps steps isolated. Marking ready only on an explicit VERDICT: READY keeps the auto-mark deterministic.

## Alternatives considered
(1) New dedicated tool prepare_step/ready_step with a hard gate refusing delegation of non-ready steps — rejected: user chose extending ask_advise and a soft gate. (2) State only + prompt guidance (lead uses plain ask_advise then update_plan) — rejected: no deterministic auto-mark. (3) Hard enforcement that delegate refuses non-ready steps — rejected by user; ready is informational.

## Scope
PlanStatus lifecycle and session tooling across comrade-tool (plan.rs), comrade-core (session.rs AgentSession, advise.rs AskAdviseTool, delegate.rs DENIED_FOR_DELEGATES), comrade-tool-session (set_step_context tool, update_plan/finish_plan open filters), comrade-tui (Ready glyph/color), lead/advisor prompts. Does not change the delegate tool's run mechanics or fix-round logic.

## Impact
Plan steps assigned to delegates now move pending -> ready -> in_progress; ready is visible in the TUI (blue ●). Reassigning a ready step's model or rewriting its context resets it to pending (readiness is stale). set_step_context and ask_advise step= are denied to delegate sub-agents. Future: could make the gate hard (refuse delegating non-ready steps) once flows mature.


## Note
merged into #0001
