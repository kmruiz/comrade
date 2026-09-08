# 0007 - Plan steps must name their model: \"self\" or a delegate
status: accepted
tags: plan, delegate, session, set_plan, model
summary: set_plan now requires a model per step; the main agent is the reserved token AGENT_MODEL = \"self\", delegates use config names.

## Context
Changed the plan-registration contract (comrade-tool-session SetPlan/UpdatePlan/FinishPlan, comrade-tool::plan AGENT_MODEL const, comrade-core delegate tool). Every plan step must state which model runs it; an empty model is no longer the way to say \"the lead does it\".

## Decision
1. Add/keep `use comrade_tool::AGENT_MODEL;` (defined in crates/comrade-tool/src/plan.rs as `pub const AGENT_MODEL: &str = "self"` and re-exported at crate root).
2. In a plan step, set `model` to AGENT_MODEL (\"self\") when the main tech-lead model runs the step, or to a configured delegate name otherwise. The set_plan JSON schema now requires `[\"goal\",\"model\"]` per step and SetPlan::invoke rejects blank models.
3. Treat \"delegated\" as: `let m = step.model.trim(); !m.is_empty() && m != AGENT_MODEL` — this is the `is_delegate_model` helper in comrade-tool-session/src/lib.rs used by update_plan (Done guard) and finish_plan. A \"self\" step can be marked done / auto-finished with no delegate run.
4. The delegate tool refuses to run a plan step whose model is AGENT_MODEL (error \"assigned to the main agent model ... not a delegate\"), and DelegateTool::new rejects any delegate configured with the reserved name \"self\".
5. When editing plan/prompt text, say steps need \"the `model` that will run it (\\\"self\\\" or a delegate name)\" — never \"model is optional / empty means you run it\".

## Consequences
Delegation enforcement (update_plan/finish_plan refusing to close a delegate-assigned step until the delegate tool ran it) is unchanged for delegate models. Direct `SessionControl::set_plan` writes in tests may still carry an empty model; that is treated as \"self\" by the plan tools and rejected only when going through the SetPlan tool.

