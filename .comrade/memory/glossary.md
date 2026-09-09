# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with remember_glossary.

## approval ([[delegates]])
> Per-[[delegates]] config key (crates/comrade-core/src/config.rs DelegateCfg.approval) controlling whether delegate/ask_advise may run that model without asking: "auto" (default) runs directly, "ask" pauses via ToolContext::confirm (skipped when the context is auto-approved), "deny" refuses to run that model through delegate/ask_advise at all. Enforced by delegate::enforce_approval inside DelegateTool::invoke and AskAdviseTool::invoke before a run starts (and before a delegated plan step is marked working).

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/delegate.rs (enforce_approval, cfg_line)`
- `crates/comrade-core/src/advise.rs`
- `.comrade/memory/0003-per-delegate-approval-policy-for-delegateask-advise.md`

**Notes:**
Reuses the Autonomy enum (ask/auto/deny). Delegates with ask/deny are annotated in listings via delegate::cfg_line.

## ask_advise
> Consultations normally need no approval, but the chosen delegate's `approval` policy applies (see "approval ([[delegates]])"): `ask` pauses for human approval before the advice runs, `deny` refuses outright.

**References:**
- `crates/comrade-core/src/advise.rs`
- `.comrade/memory/0003-per-delegate-approval-policy-for-delegateask-advise.md`

**Notes:**
Replaces the former unconditional "no approval" wording.

## readiness handshake
> ask_advise step=<id> — readiness-check mode of the ask_advise tool: consults the step's OWN delegate (read-only) about whether the step's context suffices to pick it up. Delegate closes with `VERDICT: READY` (step -> PlanStatus::Ready) or `VERDICT: NEEDS_MORE: <requests>` (step stays pending, note "awaiting context: ..."). Fire one call per delegate step in parallel after set_plan.

**References:**
- `crates/comrade-core/src/advise.rs (AskAdviseTool::invoke)`
- `crates/comrade-core/prompts/advise-system.md`
- `crates/comrade-core/src/delegate.rs (DENIED_FOR_DELEGATES)`

**Notes:**
Mutually exclusive with model/question/context args. The delegate tool description, delegation-lead.md, delegate-by-default.md and advise-system.md all instruct this handshake. Complemented by set_step_context to enrich a step and re-ask.

## ready (PlanStatus::Ready)
> PlanStatus::Ready ("ready") — a plan step whose assigned delegate has confirmed via ask_advise step=<id> that the step's context (goal/verification/context) is sufficient for it to do the work. Sits between Pending and InProgress (lifecycle pending -> ready -> in_progress). Soft gate: informational; the delegate tool still runs from pending.

**References:**
- `crates/comrade-tool/src/plan.rs (PlanStatus enum)`
- `crates/comrade-core/src/advise.rs (readiness_verdict, step-mode invoke)`
- `crates/comrade-tool-session/src/lib.rs (set_step_context tool)`
- `crates/comrade-tui/src/tui.rs (plan_glyph)`

**Notes:**
Set by AskAdviseTool step-mode on an explicit final `VERDICT: READY` reply; otherwise the step stays pending with an "awaiting context: ..." note. Reassigning the model or calling set_step_context on a ready step resets it to pending. TUI shows a blue ● glyph. Denied to delegate sub-agents.

## runbook-style prompt
> The convention that all model-facing prompt text in Comrade must be terse, imperative runbook prose (numbered steps, one idea per line, short sentences, exact tool names in backticks), so that small models can act as the tech lead. Applies to the tech-lead prompt sections, the delegate/advisor sub-agent system bodies, and ToolSpec descriptions.

**References:**
- `crates/comrade-core/prompts/delegate-by-default.md`
- `crates/comrade-core/prompts/delegate-system.md`
- `crates/comrade-core/src/react.rs`
- `crates/comrade-core/src/delegate.rs`
- `.comrade/memory/0006-restyle-all-model-facing-prompts-as-terse-runbooks-for-small-model-tech-leads.md`

**Notes:**
Adopted in ADR #6. Prompt sources: crates/comrade-core/prompts/*.md (assembled by react::build_system_prompt and delegate::render_subagent_system) and the ToolSpec.description strings in every comrade-tool-* crate. Known follow-ups: comrade-core delegate/ask_advise tool descriptions and json_schema per-property descriptions are still verbose.

