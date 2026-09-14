# 0003 - Per-delegate approval policy for delegate/ask_advise
status: superseded
date: 2026-09-09
tags: tools, delegate, ask_advise, approval, config, security
summary: Each [[delegates]] entry gets `approval = "ask"/"auto"/"deny"` (default "auto"); delegate and ask_advise enforce it with ctx.confirm before a run, so expensive/risky models can require human approval while cheap ones stay ungated.

## Context
ADR #2 made delegate (and ask_advise) unconditionally approval-free because the user asked for unattended delegation. Later the user wanted the opposite granularity: keep cheap models ungated but require human approval for an expensive model. delegate/ask_advise decide the target model per call, so a static tool-name approval list (agent.rs APPROVAL_GATED_TOOLS) cannot express the rule — the gate must be per configured delegate and enforced inside the tool that knows which model was chosen. Date: 2026-09-09 (session).

## Decision
Added a per-delegate `approval` policy field (reusing the existing Autonomy enum, serde lowercase) to DelegateCfg (config.rs), defaulting to Autonomy::Auto so existing configs keep today's ungated behaviour. Semantics: `auto` — delegate/ask_advise to this model run directly, exactly as before; `ask` — the tool calls ToolContext::confirm before running (title names the delegate and step/task, body shows a truncated task/question preview); `deny` — the tool refuses to run this model via delegate/ask_advise at all, even under security.autonomy = auto (a config-level block). Enforced by a shared pub(crate) enforce_approval in delegate.rs, called from both DelegateTool::invoke and AskAdviseTool::invoke right after the target model is resolved and BEFORE a delegated plan step is marked working (the update_plan call was deferred out of the step-resolution branch so a denied run never leaves a step half-claimed at InProgress/working). Delegates with ask/deny are annotated in every delegate listing (`[human approval required before it runs]` / `[refused: ...]` via cfg_line) so the tech lead can tell which models pause. ctx.confirm still short-circuits under auto_approve, so a global autonomy=auto run keeps skipping per-delegate ask gates.

## Rationale
Gating at tool-invoke time is the only place both the chosen model and the human channel are available: the main loop classifies tools by static name only, and a delegate/ask_advise call is approval-free or not depending on the `model` argument. Reusing the Autonomy enum keeps config vocabulary consistent with security.autonomy and gives a free hard-refusal mode (deny). Default Auto preserves ADR #2 behaviour for everyone who does not opt in. Deferring the plan-step InProgress update until after approval keeps the plan truthful when the human says no.

## Alternatives considered
1) Bool requires_approval per delegate: simpler but no deny mode and a second, inconsistent config vocabulary. 2) Make delegate/ask_advise statically approval-gated (add to APPROVAL_GATED_TOOLS) and check the model inside: would force a Justification and gate cheap models too, opposite of the ask. 3) Loop-level dynamic classification reading args.model against config: the loop has no access to the delegate config/registry, and ApprovalNotes plumbing risks stale notes leaking into the next unrelated confirm.

## Scope
Covers the config field on [[delegates]], enforcement inside DelegateTool::invoke and AskAdviseTool::invoke (including plan-step delegation ordering), annotated listings, prompt/doc wording, and the new tests. Does not change the other APPROVAL_GATED_TOOLS, security.autonomy, or the delegate sub-agent's internal auto-approve.

## Impact
A user with a cheap + expensive model pair writes `approval = "ask"` on the expensive [[delegates]] entry; every delegate/ask_advise use of it then pauses for the human while the cheap one stays automatic. The model's justification line is NOT surfaced on these dialogs (deliberately: no ApprovalNotes plumbing for dynamically-gated calls); the dialog shows the delegate name plus the task/question preview. Follow-ups: optionally surface the lead's Justification on ask-gated dialogs; a docs/config example for the cheap/expensive setup.


## Note
merged into #0001
