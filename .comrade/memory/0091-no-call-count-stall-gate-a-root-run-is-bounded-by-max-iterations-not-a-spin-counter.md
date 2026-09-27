# 0091 - No call-count stall gate: a root run is bounded by max_iterations, not a spin counter
status: accepted
date: 2026-09-27
tags: agent-loop, guards, jev, steering, comrade-core
summary: The root run is no longer stopped by a non-progress call counter (STALL_END_AT/stall_reason removed); the stall at 20 calls stays as advice only, and a run is bounded by max_iterations — note the root guardrail is advisory-only, so the root now has no automatic spin stop.

## Context
`LoopTracker` stopped the root run with a `FinalAnswer` once 40 consecutive non-progress calls followed the first workspace change (`STALL_END_AT = READ_GUARD_THRESHOLD * 2`, `stall_reason()`), justified as "end the run gracefully instead of burning the budget". The count is per CALL, not per turn, so a productive run that issues parallel tool calls and spends a long stretch verifying (tests, checks, reads) accumulates non-progress calls while still working, and got cut off mid-task. The human asked to remove it now that the guardrail (Jev, ADR 0079) judges whether a run is genuinely stuck.

## Decision
The harness MUST NOT end a root run on a call count. `STALL_END_AT`, `LoopTracker::stall_reason()` and the loop block that turned it into a `FinalAnswer` (crates/comrade-core/src/agent.rs) are REMOVED. The one-shot stall nudge at `STALL_NUDGE_AT = READ_GUARD_THRESHOLD` (20 non-progress calls) is KEPT and remains ADVICE only. A root run is now bounded by `[agent].max_iterations` (default 30) and, when configured, `[agent].run_timeout_secs`.

## Rationale
The counter measured calls, not stuckness, so it could not tell a long verification stretch from a spin — and with parallel tool calls a productive agent reaches 40 non-progress calls without looping. Judging stuckness belongs to the guardrail, which reads the conversation and answers typed diagnostics (on_task/looping/blocked/making_progress) instead of counting; keeping a counter alongside it means two mechanisms deciding the same thing, and the dumber one has the final say.

## Alternatives considered
(a) Keep the counter and raise the threshold — rejected: it is still a call count, so it still cuts off a legitimately long verification stretch, only later; (b) keep the stop but only when no guardrail is configured — rejected as the worst of both: the mis-fire stays exactly where the guardrail cannot compensate; (c) lower the nudge threshold so the model is told earlier — rejected: the nudge already fires at 20, well before 40, and was ignored in the cases that hurt.

## Scope
Covers the root agent loop's call-count stall gate and the tests around it. Does NOT cover the delegate inactivity gate, the read guard, the repeated-call refusal (`stuck_reason`), the stall/verify nudges (which stay), or making the guardrail enforcing.

## Impact
Removing the gate changes the failure mode for a genuinely spinning root agent: instead of the graceful `FinalAnswer` "Stopped: N non-progress calls in a row … the work looks complete", the run now reaches `bail!(\"reached max_iterations ({n}) without a final answer\")`, i.e. an `AgentEvent::Error`. NOTE the premise asymmetry, so nobody assumes this was covered: per ADR 0079 the ROOT guardrail is ADVISORY ONLY (\`root_guard\` never refuses a tool or stops the run — only a DELEGATE can be Stopped/Looped by Jev), so after this change the root agent has NO automatic spin stop. If one is wanted, wire the guardrail's Loop/Stop verdict into `root_guard`; do NOT re-introduce a call counter. Untouched by this decision, and easy to confuse with it: the delegate's TIME-based inactivity gate (idle nudge 300s / stop 600s, ADR 0068), the read guard (`READ_GUARD_THRESHOLD` = 20, which still refuses a read when no guardrail is configured), and `stuck_reason()`/`MAX_LOOP_REFUSALS` (3 repeats of an identical call with no state change), which all remain. Tests: `stall_tests` now covers the nudge only (`the_stall_nudge_fires_once_at_the_threshold`, `a_progress_call_resets_the_idle_run`, `test_runs_count_as_non_progress`, `reads_before_any_change_never_arm_the_guard`) and `stall_reason_only_after_the_end_threshold` was deleted. Verified with `cargo test --workspace` (comrade-core 265, comrade-tui 203, 0 failures) and `cargo clippy --all-targets`.

