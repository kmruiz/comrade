# 0092 - A completed plan is cleared at the end of the run so the next task must plan afresh
status: accepted
date: 2026-09-27
tags: plan, agent-loop, session, comrade-core
summary: A root run that ends with every plan step Done clears the session's plan (and its delegation records and finished summary) on every exit path, so the next run is forced to plan afresh; a plan with any unfinished or Blocked step is kept.

## Context
A session keeps one plan across runs, and the agent loop's "plan first" guard only fires when the plan is EMPTY (`ctx.session.plan().is_empty()`, crates/comrade-core/src/agent.rs:~835). Because a finished plan was never retired, a session that had completed one task started its next task with the previous all-done checklist still in the panel, and the guard never forced a fresh plan — the user asked for the plan to be cleared so the next run must plan.

## Decision
At the end of every ROOT run, in `run_agent_with_history` (crates/comrade-core/src/agent.rs) right after `run_agent_loop` returns and on EVERY exit path, call `clear_completed_plan(&dyn SessionControl)`: when the session's plan is non-empty and every step is `PlanStatus::Done`, replace it with an empty plan (`set_plan(Vec::new())`, which also drops the delegation records and the finished summary) and emit `AgentEvent::Notice("plan complete: cleared, so the next task plans afresh")`. A plan with ANY unfinished step (Pending/Ready/InProgress) or a `Blocked` step is KEPT, because it still describes outstanding work and "blocked" is not success. An empty plan is a no-op. The clear happens BEFORE `AgentEvent::RunEnd`, so a prompt queued during the run starts its own run against an empty plan and is therefore forced to plan.

## Rationale
The plan is scaffolding for one task: once every step has succeeded it has no further use, and its only remaining effect is to suppress the "plan first" guard on the next task. Keying the reset off the plan's OWN state (all Done) rather than the run's outcome keeps the rule to one predicate that cannot mis-fire, and keeping any plan with outstanding or blocked work preserves the checklist that still means something.

## Alternatives considered
(a) Clear only after a clean finish (a run that produced a final answer) — rejected: the condition "every step Done" is already the safety property, and threading the exit status through every exit path is more code for no extra safety; (b) keep the plan and rely on the prompt to make the model replan — rejected: that is the status quo that left a stale finished checklist in the session; (c) clear it in the TUI on RunEnd — rejected: a UI-only rule would leave headless runs different, and the core already has a single seam that runs after the loop on every exit path; (d) clear the steps into a single "done" placeholder row — rejected as cosmetic noise; (e) clear whenever no step is InProgress — rejected: that would throw away a genuinely outstanding Pending or Blocked step.

## Scope
Covers the root run's plan lifecycle and the notice that announces it. Does NOT change `self_set_plan`/`self_finish_plan`, the plan UI/rendering, the "plan first" guard itself, or delegate sub-agent plans (a delegate works the session's plan but never owns it).

## Impact
The plan is per-TASK scaffolding, not a session record: after a successful run the plan panel reads "(no plan yet)" and the session file carries no plan. Anything reading `session.plan()` after a run must handle the empty case (do not assume a completed plan survives to be displayed). The announcement is a Meta chat note, so it is HIDDEN in focus mode, exactly like the other harness notices (`loop guard: …`, `guardrail: …`); the plan panel emptying is the primary signal and a visible-in-focus note would have to hijack the model's voice, so this is accepted as-is. Tests: `agent::plan_reset_tests` (the predicate: cleared when all Done; kept for Pending/Ready/InProgress/Blocked; no-op when empty) and `agent::tests::a_finished_run_retires_a_completed_plan` / `a_finished_run_keeps_a_plan_with_work_outstanding` (the seam, driven by a scripted fake model, asserting both the cleared plan and the notice). Verified with `cargo test --workspace` (comrade-core 271, comrade-tui 203, 0 failures) and `cargo clippy --all-targets`.

