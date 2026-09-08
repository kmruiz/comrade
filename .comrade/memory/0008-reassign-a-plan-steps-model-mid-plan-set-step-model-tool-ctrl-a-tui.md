# 0008 - Reassign a plan step's model mid-plan: set_step_model tool + Ctrl-A TUI
status: accepted
tags: plan, delegate, session, set_step_model, tui, reassign
summary: Plan steps can now be reassigned to another model only while pending/blocked: SessionControl::reassign_step_model + agent tool set_step_model + TUI Ctrl-A overlay.

## Context
Before this change the only way to change which model runs a plan step after set_plan was to replace the whole plan (set_plan), which resets ids/statuses and the delegation record. The lead/human wanted to hand one pending step to another delegate (or take one back to \"self\") without touching the rest. Guard required: only when the step is NOT in_progress or done. Follows the delegation-enforcement decisions #2 and #7: reassigning must clear the step's was_delegated record so the new model must actually run it.

## Decision
New SessionControl method (crates/comrade-tool/src/plan.rs): fn reassign_step_model(&self, target: &PlanTarget, model: &str) -> Result<bool, String> with Ok(true)=reassigned / Ok(false)=no matching step / Err=status InProgress or Done (\"only pending or blocked\"). Implemented in AgentSession (crates/comrade-core/src/session.rs): finds by PlanTarget::Id/Text, refuses InProgress|Done, sets step.model = model.trim(), removes step.id from the delegated HashSet, emits AgentEvent::PlanChanged. Same semantics replicated in the tool-session StubSession. Agent-facing tool `set_step_model` (crates/comrade-tool-session/src/lib.rs, registered in all()): args index/text (oneOf) + required model; rejects blank model; no-op when unchanged; friendly errors for in_progress/done and no-match. Delegate models are NOT validated here (mirrors set_plan; the delegate tool validates). Added to DENIED_FOR_DELEGATES in crates/comrade-core/src/delegate.rs (+ module doc) so a running delegate cannot reassign plan steps. TUI (crates/comrade-tui/src/tui.rs): Ctrl-A opens a ModelPick overlay listing pending/blocked steps then candidate models (\"self\" + cfg.delegates names); Esc closes, j/k/up/down move, enter/right to model column, 1..N or enter assigns; result/guard errors surface as a chat Meta message via session.reassign_step_model. Ctrl+M was NOT used because terminals send Ctrl+M as Enter.

## Consequences
Terminal note: do not bind TUI keys to Ctrl+M (indistinguishable from Enter). TUI ModelPick keeps only pending/blocked steps; a step that turns in_progress while the overlay is open fails the session guard and reports via Meta. Empty-model steps written by direct session calls are treated as self. Tests: comrade-core session tests reassign_changes_model_and_clears_delegation_record; comrade-tool-session 7 new set_step_model_* tests; delegate deny_list test asserts set_step_model denied.

