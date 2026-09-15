# 0061 - ask_upwards: a capped escalation path from a delegate to its tech lead
status: accepted
date: 2026-09-15
tags: delegation, tools, small-model, session
summary: A stalled delegate can ask the main model one specific question via the ask_upwards tool (delegate-only, capped by MAX_UPWARD_ASKS), instead of stalling or recursing into delegation.

## Context
A small delegate derails when it hits a decision it cannot make (or the same error twice) and has no way to ask for help: the notes said "stalled" only as a last resort. The user asked for an escalation path - a stuck delegate asking the main model a question - while keeping the delegate unable to recurse into delegation.

## Decision
New tool `ask_upwards` (crates/comrade-tool-session, exposed by `upward_tools()`, backed by the `UpwardAsk`/`Upward` contract in crates/comrade-tool/src/ask.rs + SessionControl::upward()): a sub-agent asks its parent model ONE specific question (what it tried + the exact error) and gets the answer back as the tool result. It is registered ONLY in the delegate registry (crates/comrade-tui/src/main.rs); the main agent has no parent, so it never sees the tool. The delegate sub-agent loop caps it at MAX_UPWARD_ASKS and then calls refuse_upward (crates/comrade-core/src/delegate.rs), so a delegate cannot turn escalation into an infinite conversation. The main agent answers through the ParentAsk wired in comrade-tui/src/main.rs::session_bundle. The delegate prompt tells it to use ask_upwards at most twice, then decide itself.

## Rationale
Asking the model that already holds the task's context beats both stalling and giving the delegate more autonomy; capping the asks keeps the cost bounded and preserves the lead/worker split (only the lead commits and plans).

## Alternatives considered
(1) Let the delegate stall until its wall-clock budget expires (#53) - rejected: it returns a partial answer with no way for the lead to unblock it. (2) Give the delegate the full delegate tool so it can recurse - rejected: unbounded fan-out and it would still not reach the lead. (3) Pass the question back as a tool error/hint - rejected: the delegate needs an actual answer, and the lead must be able to answer it in the task's context.

## Scope
Delegate sub-agents only. The main agent and advisors (ask_advise) deliberately do not get it. Not a general chat: one question, one answer, at most MAX_UPWARD_ASKS per run.

## Impact
A stalled delegate can be unblocked instead of burning its budget, and the escalation is visible in the session. Cost: one more tool in the delegate registry, and a cap that must stay wired to the parent so a delegate never escalates into a loop.

