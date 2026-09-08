# 0032 - Delegate sub-agents now share the agent's read guard (20 reads -> nudge)
status: accepted
tags: delegate, guardrail, read-guard, subagent, agent
summary: Delegate sub-agent loop now refuses reads after 20 consecutive read-only calls and nudges to implement, mirroring the main agent; plan/commit monitors intentionally not ported (tools denied for delegates).

## Context
User asked to implement the main agent's "20 consecutive read calls -> refuse and nudge the model to implement" guardrail for delegate sub-agents, and "all guardrails we have". Survey of agent.rs guardrails vs delegate.rs (run_delegate_subagent): LoopTracker identical-call refusal already ported (decision #21); the plan nudge and verify-then-commit monitor do NOT apply to delegates because set_plan/update_plan/git_commit are in DENIED_FOR_DELEGATES. So the only portable guardrail was the read guard.

## Decision
1. agent.rs: made `allow_read_step` pub(crate) (kept is_read_only/read_guard_message private; READ_GUARD_THRESHOLD stays 20). 2. delegate.rs: added `refuse_reading(name, &mut consecutive_reads) -> Option<String>` helper right after refuse_repeat; wired into BOTH dispatch paths of run_delegate_subagent (native tool_calls loop at ~line 667, react text path at ~line 730), BEFORE refuse_repeat, with a `let mut consecutive_reads = 0usize` state near the LoopTracker. Refused native calls answered with ChatMessage::tool_result(tc.id, msg); react with a pushed render_observation User msg. 3. Nudge wording deliberately differs from the agent's read_guard_message: no `update_plan` mention (delegates cannot plan), instead "implement now (write or edit a file) and verify your work, then reply with your final answer."

## Consequences
Tests: delegate.rs tests mod has 4 new tests — 2 pure unit (refuse_reading semantics: 21st read refused, message says implement now + not update_plan, action resets counter) and 2 e2e via scripted_server with 21 distinct read_file turns + final (native and React), asserting the stub read tool is reached exactly 20 times. Verify with: cargo test -p comrade-core -- --test-threads=1 (100 pass). Note the ReadStubTool + STUB_READ_SPEC test doubles added next to STUB_WRITE_SPEC. Committed as d0fce68.

