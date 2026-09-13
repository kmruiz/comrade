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

## background job (BgHub)
> The background-process tools `run_bg`/`bg_status`/`bg_tail`/`bg_kill` (comrade-tool-project `src/bg.rs`): they start a detached `bash -c` job and poll/kill it. One `BgHub` (jobs map + id counter) per `all()` call is shared by the four tools.

**References:**
- `crates/comrade-tool-project/src/bg.rs`
- `.comrade/memory/0024-background-jobs-detached-processes-with-a-shared-bghub-in-comrade-tool-project.md`

**Notes:**
Children run with `kill_on_drop(true)`; output is captured into a 200 KB bounded buffer; `run_bg` is approval-gated and all four are DENIED_FOR_DELEGATES. See ADR #24.

## background session
> A session whose run is still in flight while it is not the active one (its LiveState is parked in its OpenSession slot). Its events keep arriving (routed by session id via on_agent_event_for) and update its own chat/metrics/plan; the session switcher marks it [running].

**References:**
- `crates/comrade-tui/src/tui.rs`

## BgJobs
> A cheap, cloneable handle to the shared background-job registry (crates/comrade-tool-project/src/bg.rs), for observers outside the four `bg_*` tools. Created by `BgJobs::new()`; returned alongside the tools by `comrade_tool_project::all_with_jobs()`. Methods: `list()` (all jobs, oldest first, as `BgJobInfo { id, command, status, running, elapsed_secs }`), `running()` (running only), `kill(id)` (cancels the job; false if unknown). The TUI carries one on Deps/App to draw the "background jobs" panel below the plan and to back M-x stop-background-job.

**References:**
- `crates/comrade-tool-project/src/bg.rs`
- `crates/comrade-tool-project/src/lib.rs (all_with_jobs)`
- `crates/comrade-tui/src/tui.rs (draw_jobs, open_jobs_pick, handle_jobs_pick_key)`

## Blocked session
> Synonym for \"Waiting session\" (the code/UI term of record is now \"waiting\"; the local variable in session_counts_label is still named `blocked`). See the \"Waiting session\" entry.

**References:**
- `crates/comrade-tui/src/tui.rs`

## code chunk
> An indexable unit of source produced by the tree-sitter chunker: for Rust, one declaration (fn/struct/enum/…, recursing into impl/mod/trait so a method is its own chunk with its container head as text context), else a 40-line window. Each chunk carries file + 1-based line (the declaration's line, or the window's first line) and kind/name, so an embedding hit is a location.

**References:**
- `crates/comrade-tool-syntax/src/chunks.rs`
- `crates/comrade-tool-memory/src/semantic.rs`

## CommandLine (Program/Shell)
> `tasks::CommandLine` in crates/comrade-tool-project/src/tasks.rs: how a resolved task runs. `Program { program, args }` (e.g. cargo, npm, mvn) or `Shell { script }` (run with bash -c). Generalized from the old cargo-only variant so a non-Cargo Ecosystem emits its own tool.

**References:**
- `crates/comrade-tool-project/src/tasks.rs`

**Notes:**
`CommandLine::describe()` renders the header line. `tasks::exec`/`run` execute it; the `find_cargo` PATH fallback only triggers for `Program { program == "cargo" }`.

## CompactRequest
> A cheap, clonable one-shot flag (`Arc<AtomicBool>`) the UI uses to ask the running agent loop to compact the context at its next rest point. `request()` sets it, `take()` consumes it once.

**References:**
- `crates/comrade-tool/src/tool.rs`
- `crates/comrade-core/src/agent.rs`

**Notes:**
Carried on `ToolContext.compact: Option<CompactRequest>`; mirrors the `Steer` control pipe. `None` in headless runs and tests.

## Context compaction
> Replacing the running `ContextManager` history with a model-written summary instead of the automatic, lossy budget trimming. Two paths: (1) user-triggered (M-c / M-x compact-context) via `comrade_tool::CompactRequest`; (2) AUTOMATIC — `run_agent_loop` calls `compact_history` when `cfg.context.auto_compact` is on (default) and `ContextManager::needs_auto_compaction()` is true (over the trim target) and the model has taken at least one turn, at most once per over-budget episode (`auto_compact_armed`).

**References:**
- `crates/comrade-core/src/compact.rs`
- `crates/comrade-core/src/context.rs`
- `crates/comrade-core/src/agent.rs`
- `crates/comrade-core/src/config.rs`
- `.comrade/memory/0034-auto-compaction-in-the-agent-loop-ctxcfgauto-compact-default-on.md`

**Notes:**
Mid-run it is requested via `comrade_tool::CompactRequest` and honoured by `run_agent_loop` at its rest point before `enforce_budget()`; while idle the TUI runs `compact_history` in a background task. `ContextManager::compact` keeps the system message and folds the summary into the "Earlier context (compacted)" rollup. The automatic path was added in ADR #34; disable with `[context] auto_compact = false`.

## delegate sub-chat
> The chat rows authored by a delegate model (its tool cards and its reply), rendered indented 2 columns under a "| " rule in the delegate's agent color with a dim per-agent background band, visually nested under the parent's delegate tool call.

**References:**
- `crates/comrade-tui/src/tui.rs (subchat_model, render_row_line, draw_chat, layout_chat_rows)`
- `crates/comrade-tui/src/colors.rs`

**Notes:**
Detected per row by subchat_model(msg.author, app.cfg.delegates); drawn by render_row_line's `sub: Option<Color>` param. Folded MsgKind::Run digests keep no sub-chat styling.

## delegate_parallel
> `delegate_parallel` (comrade-core delegate.rs, `DelegateParallelTool`): runs up to 8 independent delegate jobs concurrently in ONE call (jobs: [{model, task, context}]) and returns each reply. Works under both native and ReAct protocols, unlike the loop's own parallel dispatch of a pure-delegate native batch.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tui/src/main.rs (build_tools)`
- `.comrade/memory/0023-parallel-delegates-via-a-one-call-delegate-parallel-tool.md`

**Notes:**
Each job's approval policy is enforced before any run starts; it never touches the plan; it is DENIED_FOR_DELEGATES so a delegate cannot fan out recursively. See ADR #23.

## detect_all / pick (ecosystem selection)
> A repository may host several build systems (a Cargo.toml AND a package.json). ecosystem::detect_all(root) returns every present backend in priority order (Cargo, then npm); ecosystem::pick(root, ecosystem, verb) selects one: an explicit `ecosystem` name, else the sole backend, else the single backend whose `supports(root, verb)` is true, else an error asking for an explicit choice. `detect` (single) prefers Cargo. pom_model renders ALL detected ecosystems; pom_run_task/pom_run_tests/pom_check/pom_format_code take an `ecosystem` arg.

**References:**
- `crates/comrade-tool-project/src/ecosystem.rs`
- `crates/comrade-tool-project/src/node.rs`

## diff_choice
> A `FieldKind` variant ("diff_choice") for ask_form: a pick-list whose options carry a code diff each (`DiffOption { label, diff }`). Rendered as the diffs; the answer is the chosen option's `label`. Used to let the human pick between competing patches.

**References:**
- `crates/comrade-tool/src/form.rs`
- `crates/comrade-tool-session/src/lib.rs`

**Notes:**
Defined via a `FieldKind::DiffChoice { options: Vec<DiffOption> }` variant; seeded with the first option's label (or the field's `recommended`). In the TUI, left/right cycle the options and the selected option's `diff` is shown under the field; typing is ignored.

## Dispatch (tool)
> `agent::Dispatch{cfg,hooks,redactor}` in crates/comrade-core/src/agent.rs: the single wrapper every main-agent tool call goes through (ReAct path, native path, and the deferred parallel-delegate batch). It runs pre-hooks, applies the optional per-tool timeout (`cfg.agent.tool_timeout_secs`), redacts output, and runs post-hooks. Replaced the three raw `tool.invoke(...)` sites.

**References:**
- `crates/comrade-core/src/agent.rs`
- `.comrade/memory/0037-tool-dispatch-prepost-hooks-per-tool-timeout-per-run-budget.md`

## Ecosystem (pom_*)
> The trait (crates/comrade-tool-project/src/ecosystem.rs) backing the `pom_*` tools. A backend knows its manifest, the project model, how to map a logical verb to a command, the check command, and how to parse diagnostics/test output. `detect(root)` returns `Box<dyn Ecosystem>` (Cargo today, keyed on `Cargo.toml`).

**References:**
- `crates/comrade-tool-project/src/ecosystem.rs`
- `crates/comrade-tool-project/src/lib.rs`
- `.comrade/memory/0026-ecosystem-seam-for-the-pom-tools-cargo-backend-npmmavengo-later.md`

**Notes:**
Defaults let a minimal backend implement only model/resolve/format_command: `check_command` defaults to None, `parse_diagnostics` to the generic error-line scan, `simplify_tests` to passthrough, `is_test_command` to false. Adding npm/Maven/Go = implement the trait + one arm in `detect`. See ADR #26.

## embedded embedding model
> The int8-quantized BGE-small-en-v1.5 ONNX bundle compiled into the comrade-tool-memory binary so semantic_search runs fully offline (no download, no model cache). Raw files live in crates/comrade-tool-memory/assets/bge-small-en-v1.5-int8/ (source of truth); build.rs deflates them with flate2 into OUT_DIR/assets/*.deflate and src/semantic.rs embeds those compressed copies (include_bytes!) and inflates them in memory on first use via flate2::DeflateDecoder. Built with fastembed's try_new_from_user_defined + Pooling::Cls.

**References:**
- `crates/comrade-tool-memory/src/semantic.rs`
- `crates/comrade-tool-memory/build.rs`
- `crates/comrade-tool-memory/assets/bge-small-en-v1.5-int8/README.md`
- `Cargo.toml`

**Notes:**
Deflating the ~35 MB of assets saves ~10 MB of binary; the workspace [profile.release] strip=true removes a further ~14 MB of symbol tables. Release binary went ~96 MB -> ~68 MB.

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

## Hooks (pre/post-tool)
> `crates/comrade-core/src/hooks.rs`: user-configured shell commands run around a tool call. Configured as `[[hooks.pre_tool]]` / `[[hooks.post_tool]]` with `on` (match: `*`, exact name, or `name*` prefix) and `run` (bash -c). Exposes COMRADE_TOOL, COMRADE_ARGS, and (post only) COMRADE_OK. A failing pre-hook aborts the tool; a failing post-hook warns. Invoked from `agent::Dispatch`.

**References:**
- `crates/comrade-core/src/hooks.rs`
- `crates/comrade-core/src/agent.rs`
- `.comrade/memory/0037-tool-dispatch-prepost-hooks-per-tool-timeout-per-run-budget.md`

## LangId
> A source language the tree-sitter tools understand: Rust, JavaScript, TypeScript, Tsx, Css, Html. engine.rs maps a file extension to a LangId (`lang_of`), a LangId to its tree-sitter grammar (`grammar`), to the node kinds counted as identifier occurrences (`ident_kinds`), and to a declaration-kind -> short-label table (`decl_label`), plus `container_body` (which declarations nest others) and `decl_name` (display name). Non-Rust files are walked via `walk_sources` over `SUPPORTED_EXTS`.

**References:**
- `crates/comrade-tool-syntax/src/engine.rs`
- `.comrade/memory/0030-multi-language-tree-sitter-support-and-multi-ecosystem-polyglot-pom-detection.md`

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

## Node ecosystem (npm)
> The Node/npm build backend (Ecosystem impl `Node`, name "npm", manifest package.json) plus its parsed model `NodeModel`/`NodePackage`/`NodeScript` (node.rs): name/version/deps/scripts and `workspaces` globs resolved to subproject packages. Verbs map onto npm scripts (a script named the verb, else conventions like run->start/dev, check->typecheck); runs as `npm run <script>` (with `--prefix` for a subproject). check_command is `npx --no-install tsc --noEmit` when a tsconfig.json exists; format is prettier; parse_diagnostics reads tsc `path(line,col): error TSxxxx` lines; simplify_tests reduces jest/vitest/mocha output.

**References:**
- `crates/comrade-tool-project/src/node.rs`
- `crates/comrade-tool-project/src/ecosystem.rs`

## path completion (session prompt)
> Emacs find-file style Tab completion in the Ctrl-x C-s / Ctrl-x C-f session path minibuffer (crates/comrade-tui/src/tui.rs). KeyCode::Tab in handle_path_prompt_key runs complete_path(input, base = app root): it splits the typed path at the last `/` (split_dir_prefix) into a verbatim directory part and a partial name, reads the resulting directory (read_dir_entries; dirs carry a trailing `/`), and extends the name via complete_names — one match completes fully, several extend to their longest common prefix (longest_common_prefix, built on common_prefix from the M-x palette). `~` expands (expand_tilde); a path with no directory part is completed relative to the project root. Matches live in PathPrompt.matches and are drawn by draw_path_matches (a popup like draw_mx_list); cleared on the next edit.

**References:**
- `crates/comrade-tui/src/tui.rs (handle_path_prompt_key, complete_path, complete_names, split_dir_prefix, read_dir_entries, draw_path_matches)`

## plan_rect
> The last rendered Rect of the TUI plan panel, stored on App by draw_plan and used by handle_mouse to hit-test the mouse wheel so a scroll over the plan panel moves plan_scroll instead of the chat.

**References:**
- `crates/comrade-tui/src/tui.rs`

## plan_scroll
> Per-session vertical scroll offset (u16, in rendered rows) of the TUI plan panel, held on both App (active session) and LiveState (parked session) and swapped in App::swap_live. Adjusted by App::scroll_plan(delta); clamped to the wrapped content height in draw_plan before rendering `Paragraph::new(lines).scroll((plan_scroll, 0))`.

**References:**
- `crates/comrade-tui/src/tui.rs`

## pom_check
> Tool (comrade-tool-project) that runs `cargo check` and returns the first N compiler errors with file:line:col plus the total count - a cheap alternative to pom_run_tests for iterating on compile errors.

**References:**
- `crates/comrade-tool-project/src/lib.rs (PomCheck)`
- `crates/comrade-tool-project/src/tasks.rs (exec)`

**Notes:**
Runs with `--message-format=json`; `parse_check_json`/`format_diagnostic` parse it. Falls back to human-readable error lines when no JSON diagnostics parse. `tasks::exec` returns uncapped output so parsing sees the whole stream.

## prompt caching
> `[llm] prompt_caching` (default false): when on, `ChatRequest` sends a non-standard `cache_control: {"type":"ephemeral"}` marker on the serialized system message and the last tool definition so cache-aware (Anthropic/OpenRouter-style) providers can reuse the stable prefix. Providers that don't support it ignore the unknown field.

**References:**
- `crates/comrade-core/src/llm.rs`
- `.comrade/memory/0039-opt-in-prompt-caching-m-x-undo-command.md`

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

## Redactor
> `crates/comrade-core/src/redact.rs`: scrubs credential-looking values out of tool output before it is truncated and shown to the model/transcript. `Redactor::from_env()` harvests env vars whose NAME looks secret (TOKEN/SECRET/PASSWORD/*_KEY/*_AUTH …) with value len>=8; `with_secrets`/`none` are explicit. `redact(text)` replaces values plus inline `sk-`/`ghp_`/`AKIA`-shaped tokens (len>=16 tail) with `«redacted»`. Enabled by `[security] redact_secrets` (default true).

**References:**
- `crates/comrade-core/src/redact.rs`
- `.comrade/memory/0036-process-wide-securitypolicy-fs-confinement-shell-allowdeny-secret-redaction.md`

## resident semantic index
> The resident per-project vector index for `semantic_search` (crates/comrade-tool-memory/src/semantic.rs): `MEM_STORE`/`CODE_STORE` are process-global `Mutex<HashMap<PathBuf, Store>>` maps holding the memory and code `Store` for each project root. A search loads a store from disk at most once, reuses it across calls, and writes it back only when `refresh`/`code_refresh` report a change; on a clean repo (recorded HEAD matches and `git::dirty_files` is empty) the code file walk is skipped entirely.

**References:**
- `crates/comrade-tool-memory/src/semantic.rs`
- `.comrade/memory/0040-make-the-semantic-index-resident-load-once-write-only-on-change-skip-clean-repo-walks.md`

## retryable provider error
> A provider request failure the LLM client retries rather than surfacing: a transport error (reqwest timeout/connect/request/body, e.g. connection reset/refused) or an HTTP status in 408|425|429|500|502|503|504|529. Classified by `is_retryable`/`is_retryable_status` in crates/comrade-core/src/llm.rs; a non-success status is carried as the typed `LlmHttpError`. Retried with exponential backoff (`[llm] max_retries`, `retry_backoff_ms`); mid-stream failures after the first emitted delta are NOT retried.

**References:**
- `crates/comrade-core/src/llm.rs`
- `.comrade/memory/0029-retry-transient-provider-failures-in-the-llm-client.md`

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

## SecurityPolicy
> The process-wide filesystem + shell guardrail in `crates/comrade-tool/src/policy.rs`: `{ extra_roots: Vec<PathBuf>, shell_allow: Vec<String>, shell_deny: Vec<String> }`. Stored in a `OnceLock<RwLock<..>>`, built from `[security]` via `SecurityCfg::to_policy` and installed with `comrade_tool::set_policy` at run/startup. Read back through `comrade_tool::policy()`. `confine(root, base, path, policy)` is the symlink-safe path check (fs tools use it via `comrade-tool-fs::resolve`); `check_command(cmd, policy)` is the shell allow/deny check used by `shell`/`run_bg`.

**References:**
- `crates/comrade-tool/src/policy.rs`
- `crates/comrade-core/src/config.rs`
- `crates/comrade-tool-fs/src/lib.rs`
- `.comrade/memory/0036-process-wide-securitypolicy-fs-confinement-shell-allowdeny-secret-redaction.md`

**Notes:**
Added by ADR #36. deny wins over allow: any command containing a deny string is refused; a non-empty allow list requires a prefix match. Redaction (Redactor) is configured by the same `[security] redact_secrets` flag but lives in comrade-core.

## semantic_search
> Memory tool (comrade-tool-memory, `semantic_search`) that finds ADRs/glossary terms by MEANING using a locally run embedding model, complementing the keyword tools find_adr/find_glossary.

**References:**
- `crates/comrade-tool-memory/src/semantic.rs`
- `.comrade/memory/0022-semantic-memory-search-fastembed-model-persisted-flat-cosine-index.md`

**Notes:**
Backed by fastembed (quantized BGE-small-en-v1.5, in-process ONNX) + a flat cosine index persisted in the user cache dir and rebuilt incrementally by text hash; see ADR #22. Read-only for the loop (advisors/delegates may call it).

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

## summarise
> A tech-lead-only tool (crates/comrade-core/src/summarise.rs, `SummariseTool`) that runs ONE command whose output is expected to be large/noisy and returns a DELEGATE-WRITTEN summary of that output instead of the raw text. Takes EITHER `command` (raw shell, run behind the same policy + approval gate as `shell`: comrade_tool::check_command + ctx.confirm) OR `task` (a project task verb/alias resolved via `comrade_tool::TaskRunner`, optionally scoped with `subproject`/`ecosystem`, run WITHOUT approval like pom_run_task) - the two are mutually exclusive. Full output (uncapped) is saved to `<root>/.comrade/artifacts/<secs>-<slug>.txt` (gitignored) and its path returned, then a [[delegates]] model summarises it (one LlmClient::chat). The delegate is chosen by `model`, else auto-picked (best-fit blurb); an `approval = \"deny\"` delegate is refused. Denied for delegates.

**References:**
- `crates/comrade-core/src/summarise.rs`
- `crates/comrade-tool/src/task_runner.rs`
- `crates/comrade-tool-project/src/lib.rs (ProjectTaskRunner)`
- `crates/comrade-tui/src/main.rs (build_tools)`
- `.comrade/memory/0042-add-a-summarise-tool-that-returns-a-delegate-written-summary-of-a-commands-output.md`

**Notes:**
Auto-pick: pick_default_model scores each enabled delegate on name+llm.model+description against SUMMARISER_HINTS (summar/cheap/fast/quick/small/light/econom/budget), highest wins, config order breaks ties, falls back to the first.

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

## tool routing
> The project's convention for telling the agent which tool to use, stated as a single test: choose by what you already know. Project facts -> pom_model; past decision/term -> find_adr/find_glossary; exact text -> fs_rgrep (first choice for literal text); symbol name -> ts_find_symbol/ts_read_symbol; only the meaning -> semantic_search; nothing yet -> fs_list_files. shell is the only LAST RESORT.

**References:**
- `crates/comrade-core/prompts/tools-intro.md`
- `crates/comrade-core/prompts/working-style.md`
- `crates/comrade-core/prompts/delegate-system.md`

**Notes:**
Stated in crates/comrade-core/prompts/tools-intro.md, echoed in working-style.md step 3 and delegate-system.md step 2; reinforced by the first lines of the fs_rgrep and semantic_search ToolSpec descriptions. See ADR for the decision and why fs_rgrep is not a last resort.

## ts_test_impact
> Read-only tree-sitter tool in comrade-tool-syntax that maps the files changed since a revision (git diff) to the tests likely to cover them: a test counts as affected if it references a symbol declared in a changed file, or lives in the same crate.

**References:**
- `crates/comrade-tool-syntax/src/lib.rs (TsTestImpact)`
- `crates/comrade-tool-syntax/src/engine.rs`

**Notes:**
Heuristic, not a proof of coverage. Engine helpers: `test_functions`, `decl_names_in_text`, `identifier_tokens`. It also suggests `cargo test -p <crate>` lines.

## verify-then-commit guard
> The agent loop's monitor (agent.rs `update_verify_state` + the `git_commit` pre-check) that refuses a `git_commit` while unverified code changes exist. A change tool in CODE_CHANGES (fs_edit, fs_write_file, ts_rename, pom_format_code, shell, delegate) sets verified=false; only a successful `pom_run_tests`/`pom_run_task` whose observation contains the literal "test result: ok." sets it back to true.

**References:**
- `crates/comrade-core/src/agent.rs (CODE_CHANGES, update_verify_state, verify_guard_message)`

**Notes:**
Trap: because the loop observes the TRUNCATED tool output, a huge test run can be cut off before the "test result: ok." line, leaving the guard stuck - run a SMALL test target (e.g. one crate) so the marker survives truncation, then commit.

## verify-then-commit monitor
> Guard in the agent loop (crates/comrade-core/src/agent.rs, `update_verify_state`/`verified_after_change`) that refuses a `git_commit` until a test run has gone green since the last code change. Any tool in `CODE_CHANGES` (fs_edit, fs_write_file, ts_rename, pom_format_code, shell, delegate) sets it unverified; only a `pom_run_tests`/`pom_run_task` run that succeeded AND whose output text contains the literal `test result: ok.` sets it verified again.

**References:**
- `crates/comrade-core/src/agent.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Practical consequence: a full-suite `pom_run_tests` output is often truncated by the harness and hides the `test result: ok.` line, so it does NOT clear the guard. Running `pom_format_code` after tests re-arms the guard. To satisfy the commit guard cheaply, run a NARROW filtered test (e.g. `pom_run_tests args=<test_name>`) whose short output prints `test result: ok.`, then commit without any intervening code-changing tool.

## Waiting session
> A session whose in-flight run is paused waiting for human input (a pending ask/dialog: a tool confirmation or a question). Shown as "waiting" in BOTH places: the mode-line session-count label (e.g. "1 running, 1 waiting, 1 idle") and the Ctrl-x C-b switcher ("[waiting]"). A session is waiting iff app.dialogs holds a Dialog with that session's id; running/waiting/idle are a mutually-exclusive partition (waiting takes precedence over running). Each session's TuiUserIo is stamped with its id so PendingAsk/Dialog can be attributed (asks previously came through one shared user io). The internal local variable in session_counts_label is still named `blocked`.

**References:**
- `crates/comrade-tui/src/tui.rs (fn session_status_marker, fn session_counts_label, fn draw_session_pick, struct TuiUserIo, struct Dialog)`

## Worktree (delegate isolation)
> `crates/comrade-core/src/worktree.rs`: a detached git worktree at `<repo>/.comrade/worktrees/<id>` (`git worktree add --detach`). Created per `delegate_parallel` job with `isolate: true` so concurrent delegates cannot clobber each other's files; the job's ToolContext root points at the worktree. Kept when the job changed files (review with `git -C <path> diff`), removed when unchanged.

**References:**
- `crates/comrade-core/src/worktree.rs`
- `crates/comrade-core/src/delegate.rs`
- `.comrade/memory/0038-isolate-delegate-parallel-jobs-in-git-worktrees.md`

