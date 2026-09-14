# 0001 - ask_advise tool: read-only advisory consult of a delegate
status: accepted
date: 2026-09-09
tags: tools, delegate, advice
summary: ask_advise: consult a configured delegate for advice via a read-only sub-agent, no handoff/plan/approval

## Context
The main model kept using the `delegate` hand-off (or its own judgement) even when it only wanted a second opinion, e.g. how to plan/split a task before committing to the shape of the work. `delegate` marks plan steps working, grants an auto-approved full tool registry and a human handoff approval — heavyweight for a consult. A new `ask_advise` tool was requested that lets the lead ask a configured delegate for advice without handing off any work.

## Decision
Added an `ask_advise` tool (comrade-core/src/advise.rs): the lead passes `model` + `question` (+optional `context`) and one configured delegate runs a READ-ONLY sub-agent (reusing delegate.rs's run_delegate_subagent) then replies with advice text. Its tool registry is exactly the main loop's read-only classification (`agent::is_read_only`, exposed pub; whitelisted in comrade-tui's advise_registry()) so advisors can browse/search/git-log/consult memory+web but can never write/edit/run/commit/plan/ask. It is not approval-gated, never touches plan steps, and costs one extra model conversation. `delegate` and `ask_advise` share one source of truth for [[delegates]] validation (`delegate::build_targets`) and the delegate target struct; delegates are denied `ask_advise` (DENIED_FOR_DELEGATES) so a sub-agent cannot spawn extra model chats.

## Rationale
Advice should be grounded (user chose read-only tools) but must never mutate state; reusing the delegate sub-agent loop gives protocol parity (native + react delegates) with a different system prompt and nudge wording, and reusing agent.rs's is_read_only avoids a third tool-classification list drifting from the main loop.

## Alternatives considered
1) Text-only advice (no tools at all): cheapest, but an advisor cannot ground advice in the repo; user explicitly chose read-only tools. 2) Reuse the delegate sub-agent with the full tool registry minus DENIED_FOR_DELEGATES: would let advisors run tasks/mutate, defeating 'advice not execution'. 3) A separate hand-maintained whitelist of read-only tools: rejected because it would drift from the main loop's read-only classification (agent.rs READ_ONLY_TOOLS).

## Scope
covers comrade-core AskAdviseTool, its prompt (prompts/advise-system.md), advise_registry() in comrade-tui, tool deny/approval classification. Does not cover giving advisors non-read-only tools or plan-step integration.

## Impact
The lead can consult cheaper/faster configured delegates as advisors (prompt delegate-by-default.md now points advisory reviews at ask_advise). Registry building in comrade-tui is duplicated across delegate_registry()/advise_registry() but both are thin filters over shared name-level truth. Tool only exists when [[delegates]] are configured. UI: ask_advise shares the delegate tool icon/headline/reply-card parsing.


## Merged from #0002 - delegate runs without handoff approval; nested delegate calls stay auto-approved
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

## Merged from #0003 - Per-delegate approval policy for delegate/ask_advise
status: accepted
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

## Merged from #0004 - Plan-step readiness handshake before delegation
status: accepted
date: 2026-09-09
tags: delegate, plan, ask_advise, readiness, plan-status
summary: ask_advise gained a step= readiness mode that marks a delegate-assigned plan step `ready` when its delegate confirms the context suffices; set_step_context lets the lead feed missing context back in.

## Context
Delegated agents sometimes lacked context because the tech-lead wrote the step context it thought important, not what the delegate actually needed. Implemented a readiness handshake: before a delegate-assigned plan step is picked up, the step's own delegate is consulted (read-only) about whether the step's goal/verification/context suffice.

## Decision
Extend the ask_advise tool with an optional `step` argument: with `step` = a plan-step id, the step's OWN delegate (model must match or be omitted) is consulted about context readiness. The advisor replies with a final verdict line; an explicit `VERDICT: READY` marks the step PlanStatus::Ready (a new state between pending and in_progress); `VERDICT: NEEDS_MORE: ...` leaves it pending with an "awaiting context: ..." note. The lead enriches the step via a new set_step_context tool (pending/ready/blocked only; replacing the context of a ready step drops it back to pending) and re-asks until ready. This is a SOFT gate: the delegate tool still runs a step from pending; `ready` is informational, encouraged by prompts (delegate-by-default.md, delegation-lead.md, advise-system.md, delegate tool description).

## Rationale
Reuses the existing ask_advise consult machinery (read-only advisor sub-agent, approval policy, parallel batching of multiple tool calls in one message) instead of a new parallel scheduler. The delegate of a single task is the right judge of its own context needs, and asking per-step keeps steps isolated. Marking ready only on an explicit VERDICT: READY keeps the auto-mark deterministic.

## Alternatives considered
(1) New dedicated tool prepare_step/ready_step with a hard gate refusing delegation of non-ready steps — rejected: user chose extending ask_advise and a soft gate. (2) State only + prompt guidance (lead uses plain ask_advise then update_plan) — rejected: no deterministic auto-mark. (3) Hard enforcement that delegate refuses non-ready steps — rejected by user; ready is informational.

## Scope
PlanStatus lifecycle and session tooling across comrade-tool (plan.rs), comrade-core (session.rs AgentSession, advise.rs AskAdviseTool, delegate.rs DENIED_FOR_DELEGATES), comrade-tool-session (set_step_context tool, update_plan/finish_plan open filters), comrade-tui (Ready glyph/color), lead/advisor prompts. Does not change the delegate tool's run mechanics or fix-round logic.

## Impact
Plan steps assigned to delegates now move pending -> ready -> in_progress; ready is visible in the TUI (blue ●). Reassigning a ready step's model or rewriting its context resets it to pending (readiness is stale). set_step_context and ask_advise step= are denied to delegate sub-agents. Future: could make the gate hard (refuse delegating non-ready steps) once flows mature.

## Merged from #0018 - Per-delegate `enabled` flag to disable a model without removing it
status: accepted
date: 2026-09-13
tags: config, delegate, ask_advise, tui, models
summary: Each [[delegates]] entry gained `enabled = true|false` (default true); a disabled delegate is dropped from delegate/ask_advise targets + model enum + advertised listing (and from the Ctrl-A picker), but kept in config and shown dimmed in the model panel.

## Context
Users wanted to temporarily turn a configured delegate off without deleting its [[delegates]] entry from config.toml (and having to re-add provider/model/key later). ADR #3 already added a per-delegate `approval` policy on the same struct, so the config vocabulary for "which delegates may run" was established there.

## Decision
Added `pub enabled: bool` to DelegateCfg (crates/comrade-core/src/config.rs), defaulting to `true` (serde(default) + manual Default impl) so existing configs are unaffected. Semantics: a delegate with `enabled == false` is treated as if not configured for the agent. `build_targets` skips disabled entries BEFORE any validation (no name/model check, no HTTP client); a disabled entry may therefore be blank/duplicate without breaking the build. `DelegateTool::new` and `AskAdviseTool::new` return `Ok(None)` when no enabled delegate remains, and build the advertised listing from enabled entries only, so disabled models are absent from the `model` enum and the tool description. In the TUI (crates/comrade-tui/src/tui.rs) `delegate_panel_rows` still shows disabled delegates, dimmed with a trailing ` (disabled)` marker, and the Ctrl-A model picker filters them out. Disabling affects BOTH `delegate` and `ask_advise` (a disabled model is disabled everywhere), unlike `approval = "deny"` which keeps the model listed but refuses it at run time.

## Rationale
Reuses the existing per-delegate config surface and the same shared `build_targets`/listing path as ADR #3, so delegate and ask_advise stay consistent with no new plumbing. Defaulting to true preserves today's behaviour. Filtering (rather than listing-but-refusing like `deny`) keeps the tech lead's option list truthful — it never sees or tries a name it cannot use — while the TUI panel keeps the human informed the entry is configured but off.

## Alternatives considered
1) Reuse `approval = "deny"`: keeps the model advertised and only refuses at run time, so the lead still sees/selects a model that can never run — not what "disabled" should mean. 2) Remove the entry from config: the user explicitly did not want to lose the settings. 3) Hide disabled delegates from the TUI panel too: rejected — the human should still see a configured-but-off model. 4) Skip provider/base_url validation for disabled entries in apply_provider: rejected for now (kept simple; the entry's provider is still resolved so the panel can display it).

## Scope
Covers the config field, the fill/parse behaviour, build_targets/DelegateTool::new/AskAdviseTool::new filtering, the TUI panel marker + picker filter, and new tests in comrade-core (config, delegate, advise) and comrade-tui. Does not touch `approval`, `security.autonomy`, the delegate sub-agent internals, or MCP servers.

## Impact
A user writes `enabled = false` on a [[delegates]] entry to park a model: it disappears from the delegate/ask_advise options and the Ctrl-A picker but stays visible (dimmed, ` (disabled)`) in the model panel and remains in config.toml to flip back on. Applies to both `delegate` and `ask_advise`. Follow-up: the TUI model panel test covers the marker; a docs/config example could mention `enabled`.

## Note
Rollup of the delegation model: this ADR is the spine (ask_advise, read-only advisory sub-agent), #0002 makes delegate ungated, #0003 adds the per-delegate approval policy (ask/auto/deny), #0004 adds the plan-step readiness handshake, #0018 adds the per-delegate enabled flag. Bodies preserved under "Merged from".
