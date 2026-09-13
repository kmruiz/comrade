# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with record_glossary.

## agent color
> A stable palette hue assigned to each model (main agent + every configured delegate) by ModelColors when the app starts; used to color the model's name everywhere and, dimmed via band_color, as its sub-chat band.

**References:**
- `crates/comrade-tui/src/colors.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Assigned in App construction and re-assigned in App::reload_config; name_color/band_color are read-only lookups. Palette: AGENT_PALETTE (8 entries); band tint alpha BAND_ALPHA = 0.14 over DIFF_BASE_BG.

## AGENTS.md
> Project instructions file at the working-directory root, read by Comrade and injected into the system prompt as a "## Project instructions (AGENTS.md)" section (before the built-in prompt sections) so the repo's own rules (build commands, style, guardrails) apply from the first turn. Only the project-root AGENTS.md is read (no parent walk, no CLAUDE.md).

**References:**
- `crates/comrade-core/src/instructions.rs`
- `crates/comrade-core/src/react.rs`

**Notes:**
Loaded by crates/comrade-core/src/instructions.rs (load_project_instructions) and injected in react.rs build_system_prompt. Absent/blank file is a no-op.

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

## ask_form
> Session tool (crates/comrade-tool-session) that renders an agent-described interactive form in the chat and returns the human's answers as `id = value` lines. The form is defined by a FormSpec (JSON). Supersedes the removed `ask_user`/`UserPrompt::Question`: a plain question is now a single-field form (a select for bounded options, a text field for free input).

**References:**
- `crates/comrade-tool/src/form.rs`
- `crates/comrade-tool-session/src/lib.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Enabled by the UserPrompt::Form(FormSpec) / UserReply::Form(BTreeMap<String,String>) variants on the UserIo contract. Added FieldKind::DiffChoice (diff_choice) for picking between code diffs. The TUI records a MsgKind::Question transcript entry when a form is asked (and when it is answered) so the question stays visible in focus mode.

## ask_user dialog
> The TUI modal rendered by `draw_dialog`. It shows a `UserPrompt::Confirm` (permission/approval of mutating tools) in yellow, or a `UserPrompt::Form` (ask_form) in cyan with an editable `FormEdit`. The old `UserPrompt::Question`/`ask_user` dialog was removed (ask_form supersedes it).

**References:**
- `crates/comrade-tui/src/tui.rs (draw_dialog)`
- `crates/comrade-tool-session/src/lib.rs (ASK_FORM_SPEC)`

**Notes:**
The dialog wraps its body to the popup's real inner width (popup = min(area.width-2, 100) wide, minus 2 border cols), sizes its height to body.len() + 4 (2 borders + input row + hint row) clamped to the terminal, and anchors the body to the TOP so the action/question is never scrolled out of view. Forms edit inline in the body (up/down field, left/right adjust, space toggle, enter submit); a confirm uses y/n, `?` asks the model about the action, esc cancels.

## background session
> A session whose run is still in flight while it is not the active one (its LiveState is parked in its OpenSession slot). Its events keep arriving (routed by session id via on_agent_event_for) and update its own chat/metrics/plan; the session switcher marks it [running].

**References:**
- `crates/comrade-tui/src/tui.rs`

## Blocked session
> Synonym for \"Waiting session\" (the code/UI term of record is now \"waiting\"; the local variable in session_counts_label is still named `blocked`). See the \"Waiting session\" entry.

**References:**
- `crates/comrade-tui/src/tui.rs`

## CompactRequest
> A cheap, clonable one-shot flag (`Arc<AtomicBool>`) the UI uses to ask the running agent loop to compact the context at its next rest point. `request()` sets it, `take()` consumes it once.

**References:**
- `crates/comrade-tool/src/tool.rs`
- `crates/comrade-core/src/agent.rs`

**Notes:**
Carried on `ToolContext.compact: Option<CompactRequest>`; mirrors the `Steer` control pipe. `None` in headless runs and tests.

## Context compaction
> A user-triggered action (M-c / MxCommand `compact-context`) that asks the model to summarise what has been done and REPLACES the running `ContextManager` history with that summary, instead of the automatic, lossy budget trimming.

**References:**
- `crates/comrade-core/src/compact.rs`
- `crates/comrade-core/src/context.rs`
- `crates/comrade-core/src/agent.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Mid-run it is requested via `comrade_tool::CompactRequest` and honoured by `run_agent_loop` at its rest point before `enforce_budget()`; while idle the TUI runs `compact_history` in a background task. `ContextManager::compact` keeps the system message and folds the summary into the "Earlier context (compacted)" rollup.

## delegate sub-chat
> The chat rows authored by a delegate model (its tool cards and its reply), rendered indented 2 columns under a "| " rule in the delegate's agent color with a dim per-agent background band, visually nested under the parent's delegate tool call.

**References:**
- `crates/comrade-tui/src/tui.rs (subchat_model, render_row_line, draw_chat, layout_chat_rows)`
- `crates/comrade-tui/src/colors.rs`

**Notes:**
Detected per row by subchat_model(msg.author, app.cfg.delegates); drawn by render_row_line's `sub: Option<Color>` param. Folded MsgKind::Run digests keep no sub-chat styling.

## diff_choice
> A `FieldKind` variant ("diff_choice") for ask_form: a pick-list whose options carry a code diff each (`DiffOption { label, diff }`). Rendered as the diffs; the answer is the chosen option's `label`. Used to let the human pick between competing patches.

**References:**
- `crates/comrade-tool/src/form.rs`
- `crates/comrade-tool-session/src/lib.rs`

**Notes:**
Defined via a `FieldKind::DiffChoice { options: Vec<DiffOption> }` variant; seeded with the first option's label (or the field's `recommended`). In the TUI, left/right cycle the options and the selected option's `diff` is shown under the field; typing is ignored.

## enabled (delegate)
> Per-[[delegates]] boolean (default true). `enabled = false` keeps the entry in config.toml but removes the model from the `delegate`/`ask_advise` targets, their `model` enum and advertised listing (and from the TUI Ctrl-A picker), so it cannot be delegated to; it still shows dimmed with ` (disabled)` in the model panel. Unlike `approval = "deny"` (listed but refused at run time), a disabled delegate is invisible to the tech lead.

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tui/src/tui.rs`

## focus mode
> A chat view filter in the TUI, toggled by M-f or M-x focus-mode (same chord turns it off). When on it hides tool noise so the chat reads as pure conversation: it keeps MsgKind::User, MsgKind::Assistant, MsgKind::Delegate (advisories) and MsgKind::Reasoning, and drops MsgKind::Tool, MsgKind::Failure, MsgKind::Meta and the folded MsgKind::Run digest's summary row — but a folded Run still renders the Reasoning children inside it. While a run is in flight in focus mode, the bottom chat row shows a rotating activity spinner (`activity_line`) so a silent tool run never looks frozen.

**References:**
- `crates/comrade-tui/src/tui.rs`

## FormSpec
> JSON-described interactive form: { title, description, fields: [FormField] }, where FormField = { id, label, kind: FieldKind, required, default } and FieldKind is serde-tagged by `kind` (text|number|date|select|checkbox) with per-kind params (placeholder; min/max/step; options). Helpers: initial_values(), is_complete(), answer_lines() emit `id = value`.

**References:**
- `crates/comrade-tool/src/form.rs`

## LiveState
> The per-session half of the TUI App state (crates/comrade-tui/src/tui.rs struct LiveState): session Arc, ctx_base, history, run_tx, stop, run_handle, running, steer_tx, queued_prompt, run_cancelled, chat, section_collapsed, chat_epoch, chat_rows_cache, stream, ctx_tokens/budget/estimated, activity, session_file, sel, scroll_top, follow, was_at_bottom, search. A parked session stores its LiveState in its OpenSession slot (Box); App::swap_live mem::swaps these fields between the App (active session) and a LiveState.

**References:**
- `crates/comrade-tui/src/tui.rs`

## model panel
> The right-hand panel of the TUI titled " model ", drawn by `draw_stats` (crates/comrade-tui/src/tui.rs). Two fixed inner rows now: (1) `label_line` = model name + version left, balance right-aligned; (2) `gauge_line` = context bar merged with `NN%  used/budget` (compact via `short_tokens`, k/M) plus an `est` marker when estimated. Below them: the delegate list from `delegate_panel_rows`. Panel height = `MODEL_PANEL_FIXED_ROWS (2) + 2 borders + delegate rows`.

**References:**
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Redesigned to be compact: previously 3 inner rows (name / gauge / usage) plus a wasted blank row; the '(api)' suffix was dropped (api is the default; only 'est' is shown).

## provider preset
> A named provider in `LlmCfg.provider` (ollama, openai, deepseek, mistral, openrouter, groq, together) that resolves to a preset base URL via `provider_base_url()` when the config omits an explicit `base_url`. All providers are spoken to through the single OpenAI-compatible `LlmClient` (Bearer auth, `/chat/completions` with native tool calls).

**References:**
- `crates/comrade-core/src/config.rs (provider_base_url ~345, fill_provider_base_url ~395)`
- `crates/comrade-core/src/llm.rs (LlmClient ~284, heuristic_context ~709, model_context_from_openai ~740)`

**Notes:**
Mistral (https://api.mistral.ai/v1) is fully OpenAI-compatible: Bearer auth, /chat/completions, and `GET /models` advertising `max_context_length` (already parsed by model_context_from_openai). `heuristic_context` adds a name-based fallback: 128K (131072) for mistral-*/devstral/pixtral/ministral/magistral, 32K (32768) for codestral. Delegate entries resolve their own provider the same way.

## read window
> The 1-based inclusive [start_line, end_line] (or [start,end] range) passed to the fs file readers to select lines; clamped to the file's line count, and an empty/reversed window (hi <= lo) or a start past EOF yields an "empty window" message rather than a slice panic.

**References:**
- `crates/comrade-tool-fs/src/lib.rs:251`
- `crates/comrade-tool-fs/src/lib.rs:852`
- `crates/comrade-tool-fs/src/lib.rs:23`

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

## reasoning block
> The chat element showing a model's visible reasoning between actions (MsgKind::Reasoning). It renders like the assistant final-answer block — a one-line header + markdown body, no left rule, no collapsible card — but headed by a single 🧠 glyph in the model's name colour, and its rows are tinted with the model's dimmed background band (ModelColors::band_color). Visible by default; Tab toggles its body. See `layout_reasoning`.

**References:**
- `crates/comrade-tui/src/tui.rs`

## recommended (form/question value)
> Optional suggested answer that an agent may attach to an `ask_form` field (`recommended` in the field JSON) or to an `ask_user` question (`recommended`). It prefills the field / free-text answer and is flagged " (recommended)" next to a matching option or the field in the TUI. In auto-accept mode (TUI) or `Autonomy::Auto` (headless), the question is answered with it and a form is submitted with the recommended/default values when every required field is satisfied. The agent may obtain a good value by consulting another agent with ask_advise.

**References:**
- `crates/comrade-tool/src/form.rs`
- `crates/comrade-tool/src/tool.rs`
- `crates/comrade-tui/src/tui.rs`
- `crates/comrade-tui/src/headless.rs`
- `crates/comrade-core/prompts/tools-intro.md`

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

## session (TUI)
> A named unit owning its own plan and chat. In the TUI the active session's live state is the App's own fields (AgentSession plan/title, ContextManager history, Vec<Msg> chat); every other opened session is an OpenSession slot holding a Box<SessionFile> snapshot. Ctrl-x C-b switches, C-s saves, C-f loads, C-k closes (kill-session) and C-w forks; the M-x names are switch-session/save-session/load-session/kill-session/fork-session.

**References:**
- `crates/comrade-tui/src/session_store.rs`
- `crates/comrade-tui/src/tui.rs`
- `.comrade/memory/0013-tui-sessions-file-backed-switchsavefork-with-ctrl-x-chords.md`

**Notes:**
SessionFile is the on-disk JSON form (version/title/status/plan/delegated/finished/chat/section_collapsed/ctx_*/history/rollup/evicted). Save/load prompt for a file path each time (find-file semantics). Ctrl-x is a prefix key handled in handle_event; PathPrompt and SessionPick are the two modals it drives. AgentSession::restore and ContextManager::from_parts rebuild the live session on load/switch/fork. new-session is non-destructive: it stashes the current session and opens a fresh empty slot (emacs scratch-buffer semantics); kill-session discards the active slot and activates a neighbour and refuses to close the only session.

## side-by-side diff renderer
> The aligned removed(left)/added(right) diff renderer in the TUI chat (crates/comrade-tui/src/tui.rs): extract_diff_sides pulls (removed, added) line lists, edit_diff_label builds the header, lcs_pairs aligns them (dropping unchanged lines, merging a removed+added pair into one row), and build_diff_row/cell_spans draw each row with diff_remove_bg/diff_add_bg. Handles fs_edit (args: literal old/new or embedded diff) and, since ADR #12, git_diff (parsed from the tool result).

**References:**
- `crates/comrade-tui/src/tui.rs`

## skill
> A Claude-format skill: a directory `<name>/SKILL.md` with optional YAML frontmatter (`name`, `description`) and a markdown body of instructions (possibly referencing bundled files beside it). Comrade discovers skills under `.comrade/skills` (its default), `.claude/skills` (project) and `~/.comrade/skills` + `~/.claude/skills` (personal), project dirs winning on a name clash. Each skill is exposed to the model as a tool named `skill_<name>` whose description is the skill's one-line blurb; invoking it returns the SKILL.md body (progressive disclosure).

**References:**
- `crates/comrade-tool-skill/src/lib.rs`
- `crates/comrade-tui/src/main.rs`

**Notes:**
Frontmatter is parsed by hand (no YAML crate in the workspace). Discovery/parse live in crates/comrade-tool-skill (parse_skill_md, discover_in, discover, all); the TUI registers the tools in main.rs build_tools/delegate_registry/advise_registry, which now take the project root.

## TaggedEvent
> TaggedEvent = (u64, AgentEvent) (crates/comrade-tui/src/main.rs): an agent event tagged with the id of the session that produced it. Each session has its own bounded run-facing sender relayed by spawn_tagged_relay, which forwards (id, event) into the App's single central unbounded queue (App::events_rx); the select loop dispatches via App::on_agent_event_for(id, ev).

**References:**
- `crates/comrade-tui/src/main.rs`
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

## Waiting session
> A session whose in-flight run is paused waiting for human input (a pending ask/dialog: a tool confirmation or a question). Shown as "waiting" in BOTH places: the mode-line session-count label (e.g. "1 running, 1 waiting, 1 idle") and the Ctrl-x C-b switcher ("[waiting]"). A session is waiting iff app.dialogs holds a Dialog with that session's id; running/waiting/idle are a mutually-exclusive partition (waiting takes precedence over running). Each session's TuiUserIo is stamped with its id so PendingAsk/Dialog can be attributed (asks previously came through one shared user io). The internal local variable in session_counts_label is still named `blocked`.

**References:**
- `crates/comrade-tui/src/tui.rs (fn session_status_marker, fn session_counts_label, fn draw_session_pick, struct TuiUserIo, struct Dialog)`

