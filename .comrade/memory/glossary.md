# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with record_glossary.

## agent color
> A stable palette hue assigned to each model (main agent + every configured delegate) by ModelColors when the app starts; used to color the model's name everywhere and, dimmed via band_color, as its sub-chat band.

**References:**
- `crates/comrade-tui/src/colors.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Assigned in App construction and re-assigned in App::reload_config; name_color/band_color are read-only lookups. Palette: AGENT_PALETTE (8 entries); band tint alpha BAND_ALPHA = 0.14 over DIFF_BASE_BG.

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

## ask_user dialog
> The TUI modal shown for a `UserPrompt::Question` (from the `ask_user` tool) and for a `UserPrompt::Confirm` (permission/approval of mutating tools). Rendered by `draw_dialog`.

**References:**
- `crates/comrade-tui/src/tui.rs (draw_dialog ~line 5499, wrap_plain ~line 5633)`
- `crates/comrade-tool-session/src/lib.rs (ASK_USER_SPEC ~line 562)`

**Notes:**
The dialog wraps its body/options to the popup's real inner width (popup = min(area.width-2, 100) wide, minus 2 border cols), sizes its height to `body.len() + 4` (2 borders + input row + hint row) clamped to the terminal, and anchors the body to the TOP so the question/action is never scrolled out of view. Option text wraps via `wrap_plain` and continuation lines are space-padded under the number. Because space is limited, the `ask_user` tool spec instructs the model to ask a single short question with at most 4 short options.

## delegate sub-chat
> The chat rows authored by a delegate model (its tool cards and its reply), rendered indented 2 columns under a "| " rule in the delegate's agent color with a dim per-agent background band, visually nested under the parent's delegate tool call.

**References:**
- `crates/comrade-tui/src/tui.rs (subchat_model, render_row_line, draw_chat, layout_chat_rows)`
- `crates/comrade-tui/src/colors.rs`

**Notes:**
Detected per row by subchat_model(msg.author, app.cfg.delegates); drawn by render_row_line's `sub: Option<Color>` param. Folded MsgKind::Run digests keep no sub-chat styling.

## provider preset
> A named provider in `LlmCfg.provider` (ollama, openai, deepseek, mistral, openrouter, groq, together) that resolves to a preset base URL via `provider_base_url()` when the config omits an explicit `base_url`. All providers are spoken to through the single OpenAI-compatible `LlmClient` (Bearer auth, `/chat/completions` with native tool calls).

**References:**
- `crates/comrade-core/src/config.rs (provider_base_url ~345, fill_provider_base_url ~395)`
- `crates/comrade-core/src/llm.rs (LlmClient ~284, heuristic_context ~709, model_context_from_openai ~740)`

**Notes:**
Mistral (https://api.mistral.ai/v1) is fully OpenAI-compatible: Bearer auth, /chat/completions, and `GET /models` advertising `max_context_length` (already parsed by model_context_from_openai). `heuristic_context` adds a name-based fallback: 128K (131072) for mistral-*/devstral/pixtral/ministral/magistral, 32K (32768) for codestral. Delegate entries resolve their own provider the same way.

## readiness handshake
> ask_advise step=<id> — readiness-check mode of the ask_advise tool: consults the step's OWN delegate (read-only) about whether the step's context suffices to pick it up. Delegate closes with `VERDICT: READY` (step -> PlanStatus::Ready) or `VERDICT: NEEDS_MORE: <requests>` (step stays pending, note "awaiting context: ..."). Fire one call per delegate step in parallel after self_set_plan.

**References:**
- `crates/comrade-core/src/advise.rs (AskAdviseTool::invoke)`
- `crates/comrade-core/prompts/advise-system.md`
- `crates/comrade-core/src/delegate.rs (DENIED_FOR_DELEGATES)`

**Notes:**
Mutually exclusive with model/question/context args. The delegate tool description, delegation-lead.md, delegate-by-default.md and advise-system.md all instruct this handshake. Complemented by self_set_step_context to enrich a step and re-ask.

## ready (PlanStatus::Ready)
> PlanStatus::Ready ("ready") — a plan step whose assigned delegate has confirmed via ask_advise step=<id> that the step's context (goal/verification/context) is sufficient for it to do the work. Sits between Pending and InProgress (lifecycle pending -> ready -> in_progress). Soft gate: informational; the delegate tool still runs from pending.

**References:**
- `crates/comrade-tool/src/plan.rs (PlanStatus enum)`
- `crates/comrade-core/src/advise.rs (readiness_verdict, step-mode invoke)`
- `crates/comrade-tool-session/src/lib.rs (self_set_step_context tool)`
- `crates/comrade-tui/src/tui.rs (plan_glyph)`

**Notes:**
Set by AskAdviseTool step-mode on an explicit final `VERDICT: READY` reply; otherwise the step stays pending with an "awaiting context: ..." note. Reassigning the model or calling self_set_step_context on a ready step resets it to pending. TUI shows a blue ● glyph. Denied to delegate sub-agents.

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

## side-by-side diff renderer
> The aligned removed(left)/added(right) diff renderer in the TUI chat (crates/comrade-tui/src/tui.rs): extract_diff_sides pulls (removed, added) line lists, edit_diff_label builds the header, lcs_pairs aligns them (dropping unchanged lines, merging a removed+added pair into one row), and build_diff_row/cell_spans draw each row with diff_remove_bg/diff_add_bg. Handles fs_edit (args: literal old/new or embedded diff) and, since ADR #12, git_diff (parsed from the tool result).

**References:**
- `crates/comrade-tui/src/tui.rs`

## tool name prefixes (fs_/ts_/pom_/self_)
> The model-facing toolset naming convention (ADR #7): every tool's name carries a domain prefix - fs_* for filesystem tools (comrade-tool-fs), ts_* for tree-sitter/code tools (comrade-tool-syntax), pom_* for project/task tools (comrade-tool-project), self_* for session/planning tools the agent runs on itself (comrade-tool-session), memory tools use the adr/glossary families (record_adr/find_adr/read_adr/amend_adr, record_glossary), and ask_user asks the human. apply_edit+apply_patch merged into fs_edit (literal old/new OR diff); find_definition+read_symbol merged into ts_read_symbol (body flag); references_count was removed. Engine helper fns keep internal names.

**References:**
- `.comrade/memory/0007-namespaced-consistent-tool-names-fs-ts-pom-self-prefixes-merges.md`
- `crates/comrade-tool-syntax/src/lib.rs`
- `crates/comrade-tool-fs/src/lib.rs`
- `crates/comrade-core/src/agent.rs`

**Notes:**
Model-facing tool names appear in: ToolSpec name in each comrade-tool-* lib.rs, comrade-core tables (MUTATING_TOOLS/APPROVAL_GATED_TOOLS/READ_ONLY_TOOLS/CODE_CHANGES in agent.rs, DENIED_FOR_DELEGATES in delegate.rs), prompts/*.md, react.rs assertions, and tui.rs name-keyed rendering.

