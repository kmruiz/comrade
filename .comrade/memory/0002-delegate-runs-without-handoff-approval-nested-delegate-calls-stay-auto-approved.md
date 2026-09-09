# 0002 - delegate runs without handoff approval; nested delegate calls stay auto-approved
status: superseded
date: 2026-09-09
tags: tools, delegate, approval, security, autonomy
summary: delegate is no longer approval-gated: removed from APPROVAL_GATED_TOOLS and its two ctx.confirm handoff prompts; nested delegate tool calls remain auto-approved. ask_advise was already ungated.

## Context
The `delegate` tool was approval-gated twice over: listed in APPROVAL_GATED_TOOLS (agent.rs) so the main loop demanded a Justification, and DelegateTool::invoke (delegate.rs) called ctx.confirm before running a plan-step or ad-hoc delegate ('Delegate plan step N to X?' / 'Delegate task to X?'). `ask_advise` was already ungated (read-only advisor, no ctx.confirm). The user asked to make both delegate and ask_advise not require approval. Date: 2026-09-09 (session).

## Decision
`delegate` is no longer approval-gated: removed from APPROVAL_GATED_TOOLS and the two ctx.confirm handoff prompts in DelegateTool::invoke were deleted, so delegating a plan step or ad-hoc task runs directly and marks the step working immediately. A delegate's nested tool calls stay auto-approved inside its own run (dctx.auto_approve = true in run_delegate_subagent) — unchanged. `ask_advise` remains ungated and read-only; nothing changed for it. Tool descriptions, module docs and the delegation/protocol prompt markdown were reworded to say delegating runs without human approval.

## Rationale
The user explicitly requested approval-free delegate/ask_advise. Because every nested delegate tool call was already auto-approved after the single handoff prompt, the handoff approval was the last remaining pause; removing it matches the request and makes headless/auto runs smoother. ask_advise needed no code change (already ungated, read-only).

## Alternatives considered
1) Keep the handoff approval and only drop the Justification requirement — rejected: the two ctx.confirm prompts in DelegateTool::invoke were the actual human gate, so removing just the classification would not stop the dialogs. 2) Make ungated behaviour conditional on config (security.autonomy) — rejected: user asked for a simple unconditional change; delegate nested tools were already auto-approved, and ask_advise is read-only, so the residual risk of running delegates unattended was judged acceptable by the user. 3) Leave as-is — rejected by request.

## Scope
covers delegate approval classification in agent.rs (APPROVAL_GATED_TOOLS), the two ctx.confirm handoffs in delegate.rs, and the wording in delegate.rs module doc/tool description, run_delegate_subagent docs, prompts/delegation-lead.md and prompts/protocol.md. Does not cover the other approval-gated tools or autonomy config.

## Impact
Delegating now has no human checkpoint: the lead can spawn sub-agents that auto-approve their own write/edit/run/remember calls unattended. Approval gates remain for write_file/rename/shell/remember/amend_decision/remember_glossary. The delegate tool's schema no longer advertises a `justification` argument. Follow-ups to consider: an autonomy-level config knob if unattended delegation ever needs to be restricted again.


## Note
Superseded by #3: delegation is no longer unconditionally approval-free. Each [[delegates]] entry can opt back into an approval pause (`approval = "ask"`) or a hard refusal (`approval = "deny"`); the default (`"auto"`) keeps ADR #2's ungated behaviour. delegate.rs/advise.rs enforce the per-delegate gate with ctx.confirm before running.
