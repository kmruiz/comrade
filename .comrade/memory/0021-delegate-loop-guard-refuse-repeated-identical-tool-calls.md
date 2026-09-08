# 0021 - Delegate loop guard: refuse repeated identical tool calls
status: accepted
tags: delegate, loop, subagent, agent, bugfix
summary: run_delegate_subagent lacked the main loop's LoopTracker; stuck delegates spun to max_iterations on one identical call. Fixed by reusing agent::LoopTracker.

## Context
User reported delegates "can't do work / stop / loop forever on the same argument". Root cause: the main agent loop (crates/comrade-core/src/agent.rs run_agent_loop) detects no-progress loops with LoopTracker (refuse identical call repeated with no state change; abort after MAX_LOOP_REFUSALS=3), but the delegate sub-agent loop (crates/comrade-core/src/delegate.rs run_delegate_subagent) had NO equivalent guard, so a delegate that kept repeating one failing call (e.g. a tool error it could not resolve, or weak native tool calling) looped until [agent] max_iterations — the user's config had 10000.

## Decision
1. In agent.rs mark LoopTracker (struct + check/record/mark_stuck/stuck_reason), MAX_LOOP_REFUSALS, LOOP_WINDOW, is_mutating and loop_refusal pub(crate) so the sibling delegate module can reuse them. No logic changed. 2. In delegate.rs add helper refuse_repeat(tracker, sig) -> Result<Option<String>> (checks tracker.check; count >= MAX_LOOP_REFUSALS bails with "delegate repeated identical action `{sig}` {count}x..."; otherwise returns the loop_refusal text) and wire it into BOTH dispatch paths of run_delegate_subagent, building sig = "{name} {canonical-args-json}" like agent.rs does, calling tracker.record(name, sig) after each real invocation. Native path answers refused calls with ChatMessage::tool_result(tc.id, refusal) to keep the conversation API-valid (aborting after MAX_LOOP_REFUSALS is fine — no further request is made). React path pushes a User render_observation. 3. Tests: repeated_native_tool_call_is_refused_then_aborts, repeated_react_tool_call_is_refused_then_aborts (4 identical scripted responses: 1 executes, 2 refused, bail on the 4th), identical_call_after_a_mutation_is_not_a_loop (a->b->a all execute, no false positive). Sequence detail: LoopTracker records a mutating call at the NEW mutation_seq, so the first identical repeat is detected on the NEXT identical call.

## Consequences
Verify with: cargo test -p comrade-core delegate::tests -- --test-threads=1 (19 pass) and the whole workspace suite. A delegate hitting the guard now fails the delegation with an explicit error (invoke restores the plan step to its previous status), rather than returning a fake final answer. The refusal message lets a smarter model change tack on the first 2 repeats; only the 3rd identical repeat aborts.

