# 0002 - Mechanize plan-step delegation instead of prompt persuasion
status: accepted
tags: delegate, plan, agent-loop, enforcement, session
summary: Delegation is now enforced: update_plan/finish_plan refuse to close a step assigned a delegate `model` until the delegate tool has run it.

## Context
Users reported the root/planner model never actually delegated plan steps to configured [[delegates]] models: DelegateTool existed (crates/comrade-core/src/delegate.rs), was advertised to the model only when delegates were configured, and had integration tests — but delegation was purely voluntary prompt persuasion. The working-style prompt simultaneously told the root "you are the developer, write real code yourself", so in practice steps assigned a `model` via set_plan were executed by the root itself and marked done, bypassing the delegate tool entirely.

## Decision
Enforce delegation mechanically instead of by persuasion:
1. comrade-tool SessionControl (crates/comrade-tool/src/plan.rs) gained two defaulted methods: mark_step_delegated(id) and step_was_delegated(id) -> bool.
2. AgentSession (crates/comrade-core/src/session.rs) stores the set of delegated step ids (RwLock<HashSet<u64>>, reset on every set_plan) and overrides both methods.
3. DelegateTool::invoke calls mark_step_delegated(id) once a delegate reply arrives for a plan step (the step branch only; ad-hoc `task` runs mark nothing).
4. The update_plan tool (crates/comrade-tool-session/src/lib.rs) refuses status=done on a step whose `model` is non-empty unless step_was_delegated(id); finish_plan refuses while any Pending/InProgress step has a non-empty model that was never delegated. Error text tells the root to delegate (delegate tool with step=<id>) or, if no delegate is available, replace the plan via set_plan leaving `model` empty.
5. react.rs system-prompt delegation paragraph + delegate tool description now state delegation is enforced, not optional.
Escape hatches kept: delegate-failure restores previous status; after 5 failed fix rounds the delegate DID run the step (was_delegated true) so the root may legitimately take over and close it.

