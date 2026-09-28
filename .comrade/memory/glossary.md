# Project glossary

Project keywords and their meaning, with references to the code or documentation where they appear. One `## term` section per keyword, sorted alphabetically. Look terms up with read_glossary, search with find_glossary, add or update with record_glossary.

## .comrade.toml
> Project-level configuration file at the project root (the `--dir` directory, else the cwd), layered on top of the user config by `Config::load_layered`. Tables merge per key (project wins); `[[delegates]]` and `[[mcp.servers]]` merge by `name` (a project entry with the same name replaces the user's in place, new names append); other arrays replace wholesale. `LoadedConfig.repo_source` reports the file if one was found.

**References:**
- `crates/comrade-core/src/config.rs (PROJECT_CONFIG_FILE, Config::load_layered, merge_toml_values)`
- `crates/comrade-core/src/config/tests.rs`
- `crates/comrade-tui/src/main.rs (build_deps)`
- `crates/comrade-tui/src/tui.rs (reload_config)`
- `.comrade/memory/0051-layer-a-project-level-comradetoml-over-the-user-config.md`

**Notes:**
Merged as `toml::Value` trees BEFORE deserializing into `Config`, so key presence (e.g. an explicit `llm.base_url`) survives the merge and `apply_provider` stays correct. Because it comes from the repo it can also set `[security]`/`[hooks]` — treat as trusted input. See ADR #0051.

## ADR (when to record)
> An ADR (.comrade/memory/NNNN-*.md) is a durable architectural/design GUIDELINE that the agent, other developers and DELEGATES must follow - not a task log. Record one with record_adr ONLY for an important decision with lasting consequences, written to be read by someone implementing a feature (decision, rationale, alternatives, scope, impact). Do NOT record task notes, session logs, bugfixes, refactors, "we added X", how-tos or one-off choices; ask "would another developer or delegate need this as a RULE?" first. The lead reads relevant ADRs before planning and cites their ids in a delegate's step `context` (a delegate has `find_adr`/`read_adr` and must follow any ADR the Context names). Guidance lives in the prompts: crates/comrade-core/prompts/memory.md, crates/comrade-core/prompts/tools-intro.md, crates/comrade-core/prompts/working-style.md, crates/comrade-core/prompts/delegate-by-default.md, crates/comrade-core/prompts/delegate-system.md.

**References:**
- `crates/comrade-core/prompts/memory.md`
- `crates/comrade-core/prompts/delegate-by-default.md`
- `.comrade/memory/0082-adrs-are-durable-architectural-guidelines-not-task-logs.md`

## ADR rollup
> A canonical ADR that has absorbed several closely-related decisions about one feature or thread, so the memory holds one entry per topic instead of many fragments. Produced by merge_adr: each source's full body is appended to the target under a `## Merged: <title>` heading and the SOURCE IS REMOVED (its body now lives in the target), so the memory cannot silt up with superseded fragments - a merged id no longer resolves, so de-number any references to it when you merge. Read the canonical entry; the merged bodies are intact under the `## Merged:` headings. Example rollups: #0022 (semantic_search), #0001 (delegation/approval model), #0020 (context compaction), #0047 (approval-gate simplifications), #0030 (polyglot POM), #0033 (pom_run_tests output), #0045 (release), #0013 (sessions), #0083 (proactive sensors), #0084 (LLM client), #0085 (harness tool additions).

**References:**
- `.comrade/memory/0022-semantic-memory-search-fastembed-model-persisted-flat-cosine-index.md`
- `.comrade/memory/0020-user-triggered-context-compaction-m-c-replaces-the-history-with-a-model-summary.md`
- `.comrade/memory/0047-remove-the-justification-argument-from-the-approval-gate.md`
- `.comrade/memory/0030-multi-language-tree-sitter-support-and-multi-ecosystem-polyglot-pom-detection.md`

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

## Anthropic OpenAI-compatibility layer
> Anthropic's official OpenAI-compatibility layer at https://api.anthropic.com/v1 — speaks /chat/completions with Bearer auth, so Claude models work through the existing OpenAI-compatible LlmClient. Selected with `[llm] provider = "anthropic"` (alias `claude`) + `api_key = "sk-ant-..."`. Caveats: no prompt caching (cache_control ignored), tool schema not guaranteed (strict ignored), system/developer messages hoisted/concatenated, temperature capped at 1.

**References:**
- `crates/comrade-core/src/config.rs (provider_base_url)`
- `crates/comrade-core/src/llm/context_window.rs (heuristic_context)`
- `.comrade/memory/0084-llm-client-provider-presets-prompt-caching-and-the-m-x-undo-command.md`

## approval ([[delegates]])
> Per-[[delegates]] config key (crates/comrade-core/src/config.rs DelegateCfg.approval) controlling whether delegate/ask_advise may run that model without asking: "auto" (default) runs directly, "ask" pauses via ToolContext::confirm (skipped when the context is auto-approved), "deny" refuses to run that model through delegate/ask_advise at all. Enforced by delegate::enforce_approval inside DelegateTool::invoke and AskAdviseTool::invoke before a run starts (and before a delegated plan step is marked working).

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/delegate.rs (enforce_approval, cfg_line)`
- `crates/comrade-core/src/advise/mod.rs`
- `.comrade/memory/0001-ask-advise-tool-read-only-advisory-consult-of-a-delegate.md`

**Notes:**
Reuses the Autonomy enum (ask/auto/deny). Delegates with ask/deny are annotated in listings via delegate::cfg_line.

## approval mode (ask/auto/edit)
> How the Comrade cockpit answers the prompts a run raises: `ask` (blue) answers nothing automatically, `auto` (orange) auto-answers both tool confirmations and ask_form questions from their recommended values, and `edit` (purple) auto-accepts confirmations but still stops at questions. Ctrl-Space cycles ask -> auto -> edit -> ask.

**References:**
- `crates/comrade-tui/src/tui.rs`

## ask_advise
> Consultations normally need no approval, but the chosen delegate's `approval` policy applies (see "approval ([[delegates]])"): `ask` pauses for human approval before the advice runs, `deny` refuses outright.

**References:**
- `crates/comrade-core/src/advise/mod.rs`
- `.comrade/memory/0001-ask-advise-tool-read-only-advisory-consult-of-a-delegate.md`

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

## ask_upwards
> ask_upwards - the session tool (only in the delegate registry) that lets a stalled sub-agent ask its parent model one specific question and get the answer back as the tool result. Backed by the UpwardAsk/Upward contract (crates/comrade-tool/src/ask.rs) exposed as SessionControl::upward(); the delegate sub-agent loop caps it at MAX_UPWARD_ASKS and then calls refuse_upward. The main agent has no parent, so it never gets the tool. Since ADR #63 the SAME channel also carries permission requests: `UpwardAsk::approve(title, detail)` returns a `Verdict` (Approved/Denied(reason)/Unavailable) and gates a delegate's destructive fs_write_file, with Unavailable meaning refuse.

**References:**
- `crates/comrade-tool-session/src/lib.rs (ask_upwards, upward_tools)`
- `crates/comrade-tool/src/ask.rs (UpwardAsk, Upward, Verdict)`
- `crates/comrade-core/src/upward.rs (ParentAsk, parse_verdict)`
- `crates/comrade-core/src/delegate.rs (MAX_UPWARD_ASKS, refuse_upward, refuse_destructive)`
- `crates/comrade-tui/src/main.rs (delegate_registry, session_bundle)`
- `.comrade/memory/0061-ask-upwards-a-capped-escalation-path-from-a-delegate-to-its-tech-lead.md`
- `.comrade/memory/0063-destructive-delegate-writes-need-the-tech-leads-permission-fail-closed.md`

**Notes:**
Delegate prompt: use it at most twice (one specific question: what you tried + the exact error), then decide yourself and continue. `ParentAsk` answers both `ask` and `approve` with a single tool-less chat call to the session's own model, so the escalation cannot recurse into another agent run; approve is bounded by APPROVAL_TIMEOUT (60s) and its reply is read by parse_verdict, which approves only a line opening with APPROVE.

## ask_user dialog
> The TUI modal rendered by `draw_dialog`. It shows a `UserPrompt::Confirm` (permission/approval of mutating tools) in yellow, or a `UserPrompt::Form` (ask_form) in cyan with an editable `FormEdit`. The old `UserPrompt::Question`/`ask_user` dialog was removed (ask_form supersedes it).

**References:**
- `crates/comrade-tui/src/tui.rs (draw_dialog)`
- `crates/comrade-tool-session/src/lib.rs (ASK_FORM_SPEC)`

**Notes:**
The dialog wraps its body to the popup's real inner width (popup = min(area.width-2, 100) wide, minus 2 border cols), sizes its height to body.len() + 4 (2 borders + input row + hint row) clamped to the terminal, and anchors the body to the TOP so the action/question is never scrolled out of view. Forms edit inline in the body (up/down field, left/right adjust, space toggle, enter submit); a confirm uses y/n, `?` asks the model about the action, esc cancels.

## background job (BgHub)
> The background-process tools `run_bg`/`bg_status`/`bg_tail`/`bg_kill` (`crates/comrade-tool-project/src/bg.rs`): they start a detached `bash -c` job and poll/kill it. One `BgHub` (jobs map + id counter) per `all()` call is shared by the four tools.

**References:**
- `crates/comrade-tool-project/src/bg.rs`
- `.comrade/memory/0085-harness-tool-additions-background-jobs-search-web-git-check-tools-summarise-and-show-diagram.md`

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

## CI workflow
> .github/workflows/ci.yml — mandatory CI job `ci` whose check-run name is "fmt + test": runs `cargo fmt --all -- --check` then `cargo test --workspace` on push to main/master and on every pull_request. Read-only (permissions: contents: read); toolchain dtolnay/rust-toolchain@stable with the rustfmt component.

**References:**
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`

**Notes:**
Branch protection on `main` (kmruiz/comrade) requires the status-check context "fmt + test" (strict off, admins bypass) — the required context is the JOB name, not the workflow name `ci`. Complements .github/workflows/release.yml (tag-driven binaries).

## code chunk
> An indexable unit of source produced by the tree-sitter chunker: for Rust, one declaration (fn/struct/enum/…, recursing into impl/mod/trait so a method is its own chunk with its container head as text context), else a 40-line window. Each chunk carries file + 1-based line (the declaration's line, or the window's first line) and kind/name, so an embedding hit is a location.

**References:**
- `crates/comrade-tool-syntax/src/chunks.rs`
- `crates/comrade-tool-memory/src/semantic/mod.rs`

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
- `.comrade/memory/0020-user-triggered-context-compaction-m-c-replaces-the-history-with-a-model-summary.md`

**Notes:**
Mid-run it is requested via `comrade_tool::CompactRequest` and honoured by `run_agent_loop` at its rest point before `enforce_budget()`; while idle the TUI runs `compact_history` in a background task. `ContextManager::compact` keeps the system message and folds the summary into the "Earlier context (compacted)" rollup. The automatic path was added in ADR #34; disable with `[context] auto_compact = false`.

## contribution templates
> GitHub contribution scaffolding under .github/: ISSUE_TEMPLATE/config.yml (blank issues disabled, security + discussions links), bug_report.yml and feature_request.yml (issue forms with labels bug/enhancement), and pull_request_template.md. The PR template requires a summary, a linked issue, the changes, how it was tested (checkboxes for `cargo fmt --all -- --check` and `cargo test --workspace`) and a checklist including updating `.comrade/memory/` (ADR/glossary) for architectural changes.

**References:**
- `.github/pull_request_template.md`
- `.github/ISSUE_TEMPLATE/bug_report.yml`
- `.github/ISSUE_TEMPLATE/feature_request.yml`
- `LICENSE`

**Notes:**
The project declares `license = "MIT OR Apache-2.0"` in Cargo.toml but only an Apache-2.0 `LICENSE` file exists (added 2026-09-13); add LICENSE-MIT or narrow the Cargo.toml license field if that matters.

## CSMV store format
> The persisted on-disk format of a `semantic_search` index. Since ADR 0040 it is a compact dependency-free binary blob: magic `CSMV`, u16 version, then length-prefixed strings and raw little-endian `f32` vectors, produced/parsed by `encode_store`/`decode_store` and written to `$XDG_CACHE_HOME/comrade/semantic/<key>.bin` (`load_store` normalises vectors on load and still reads a legacy `<key>.json` once, migrating it). Chosen over JSON (which stores each f32 as ~7 text bytes) and over bson/flate2: measured 11.6 MB JSON -> 4.1 MB binary for this repo's code index, and deflate only shaves a further ~16% because the payload is incompressible f32.

**References:**
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `.comrade/memory/0022-semantic-memory-search-fastembed-model-persisted-flat-cosine-index.md`

## delegate inactivity gate (IDLE_NUDGE)
> The delegate inactivity gate: a delegated sub-agent's budget (`[agent].delegate_timeout_secs` -> `DelegateLimits::timeout`, default 300s) measures time in which the delegate completed NOTHING - no model reply, no tool result. At that many idle seconds `run_delegate_subagent` (crates/comrade-core/src/delegate.rs) injects the crate constant `IDLE_NUDGE` (once per idle stretch, via `ContextManager::push_user_merged`); at TWICE it (600s at the default) the run stops and returns `timeout_answer(...)` ("stopped after Ns of inactivity"). Every completed model request and every tool call that returns resets the clock (`last_progress`), and `invoke_within` bounds each request/tool by the time left until the NEXT gate, so an in-flight hung call is interruptible exactly at the nudge point. `0` disables the gate.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/delegate/tests.rs`

**Notes:**
Distinct from the call-count guards STALL_NUDGE / VERIFY_NUDGE / DELEGATE_READ_NUDGE, which fire on non-progress CALLS; this one fires on idle TIME. Those call-count guards only ADVISE - the root's call-count STOP was removed (ADR #91). Unlike them, this gate does stop the delegate run. A hung tool is cut off at the gate and ANSWERED with an error tool result (so the history stays API-valid and the delegate gets another turn) rather than aborting the run. Replaced the wall-clock budget of ADR #53 (see ADR #68). Tests: crates/comrade-core/src/delegate/tests.rs `a_frozen_delegate_is_nudged_to_act`, `a_progressing_delegate_is_never_cut_off`, `a_hanging_tool_is_cut_off_and_the_delegate_recovers`.

## delegate sub-chat
> The chat rows authored by a delegate model (its tool cards and its reply), rendered indented 2 columns under a "| " rule in the delegate's agent color with a dim per-agent background band, visually nested under the parent's delegate tool call.

**References:**
- `crates/comrade-tui/src/tui.rs (subchat_model, render_row_line, draw_chat, layout_chat_rows)`
- `crates/comrade-tui/src/colors.rs`

**Notes:**
Detected per row by subchat_model(msg.author, app.cfg.delegates); drawn by render_row_line's `sub: Option<Color>` param. Folded MsgKind::Run digests keep no sub-chat styling.

## Delegate timeout
> A wall-clock budget (`[agent].delegate_timeout_secs`, default **300s / 5 minutes**; `0` = no limit) applied to every delegated sub-agent run (`delegate`, `delegate_parallel`, `ask_advise`). Enforced in `crates/comrade-core/src/delegate.rs`: each model request and tool call is bounded by the time left, and when the budget runs out the delegate returns `timeout_answer(...)` — a partial answer if it had one, else a notice that it did not finish. Prevents a slow/hung model request or hanging tool from holding the parent run open. (Originally 60s; raised to 300s on 2026-09-14 — the 60s default was too aggressive.)

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/config.rs`
- `README.md`

## delegate tool
> The single tool the tech lead uses to hand work to a developer model. It has two mutually exclusive modes: `step` (run ONE plan step on the delegate model assigned to it, with `working:` notes and up to 5 `feedback` fix rounds) and `jobs` (fan out 1..=8 ad-hoc `{model, task, context}` tasks in one call and return every reply together). There is no separate `delegate_parallel` tool any more.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/delegate/parallel.rs`
- `crates/comrade-core/prompts/delegate-by-default.md`

**Notes:**
Every `jobs` entry is ALWAYS isolated in its own git worktree under .comrade/worktrees/ - the old `isolate` flag is gone. A non-git project falls back to the shared workspace with a notice. If every job in a batch fails, the tool returns `Err`; a partial batch returns `Ok` with per-job `FAILED` lines. Delegates themselves never get this tool (it is in DENIED_FOR_DELEGATES, so no recursion).

## delegate_parallel
> `delegate_parallel` (comrade-core delegate.rs, `DelegateParallelTool`): runs up to 8 independent delegate jobs concurrently in ONE call (jobs: [{model, task, context}]) and returns each reply. Works under both native and ReAct protocols, unlike the loop's own parallel dispatch of a pure-delegate native batch.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tui/src/main.rs (build_tools)`
- `.comrade/memory/0072-merge-delegate-parallel-into-delegate-as-a-jobs-only-tool-always-isolated.md`

**Notes:**
Each job's approval policy is enforced before any run starts; it never touches the plan; it is DENIED_FOR_DELEGATES so a delegate cannot fan out recursively. See ADR #23.

## DelegateThought
> `AgentEvent::DelegateThought { model, text }` (crates/comrade-core/src/session.rs) — the per-turn reasoning text a delegated sub-agent produced, emitted by `SessionEvents::reasoning` (the defaulted `ActivityEvents::reasoning(author, text)` method) from run_delegate_subagent for a native tool-calling turn (`turn.content`) or a ReAct turn (`turn_p.thought`). The TUI turns it into `Msg::reasoning(model, text)`, a 🧠 block under the delegate's name.

**References:**
- `crates/comrade-core/src/session.rs`
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tool/src/tool.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Only fires when the model actually writes text alongside its tool call; a native tool-calling turn with empty content emits nothing, so the native {protocol} in render_subagent_system asks the delegate for one short sentence before each tool call. The resulting row is a run member: it folds into the completed stretch's digest and is rendered in focus mode (default ON).

## destructive-write approval
> The permission gate a delegated sub-agent must pass before a destructive write: `delegate::refuse_destructive` (crates/comrade-core/src/delegate.rs) inspects an `fs_write_file` call, computes `comrade_tool::removed_declarations(before, after)`, and when the rewrite would delete declarations the file already had, it asks the parent model through `UpwardAsk::approve(title, detail)` and acts on the returned `Verdict` (Approved -> write runs; Denied(reason) -> refused with the reason; Unavailable -> refused). Fail closed: no parent wired, no answer within APPROVAL_TIMEOUT (60s), or a reply whose first substantive line does not open with APPROVE all mean "refused". Non-destructive writes and file creation never ask, so the gate costs nothing until a deletion appears.

**References:**
- `crates/comrade-core/src/delegate.rs (refuse_destructive, join_or)`
- `crates/comrade-tool/src/ask.rs (Verdict, UpwardAsk::approve)`
- `crates/comrade-tool/src/decl.rs (declarations, removed_declarations)`
- `crates/comrade-core/src/upward.rs (ParentAsk::approve, parse_verdict, APPROVAL_TIMEOUT)`
- `crates/comrade-core/src/delegate/tests.rs (delegate_may_not_delete_code_without_the_tech_leads_permission)`
- `.comrade/memory/0063-destructive-delegate-writes-need-the-tech-leads-permission-fail-closed.md`

**Notes:**
The refusal is returned as the tool result and emitted as a tool_call/tool_result event pair so it is visible in the delegate's sub-chat. The lead agent is NOT gated here: its fs_write_file asks the human via ctx.confirm and, under autonomy=auto, still gets the tool's "this rewrite REMOVED X" warning. `Verdict` lives in comrade-tool (ask.rs) next to UpwardAsk, so the default impl fails closed for any parent that does not implement approve.

## detect_all / pick (ecosystem selection)
> A repository may host several build systems (a Cargo.toml AND a package.json). ecosystem::detect_all(root) returns every present backend in priority order (Cargo, then npm); ecosystem::pick(root, ecosystem, verb) selects one: an explicit `ecosystem` name, else the sole backend, else the single backend whose `supports(root, verb)` is true, else an error asking for an explicit choice. `detect` (single) prefers Cargo. pom_model renders ALL detected ecosystems; pom_run_task/pom_run_tests/pom_check/pom_format_code take an `ecosystem` arg.

**References:**
- `crates/comrade-tool-project/src/ecosystem/mod.rs`
- `crates/comrade-tool-project/src/node.rs`

## diagram block sentinel
> The two lines `--- diagram (ascii) ---` and `--- end diagram ---` that `show_diagram` wraps around its output. `result_rows` in crates/comrade-tui/src/tui.rs keys off them (constants `DIAGRAM_OPEN`/`DIAGRAM_CLOSE`) to render every line of the block as one chat row, verbatim, never re-wrapped. Shared contract: the same strings exist in crates/comrade-tool-session/src/diagram.rs — change both together.

**References:**
- `crates/comrade-tool-session/src/diagram.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
The sentinel is also how focus mode recognises a diagram to keep: `diagram_block(msg)` in crates/comrade-tui/src/tui.rs returns a tool card's result when it contains `DIAGRAM_OPEN`, so `focus_visible` keeps that row (and a folded Run holding one) and `layout_diagram` draws it with no card chrome. Keying on the block, not the tool name, means any tool embedding a diagram block is covered. See ADR #16.

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
> The trait (crates/comrade-tool-project/src/ecosystem/mod.rs) backing the `pom_*` tools. A backend knows its manifest, the project model, how to map a logical verb to a command, the check command, and how to parse diagnostics/test output. `detect(root)` returns `Box<dyn Ecosystem>` (Cargo today, keyed on `Cargo.toml`).

**References:**
- `crates/comrade-tool-project/src/ecosystem/mod.rs`
- `crates/comrade-tool-project/src/lib.rs`
- `.comrade/memory/0030-multi-language-tree-sitter-support-and-multi-ecosystem-polyglot-pom-detection.md`

**Notes:**
Defaults let a minimal backend implement only model/resolve/format_command: `check_command` defaults to None, `parse_diagnostics` to the generic error-line scan, `simplify_tests` to passthrough, `is_test_command` to false. Adding npm/Maven/Go = implement the trait + one arm in `detect`. See ADR #26.

## embedded embedding model
> The int8-quantized BGE-small-en-v1.5 ONNX bundle compiled into the comrade-tool-memory binary so semantic_search runs fully offline (no download, no model cache). Raw files live in crates/comrade-tool-memory/assets/bge-small-en-v1.5-int8/ (source of truth); build.rs deflates them with flate2 into OUT_DIR/assets/*.deflate and crates/comrade-tool-memory/src/semantic/mod.rs embeds those compressed copies (include_bytes!) and inflates them in memory on first use via flate2::DeflateDecoder (inflate()/FastEmbedder). Built with fastembed's try_new_from_user_defined + Pooling::Cls. NOTE: this flate2/deflate machinery is the "compression we already have" — the semantic index store deliberately does NOT use it (see CSMV store format).

**References:**
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `crates/comrade-tool-memory/build.rs`

## embedding batch padding
> ONNX Runtime pads every sequence in a batch to the longest one, so a batch of mixed-length texts costs batch_size × max_length forward passes. On the semantic-search cold index build this padding was ~3x the real content, which is why `embed_ordered` sorts texts by length before embedding and restores the caller's order afterwards.

**References:**
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `crates/comrade-tool-memory/src/semantic/tests.rs`

**Notes:**
Why a larger batch size makes the cold build slower, not faster. Measured on this repo: 2340 code chunks, real content 1.87M chars but padded to 5.76M at batch 16 in file order; length-sorted it is ~1.93M. See ADR #22.

## enabled (delegate)
> Per-[[delegates]] boolean (default true). `enabled = false` keeps the entry in config.toml but removes the model from the `delegate`/`ask_advise` targets, their `model` enum and advertised listing (and from the TUI Ctrl-A picker), so it cannot be delegated to; it still shows dimmed with ` (disabled)` in the model panel. Unlike `approval = "deny"` (listed but refused at run time), a disabled delegate is invisible to the tech lead.

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tui/src/tui.rs`

## evaluate_questions (requirements filter)
> The lead-only tool `evaluate_questions` (crates/comrade-core/src/requirements.rs, registered in comrade-tui only when Jev is configured) that filters the lead's CLARIFYING questions before it asks the user. Input `{ request, questions }` (questions is an array, a lone string is also accepted); it sends `state = { user_request }` and ONE Jev `noul` per candidate question - "is this a GOOD clarifying question to ask the user?" - whose criteria demand a FEATURE-level question and allow a technical one only for a big architectural change. The result is DATA: each question with its probability and an `ACCEPTED`/`REJECTED` label (accept at `>= 0.60`), never an imperative. The workflow is in the conditional prompt section `crates/comrade-core/prompts/requirements.md` (included by react::build_system_prompt only when the tool is advertised), pointed at from tools-intro.md and working-style.md step 2: for a feature or bug, consult a delegate with `ask_advise`, draft feature-level questions, filter them with `evaluate_questions`, and ask the user the accepted ones with `ask_form` - every field carrying a `recommended` value and a rationale - before planning.

**References:**
- `crates/comrade-core/src/requirements.rs`
- `crates/comrade-core/prompts/requirements.md`
- `crates/comrade-tui/src/main.rs`
- `.comrade/memory/0086-gather-requirements-before-planning-delegate-drafted-jev-filtered-questions-answered-by-the-user-with-suggestions.md`

## fixed-footprint overflow
> A context-window overflow in which the FIXED part of a request - the sub-agent's system prompt plus the schemas of the tools advertised to it - already exceeds the model's window before any work has accumulated. Distinguished from a variable-history overflow, where what the agent accumulated (tool output, turns) is what grew past the window. Only the latter can be fixed by summarising; summarising an empty history still overflows.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-tool/src/ask.rs`
- `crates/comrade-core/src/upward.rs`
- `.comrade/memory/0006-restyle-all-model-facing-prompts-as-terse-runbooks-for-small-model-tech-leads.md`

**Notes:**
Detected in the delegate loop by `history_is_material` (crates/comrade-core/src/delegate.rs) returning false - i.e. no assistant turn exists yet - at the moment `is_context_overflow` matches. Such an overflow must NOT spend one of the MAX_DELEGATE_COMPACTIONS recoveries; it is answered instead by one retry with a slimmer advertised schema set (SLIM_SPEC_DROP) in native mode. This is the case that killed every delegate job in the 2026-09-16 session and the ministral-3-3b delegate in ADR 6. Diagnostic clue: the run fails on its FIRST request, before any tool call.

## flat tool schema
> Flat tool schema - the required shape of a model-facing ToolSpec.json_schema: ONE object shape with every mandatory field listed in the top-level `required` (plus `additionalProperties: false`), and no `anyOf`/`oneOf`/`allOf` over required-subset branches. A local OpenAI-compatible server (LM Studio) compiles the schema into a generation grammar: an `anyOf` made ministral-3-3b emit ~60 empty-argument calls per request and a `oneOf` silently dropped the alternative-required `index`. Alternatives are expressed instead as optional properties plus runtime validation (fs_edit advertises literal mode only and keeps `diff` as an internal runtime argument; self_update_plan/self_set_step_model/self_set_step_context require `index` with `text` as a runtime fallback).

**References:**
- `crates/comrade-tool-fs/src/lib.rs (FS_EDIT_SPEC, fs_edit_schema_is_flat_literal_only)`
- `crates/comrade-tool-session/src/lib.rs (SELF_UPDATE_PLAN_SPEC etc.)`
- `crates/comrade-tool-session/src/tests.rs (plan_step_tools_advertise_a_flat_step_selector)`
- `.comrade/memory/0057-tool-schemas-stay-flat-no-anyofoneof-a-model-must-choose-from.md`

**Notes:**
Known remaining exceptions (measured harmless, flattened only if they misbehave): amend_adr's `anyOf` over status|note, and ask_form's nested `options` `anyOf` (string | {label,diff}) - interactive-only, off the small-model path.

## focus mode
> A chat view filter in the TUI, toggled by M-f or M-x focus-mode (same chord turns it off). It is ON by default at startup (build_app sets focus_mode: true). When on it hides tool noise so the chat reads as pure conversation: it keeps MsgKind::User, MsgKind::Assistant, MsgKind::Delegate (advisories) and MsgKind::Reasoning, and drops MsgKind::Tool, MsgKind::Failure, MsgKind::Meta and the folded MsgKind::Run digest's summary row — but a folded Run still renders the Reasoning children inside it. While a run is in flight in focus mode, the bottom chat row shows a rotating activity spinner (activity_line) so a silent tool run never looks frozen. The mode line (bottom status bar) shows "focus mode enabled" (bold green) when on and "focus mode disabled" (dim gray) when off, right after the auto/ask token. Because the view hides tool cards, the agent is ALSO told to narrate: the system prompt always carries the "Keeping the user informed" section (crates/comrade-core/prompts/progress.md) asking for one short sentence per batch of tool calls, which reaches the user as a MsgKind::Reasoning block — see ADR 0089.

**References:**
- `crates/comrade-tui/src/tui.rs`
- `crates/comrade-core/prompts/progress.md`
- `crates/comrade-core/src/react.rs`

## follow_plan
> Per-session flag (App + LiveState, default true) controlling plan-panel autofollow: while true, draw_plan scrolls the plan so the active step (first InProgress, else first Pending/Ready; bottom when nothing is actionable) stays visible. Any manual scroll (App::scroll_plan from the wheel / PageUp / PageDown / M-x scroll-plan-*) sets it false; M-x toggle-plan-follow flips it.

**References:**
- `crates/comrade-tui/src/tui.rs`

## FormSpec
> JSON-described interactive form: { title, description, fields: [FormField] }, where FormField = { id, label, kind: FieldKind, required, default } and FieldKind is serde-tagged by `kind` (text|number|date|select|checkbox) with per-kind params (placeholder; min/max/step; options). Helpers: initial_values(), is_complete(), answer_lines() emit `id = value`.

**References:**
- `crates/comrade-tool/src/form.rs`

## guardrail mechanism
> The judge consulted while a delegated sub-agent is still RUNNING (every `[agent].delegate_supervise_secs`, default 60s) to decide whether the lead should `continue` (leave it alone), `steer` (write a correction for it) or `stop` (end the run and report the reason). The seam is `comrade_tool::Guardrail` + `GuardOutcome` + `GuardInput`/`GuardMessage` (crates/comrade-tool/src/ask.rs); the session carries it via `SessionControl::guardrail()` / `AgentSession::set_guardrail`. The implementation is `JevGuardrail` (crates/comrade-core/src/guardrails.rs): it sends structured `state` (`delegated_task` = the parent's own `{task, context}`, plus the last ~20 `recent_conversation` entries as `{role, content}`, each message capped at 20,000 chars) to TypeSafe's `POST https://api.typesafe.ai/v1/systemone` with FIVE typed `noul` questions and maps the probabilities to a verdict ITSELF via the pure `decide`. Configured under `[guardrails]` (`jev_api_key`, `jev_url`, `jev_model`, `jev_timeout_secs`); `guardrail_from_cfg` builds it (None without a key). Applied at the delegate loop's rest point in crates/comrade-core/src/delegate.rs.

**References:**
- `crates/comrade-core/src/guardrails.rs`
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/upward.rs`
- `crates/comrade-tool/src/ask.rs`
- `crates/comrade-core/src/config.rs`
- `.comrade/memory/0079-the-guardrail-mechanism-uses-jev-to-continue-steer-or-stop-a-running-delegate.md`

**Notes:**
The five diagnostics are `on_task`, `looping`, `needs_context`, `blocked`, `making_progress` (each a `noul` yes/no probability). COMRADE decides, not Jev: LOOP if `looping >= 0.8`; else STOP if `blocked >= 0.8`; else STEER if `needs_context >= 0.7` (hint: supply the context), `on_task <= 0.3` (drift), or `making_progress <= 0.3` (stall); else CONTINUE. Missing answers default benign (on task, progressing, not looping) so a partial/empty response fails open to `continue`; thresholds are constants in guardrails.rs. Jev returns probabilities, not prose, so on `steer` the lead MODEL writes the correction via the historical `UpwardAsk::supervise` path with the diagnostic hint appended to the briefing; `continue` costs no model call. Jev is first, model is the fallback: no key, a disabled guardrail, an error or a timeout (`invoke_within`) makes `guardrail_check` return None and the loop supervises with the model as before. `stop` returns `guardrail_stop_answer` to the lead. A LOOP is special: `loop_recovery` (delegate.rs) sends the lead a status (reason + the delegate's `task`/`context` + transcript tail), calls `UpwardAsk::recover` to pick `Recovery::{SelfWork, Split, Restart(context)}`, SHOWS the decision in the chat via `ActivityEvents::notice` ("stuck in a loop - lead will …", SessionEvents -> AgentEvent::Notice), and returns `recovery_answer` so the lead takes the task over, splits it and re-delegates, or re-delegates with the improved context. Budget unchanged: a guardrail round and `recover_context` share `MAX_DELEGATE_INTERVENTIONS = 5` parent calls per run. `Duration::ZERO` / `delegate_supervise_secs = 0` disables the mechanism; comrade-tui passes ZERO for `ask_advise`. The SAME guardrail also advises the ROOT agent (`root_guard` in crates/comrade-core/src/agent.rs), ADVISORY ONLY (never refuses a tool or stops the run): consulted on a periodic tick (`[guardrails].interval_secs`, default 60) and when the root read guard fires, its diagnosis replaces the canned stall/verify/loop-refusal guidance and is delivered as `AgentEvent::Notice("guardrail: …")` + a harness note. With a guardrail configured, the consecutive-read threshold now ADVISES and lets the read run (the agent steers itself); with no guardrail the old hard read refusal and canned nudges stand. Tests: guardrails.rs unit tests, upward.rs `parse_recovery_reads_the_three_decisions`, agent.rs `root_guard_delivers_advice_and_falls_back`, and the delegate tests `guardrail_continue_skips_the_lead_model`, `guardrail_steer_asks_the_lead_to_write_the_correction`, `guardrail_stop_ends_the_run_and_reports_to_the_lead`, `guardrail_loop_asks_the_lead_to_recover_and_shows_it`.

## harness note (steering channel)
> The trusted channel for harness-authored steering. `ContextManager::push_note` (crates/comrade-core/src/context.rs) queues a note; `request_messages` renders queued notes into the SYSTEM message under a `## Harness notes` header (one-shot, cleared with `clear_notes`) and the history itself is untouched. Every harness nudge, correction and human steer goes through it: `STALL_NUDGE`/`VERIFY_NUDGE`/`PLAN_FIRST_NUDGE` (agent.rs), `LOOP_REFUSAL_NUDGE` and `read_guard_message`, the delegate `IDLE_NUDGE`/`VERIFY_NUDGE`/`STALL_NUDGE` and `SUPERVISE_PREFIX` corrections (delegate.rs), and messages drained from the human `Steer` bus. The UI is told via `AgentEvent::Notice("loop guard: …")`, never a fake `ToolResult`. `ContextManager::push_user_merged` still exists for genuine user turns but is no longer used for harness steering.

**References:**
- `crates/comrade-core/src/context.rs`
- `crates/comrade-core/src/agent.rs`
- `crates/comrade-core/src/delegate.rs`

**Notes:**
Replaced the earlier `push_user_merged` delivery, which folded the nudge `\n\n` onto the trailing tool result (or ReAct observation). That made a benign loop guard look exactly like a prompt-injection planted in tool output — a model using this build flagged it as such. Delivering in the system message keeps the directive trusted, never disturbs the user/assistant/tool role alternation (the reason `push_user_merged` existed), and leaves tool results as pure data. ADR 0078 records the decision; ADR 0059's merge rule is superseded for harness notes (it still applies to genuine user text). Tests: `harness_notes_ride_the_system_message_and_are_transient` (context.rs), plus the delegate read-guard/idle/supervision tests.

## Hooks (pre/post-tool)
> `crates/comrade-core/src/hooks.rs`: user-configured shell commands run around a tool call. Configured as `[[hooks.pre_tool]]` / `[[hooks.post_tool]]` with `on` (match: `*`, exact name, or `name*` prefix) and `run` (bash -c). Exposes COMRADE_TOOL, COMRADE_ARGS, and (post only) COMRADE_OK. A failing pre-hook aborts the tool; a failing post-hook warns. Invoked from `agent::Dispatch`.

**References:**
- `crates/comrade-core/src/hooks.rs`
- `crates/comrade-core/src/agent.rs`
- `.comrade/memory/0037-tool-dispatch-prepost-hooks-per-tool-timeout-per-run-budget.md`

## ImagePart
> One image attached to a user message: a display `name`, its real `ImageMime` (decided by magic bytes) and the base64 payload. Turns into `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}` in a model request. The name is a UI label only — it is not part of the wire form.

**References:**
- `crates/comrade-tool/src/attach.rs`

**Notes:**
At most `MAX_IMAGES` (5) per message, at most `MAX_IMAGE_BYTES` (8 MiB) each; an oversized image is refused, never shrunk. Build one with `ImagePart::from_bytes`, read a file with `load_image_file`, or rebuild from a request with `from_data_uri`.

## index warm-up
> The background build of the semantic index (memory + code) so the first `semantic_search` is instant. Triggered automatically by comrade-tui's main() at startup (comrade_tool_memory::warm, a detached, idempotent thread), by the `warm_semantic_index` tool for the agent, by every `semantic_search` call (idempotent), or by `comrade --warm-index` (blocking) for run_bg/bg jobs. It is incremental: an already-warm index is a no-op (~0.04s). On completion it writes a small `<key>.status` marker beside the index; that marker is what tells a later process (or a search during the build) that the persisted index is ready to serve.

**References:**
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `crates/comrade-tui/src/main.rs`

**Notes:**
Not a background-job-registry job: no jobs-panel entry, not killable via bg_kill. See the ADR 'Warm the semantic index in the background at startup' and ADR 0077. Measured cold 67.6s / incremental 2.0s / no-op 0.04s on this repo; on the much larger chatapp checkout, excluding its ignored `.venv` cut the candidate files from 15,037 to 268 and the cold build to 153s (3,819 code chunks), with a 0.09s no-op and a 6.8 MB index (was 16.3 MB when the dependency tree leaked in).

## Jev client
> The shared TypeSafe/SystemOne transport, `crates/comrade-core/src/jev.rs`: `Jev::from_cfg(&GuardrailsCfg)` builds it from `[guardrails]` (None without a key), `Jev::evaluate(state, questions)` POSTs `{ state, model, questions }` to the endpoint with `Authorization: Bearer` and returns the `answers` map, and `noul`/`score`/`top_level` read one answer's fields. `JevGuardrail` (guardrails.rs) and `validate_tests` (tdd.rs) both build on it.

**References:**
- `crates/comrade-core/src/jev.rs`
- `crates/comrade-core/src/guardrails.rs`
- `crates/comrade-core/src/tdd.rs`

## LangId
> A source language the tree-sitter tools understand: Rust, JavaScript, TypeScript, Tsx, Css, Html. engine.rs maps a file extension to a LangId (`lang_of`), a LangId to its tree-sitter grammar (`grammar`), to the node kinds counted as identifier occurrences (`ident_kinds`), and to a declaration-kind -> short-label table (`decl_label`), plus `container_body` (which declarations nest others) and `decl_name` (display name). Non-Rust files are walked via `walk_sources` over `SUPPORTED_EXTS`.

**References:**
- `crates/comrade-tool-syntax/src/engine.rs`
- `.comrade/memory/0030-multi-language-tree-sitter-support-and-multi-ecosystem-polyglot-pom-detection.md`

## LiveState
> The per-session half of the TUI App state (crates/comrade-tui/src/tui.rs struct LiveState): session Arc, ctx_base, history, run_tx, stop, run_handle, running, steer_tx, queued_prompt, run_cancelled, chat, section_collapsed, chat_epoch, chat_rows_cache, stream, ctx_tokens/budget/estimated, activity, session_file, sel, scroll_top, follow, was_at_bottom, search. A parked session stores its LiveState in its OpenSession slot (Box); App::swap_live mem::swaps these fields between the App (active session) and a LiveState.

**References:**
- `crates/comrade-tui/src/tui.rs`

## MAX_DELEGATE_INTERVENTIONS
> One shared per-run budget for every time the tech lead is called into a delegate run: `const MAX_DELEGATE_INTERVENTIONS: usize = 5` in crates/comrade-core/src/delegate.rs. A supervision round (`UpwardAsk::supervise`) and a context-overflow recovery (`recover_context`) both spend from it, so a run makes at most 5 parent model calls of either kind; when it is exhausted an overflow returns `overflow_answer` instead of summarising again.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/delegate/tests.rs`

**Notes:**
Replaced the earlier pair of separate caps (`MAX_DELEGATE_SUPERVISIONS = 5` and `MAX_DELEGATE_COMPACTIONS = 3`), which allowed up to 8 parent calls in one run - folded at the human's request. The reasoning: the cost being bounded is how many times the lead is dragged into ONE sub-agent's run, however it happens, since every round is a real parent model call. Driven in tests by `supervision_and_recovery_share_one_budget`.

## Message timestamp (stamp)
> The per-message chat timestamp in comrade-tui: `Msg::ts` holds Unix seconds (set once in `App::push_msg`), and the header row of a User/Assistant/Delegate/Reasoning block renders its label right-aligned in dim. The label comes from `fmt_stamp(ts, now)`: "now" under a minute, "{m}m" under an hour, and the local wall-clock "HH:MM" (via `hhmm_local`, chrono `Local`) once the message is at least an hour old. A message with no ts (restored from an older session file) shows no stamp.

**References:**
- `crates/comrade-tui/src/tui.rs`
- `.comrade/memory/0066-chat-messages-carry-a-timestamp-rendered-chat-style-on-the-header-row.md`

**Notes:**
The row layout cache (`ChatRowsCache`) stores a `now_min` minute bucket so the relative labels refresh as the minute rolls over. See ADR #66. Width rule (learned the hard way): the stamp's column maths must use DISPLAY cells, not chars - `Span::width()` / unicode-width - because the row's width budget comes from the terminal's cells and any wide glyph (the 🧠 reasoning tag, an emoji or CJK in a prompt) makes a char count too small, pushing the stamp past the panel's right edge. The body wrappers (`md_to_lines` et al.) still count chars, so a very wide-glyph body makes its row too wide and `right_stamp` then omits the stamp rather than spilling.

## model panel
> The right-hand panel of the TUI titled " model ", drawn by `draw_stats` (crates/comrade-tui/src/tui.rs). Two fixed inner rows now: (1) `label_line` = model name + version left, balance right-aligned; (2) `gauge_line` = context bar merged with `NN%  used/budget` (compact via `short_tokens`, k/M) plus an `est` marker when estimated. Below them: the delegate list from `delegate_panel_rows`. Panel height = `MODEL_PANEL_FIXED_ROWS (2) + 2 borders + delegate rows`.

**References:**
- `crates/comrade-tui/src/tui.rs`

**Notes:**
Redesigned to be compact: previously 3 inner rows (name / gauge / usage) plus a wasted blank row; the '(api)' suffix was dropped (api is the default; only 'est' is shown).

## module tree convention
> The convention for keeping Comrade's source files small: any file over 1000 lines is turned into a module tree. Either `<name>/mod.rs` (module docs + shared `use` block + type definitions + `mod sub; pub use sub::*;`) or the modern `<name>.rs` + `<name>/<sub>.rs` layout (Rust 2018 resolves `mod sub;` in `<name>.rs` to `<name>/<sub>.rs`, so no file has to be deleted). Each moved submodule starts with `use super::*;`. Items reached through a parent re-export must be `pub(crate)` or more visible. Test modules move to their own file and are declared `#[cfg(test)] mod tests;`.

**References:**
- `crates/comrade-core/src/advise/mod.rs`
- `crates/comrade-tool-project/src/ecosystem/mod.rs`
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `crates/comrade-tool-session/src/tests.rs`
- `crates/comrade-core/src/llm/ollama.rs`
- `.comrade/memory/0046-source-files-stay-under-1000-lines-oversized-modules-become-directory-trees.md`

**Notes:**
Private items cannot be re-exported, so a moved item used by a sibling submodule needs pub(crate) — this is the usual fix-up after a move. Keeping struct definitions in the root module preserves private-field access for descendants. Long match/table functions are deliberately kept whole.

## Node ecosystem (npm)
> The Node/npm build backend (Ecosystem impl `Node`, name "npm", manifest package.json) plus its parsed model `NodeModel`/`NodePackage`/`NodeScript` (node.rs): name/version/deps/scripts and `workspaces` globs resolved to subproject packages. Verbs map onto npm scripts (a script named the verb, else conventions like run->start/dev, check->typecheck); runs as `npm run <script>` (with `--prefix` for a subproject). check_command is `npx --no-install tsc --noEmit` when a tsconfig.json exists; format is prettier; parse_diagnostics reads tsc `path(line,col): error TSxxxx` lines; simplify_tests reduces jest/vitest/mocha output.

**References:**
- `crates/comrade-tool-project/src/node.rs`
- `crates/comrade-tool-project/src/ecosystem/mod.rs`

## ort prebuilt (dfbin)
> The prebuilt ONNX Runtime static library that `ort-sys` downloads at build time (the `download-binaries` path, enabled here through fastembed's `ort-download-binaries-native-tls` feature). It is cached per target triple under `~/.cache/ort.pyke.io/dfbin/<triple>/<sha256>/libonnxruntime.a` and is ~105 MB before the linker prunes it; the target list lives in ort-sys's own build/download/dist.tsv.

**References:**
- `crates/comrade-tool-memory/Cargo.toml`
- `.comrade/memory/0075-reject-muslstatic-linking-for-the-linux-release-binary-it-is-larger-not-smaller.md`

**Notes:**
Only `*-linux-gnu` Linux triples are published (aarch64- and x86_64-unknown-linux-gnu, plus android); there is NO musl build. When a target has no row, ort-sys's build/download/resolve.rs aborts with `no prebuilt binaries available for target {target}` and tells you to compile ONNX Runtime from source — which is why the Linux binary cannot simply be rebuilt for x86_64-unknown-linux-musl (see ADR 75).

## path completion (session prompt)
> Emacs find-file style Tab completion in the Ctrl-x C-s / Ctrl-x C-f session path minibuffer (crates/comrade-tui/src/tui.rs). KeyCode::Tab in handle_path_prompt_key runs complete_path(input, base = app root): it splits the typed path at the last `/` (split_dir_prefix) into a verbatim directory part and a partial name, reads the resulting directory (read_dir_entries; dirs carry a trailing `/`), and extends the name via complete_names — one match completes fully, several extend to their longest common prefix (longest_common_prefix, built on common_prefix from the M-x palette). `~` expands (expand_tilde); a path with no directory part is completed relative to the project root. Matches live in PathPrompt.matches and are drawn by draw_path_matches (a popup like draw_mx_list); cleared on the next edit.

**References:**
- `crates/comrade-tui/src/tui.rs (handle_path_prompt_key, complete_path, complete_names, split_dir_prefix, read_dir_entries, draw_path_matches)`

## plan reset (cleared plan)
> The retirement of a finished plan at the end of a root run: in `run_agent_with_history` (crates/comrade-core/src/agent.rs), `clear_completed_plan` replaces the session's plan with nothing when it is non-empty and EVERY step is `PlanStatus::Done`, which also drops the delegation records and the finished summary, and announces it with `AgentEvent::Notice("plan complete: cleared, so the next task plans afresh")` (a Meta chat line, hidden in focus mode). A plan with any Pending/Ready/InProgress or Blocked step is kept. This is what forces the NEXT task to plan: the loop's "plan first" guard only fires when `ctx.session.plan().is_empty()`. It happens on every exit path (including errors and user cancels) and before `AgentEvent::RunEnd`. See ADR 0092.

**References:**
- `crates/comrade-core/src/agent.rs`

**Notes:**
Don't rely on a completed plan surviving a run — after a successful task the panel reads "(no plan yet)". Not to be confused with `self_finish_plan` (which marks the remaining non-done/non-blocked steps Done and stores a summary) or with the plan-first guard itself. The rule is ADR 0092 (its file name is mangled by the memory tool, so it is cited by id).

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

## Proactive mode
> Comrade's config-driven polling of external sources (JIRA tickets, GitHub issues, queues, …). Declared as `[[sensors]]`; a change posts a notification and, depending on each sensor's `mode`, either asks the human (`ask`) or opens a new session backed by a temporary file and runs it (`auto`). Implemented in the `proactive` module (one tokio task per enabled sensor) and drained in the TUI's main loop.

**References:**
- `crates/comrade-tui/src/proactive.rs`
- `crates/comrade-core/src/config.rs`
- `README.md`

## prompt caching
> `[llm] prompt_caching` (default false): when on, `ChatRequest` sends a non-standard `cache_control: {"type":"ephemeral"}` marker on the serialized system message and the last tool definition so cache-aware (Anthropic/OpenRouter-style) providers can reuse the stable prefix. Providers that don't support it ignore the unknown field.

**References:**
- `crates/comrade-core/src/llm.rs`
- `.comrade/memory/0084-llm-client-provider-presets-prompt-caching-and-the-m-x-undo-command.md`

## provider preset
> A named provider in `LlmCfg.provider` (ollama, openai, deepseek, mistral, anthropic, openrouter, groq, together) that resolves to a preset base URL via `provider_base_url()` when the config omits an explicit `base_url`. All providers are spoken to through the single OpenAI-compatible `LlmClient` (Bearer auth, `/chat/completions` with native tool calls).

**References:**
- `crates/comrade-core/src/config.rs (provider_base_url ~433, fill_provider_base_url ~479)`
- `crates/comrade-core/src/llm.rs (LlmClient ~350)`
- `crates/comrade-core/src/llm/context_window.rs (heuristic_context)`

**Notes:**
Mistral (https://api.mistral.ai/v1) is fully OpenAI-compatible: Bearer auth, /chat/completions, and `GET /models` advertising `max_context_length` (already parsed by model_context_from_openai). `anthropic` (alias `claude` -> https://api.anthropic.com/v1) uses Anthropic's OpenAI-compatibility layer; see the "Anthropic OpenAI-compatibility layer" entry. `heuristic_context` adds name-based fallbacks: 128K for mistral-*/devstral/pixtral/ministral/magistral, 32K for codestral, 200K for claude-*. Delegate entries resolve their own provider the same way.

## rank_alternatives (alternative ranking)
> The lead-only tool `rank_alternatives` (crates/comrade-core/src/alternatives.rs, registered in comrade-tui only when Jev is configured) that challenges an approach before the lead commits. Input `{ request, alternatives }` (2-4 options, a lone string also accepted); it sends `state = { user_request, alternatives }` and ONE Jev `choice` question - "which single alternative is best?" (soundness, then simplicity and cost) - and returns the full probability distribution as DATA, best first (the `jev::probabilities` helper; Jev's single `choice` is put first if no distribution comes back). The `## Challenge the approach` prompt tells the lead to present the TOP 3 to the user with its reasoning via `ask_form`, then treat the user's decision as FINAL.

**References:**
- `crates/comrade-core/src/alternatives.rs`
- `crates/comrade-core/prompts/challenge.md`
- `.comrade/memory/0087-challenge-the-approach-rank-alternatives-with-jev-then-the-user-decides.md`

## read window
> The 1-based inclusive [start_line, end_line] (or [start,end] range) passed to the fs file readers to select lines; clamped to the file's line count, and an empty/reversed window (hi <= lo) or a start past EOF yields an "empty window" message rather than a slice panic.

**References:**
- `crates/comrade-tool-fs/src/lib.rs:251`
- `crates/comrade-tool-fs/src/lib.rs:852`
- `crates/comrade-tool-fs/src/lib.rs:23`

## readiness handshake
> ask_advise step=<id> — readiness-check mode of the ask_advise tool: consults the step's OWN delegate (read-only) about whether the step's context suffices to pick it up. Delegate closes with `VERDICT: READY` (step -> PlanStatus::Ready) or `VERDICT: NEEDS_MORE: <requests>` (step stays pending, note "awaiting context: ..."). Fire one call per delegate step in parallel after self_set_plan.

**References:**
- `crates/comrade-core/src/advise/mod.rs (AskAdviseTool::invoke)`
- `crates/comrade-core/prompts/advise-system.md`
- `crates/comrade-core/src/delegate.rs (DENIED_FOR_DELEGATES)`

**Notes:**
Mutually exclusive with model/question/context args. The delegate tool description, delegation-lead.md, delegate-by-default.md and advise-system.md all instruct this handshake. Complemented by self_set_step_context to enrich a step and re-ask.

## ready (PlanStatus::Ready)
> PlanStatus::Ready ("ready") — a plan step whose assigned delegate has confirmed via ask_advise step=<id> that the step's context (goal/verification/context) is sufficient for it to do the work. Sits between Pending and InProgress (lifecycle pending -> ready -> in_progress). Soft gate: informational; the delegate tool still runs from pending.

**References:**
- `crates/comrade-tool/src/plan.rs (PlanStatus enum)`
- `crates/comrade-core/src/advise/mod.rs (readiness_verdict, step-mode invoke)`
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

## release workflow
> .github/workflows/release.yml — GitHub Actions workflow triggered on push of a ref named v[0-9]* (tag or branch). `build` matrix: ubuntu-latest/macos-latest/windows-latest each run `cargo build --release --bin comrade` and upload `comrade-<ref>-<platform>.tar.gz|.zip`; `release` (needs: build, contents: write) checks out with fetch-depth: 0 + fetch-tags: true, downloads the artifacts, renders the release body with `bash .github/scripts/release-notes.sh <ref> > $RUNNER_TEMP/notes.md` and runs `gh release create <ref> --title <ref> --target <sha> --notes-file $RUNNER_TEMP/notes.md dist/*`. The body therefore describes only what changed in that release (no --generate-notes, no static header — see ADR 0045).

**References:**
- `.github/workflows/release.yml`
- `.github/scripts/release-notes.sh`

## Release-notes commit
> The commit a release tag points at. Its subject is exactly `release: vX.Y.Z` and its **body is the GitHub release description**, written by the agent that cuts the release; `.github/scripts/release-notes.sh <tag>` extracts the body and appends the `**Full Changelog**` compare link, `release.sh` refuses to tag anything else, and the workflow's `release` job publishes it with `gh release create --notes-file`. Committed with `git commit --allow-empty -m "release: vX.Y.Z" -m "<what changed>"` right before running `./release.sh`.

**References:**
- `.github/scripts/release-notes.sh`
- `release.sh`
- `.github/workflows/release.yml`

## release.sh
> Root-level bash script that cuts a release: `./release.sh {patch|minor|major} [--no-notes]` finds the highest `vX.Y.Z` git tag (git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname, fallback v0.0.0), applies the semver bump, creates an annotated tag and pushes it to origin, which triggers the release workflow. Unless `--no-notes` is given it first requires HEAD to be the Release-notes commit for the next version (subject exactly `release: v<next>`, non-empty body) and prints the description rendered by `.github/scripts/release-notes.sh` before pushing.

**References:**
- `release.sh`
- `.github/scripts/release-notes.sh`

## RequirementTest
> A test the tech lead declared for a plan step: the tests that must pass for that step to be done. Declared with `self_set_requirement_tests` (name + file:line), stored on the `PlanStep.tests` field, and shown in the plan panel under the step.

**References:**
- `crates/comrade-tool/src/plan.rs`
- `crates/comrade-tool-session/src/lib.rs`
- `crates/comrade-tui/src/tui.rs`

## resident semantic index
> The resident per-project vector index for `semantic_search` (crates/comrade-tool-memory/src/semantic/mod.rs): `MEM_STORE`/`CODE_STORE` are process-global `Mutex<HashMap<PathBuf, Store>>` maps holding the memory and code `Store` for each project root. A search loads a store from disk at most once, reuses it across calls, and writes it back only when the CONTENT actually changed — `refresh`/`code_refresh` return `(Store, bool)` where the bool comes from `same_docs` (order-independent `(id, hash)` signatures), the per-file `FileStamp` map and the recorded HEAD, so a dirty tree that re-parses to identical chunks is NOT a change (no full-cache rewrite). Vectors are stored pre-normalised (L2) so ranking is a plain dot product (`dot`), not `cosine`.

**References:**
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `.comrade/memory/0077-semantic-search-indexes-only-git-visible-files-persists-a-readiness-marker-and-never-blocks.md`

**Notes:**
The code file SET comes from git when the project is a repo: `git::listed_files` = `git ls-files --cached --others --exclude-standard`, so an ignored dependency tree (e.g. `.venv`) is never walked; a non-git project walks with a skip list in `SKIP_DIRS`. A `Kind::Memory`/`Kind::Code` `load_if_ready` gates ranking: a search only ranks an index whose build FINISHED (resident, or a `<key>.status` marker on disk). It never builds synchronously — if nothing is ready it returns a not-ready message at once and the background `warm` does the work. `take_or_load` moves the resident store out of the map while building, so no lock is held during embedding.

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

## score_feature (feature scoring)
> The lead-only tool `score_feature` (crates/comrade-core/src/scoring.rs, registered in comrade-tui only when Jev is configured) that scores a feature BEFORE planning. Input `{ feature }`; it asks Jev three `score` questions over four levels (0-3) - `customer_value`, `technical_challenge`, `ux_challenge` - plus two `noul` risks (`architecture_risk`, `product_risk`, 0-1). The result is DATA (the numbers + level labels), no imperative. The `## Requirements` prompt turns them into decisions: a high architecture/product risk (>= 0.6) is RAISED with the user at once, a low customer value (<= 1) questions whether to build it, and a high technical/UX challenge (>= 2) means gather MORE information before planning.

**References:**
- `crates/comrade-core/src/scoring.rs`
- `crates/comrade-core/src/jev.rs`
- `.comrade/memory/0088-score-a-feature-0-3-for-value-challenge-and-risk-and-let-the-scores-drive-the-requirements.md`

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
- `crates/comrade-tool-memory/src/semantic/mod.rs`
- `.comrade/memory/0022-semantic-memory-search-fastembed-model-persisted-flat-cosine-index.md`

**Notes:**
Backed by fastembed (quantized BGE-small-en-v1.5, in-process ONNX) + a flat cosine index persisted in the user cache dir and rebuilt incrementally by text hash; see ADR #22. Read-only for the loop (advisors/delegates may call it).

## Sensor
> A `[[sensors]]` entry in the config (`.comrade.toml` or the user config): a shell command Comrade polls on `interval_secs`; when the command's stdout changes (diffed line-by-line) it emits a `SensorEvent`, and per its `mode` Comrade either notifies+asks (`ask`) or opens a session to handle it (`auto`). Fields: name, command, interval_secs (default 300, floored 10), mode, prompt, enabled.

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-tui/src/proactive.rs`
- `README.md`

## Sensor probe
> A polled source for a proactive-mode sensor. Since ADR 0054 a `[[sensors]]` entry may poll either a shell `command` (run via `bash -c`) or a registered `tool` (a built-in, a bridged MCP tool such as `mcp_jira_…`, or a `skill_…`) with optional JSON `args`; both are wrapped behind the `Probe` trait in crates/comrade-tui/src/proactive.rs and their string result is diffed by `line_delta`.

**References:**
- `crates/comrade-tui/src/proactive.rs`
- `crates/comrade-core/src/config.rs`

## Sensor session
> A session a proactive sensor opens to handle a detected change: titled `sensor: <name>` and backed by a `std::env::temp_dir()` file. Since ADR 0056 it is auto-closed (and its temp file deleted) as soon as its run finishes, so recurring sensor runs do not accumulate. Distinguished from human sessions by the `OpenSession.sensor` flag.

**References:**
- `crates/comrade-tui/src/tui.rs`

## Sensors queue
> A panel in the TUI's right column (between the model panel and the plan, drawn by draw_sensors) listing every proactive sensor request received but not yet handled, oldest/highest-priority first. The selected row is highlighted; M-x commands sensors-next/previous (selection), sensors-priority-up/down (reorder), sensors-discard (drop) and sensors-start (tackle now) manage it. `auto` entries are started automatically once the app is idle.

**References:**
- `crates/comrade-tui/src/tui.rs`
- `.comrade/memory/0083-proactive-sensors-polled-config-sensors-that-notify-act-and-queue-changes.md`

## session (TUI)
> A named unit owning its own plan and chat. In the TUI the active session's live state is the App's own fields (AgentSession plan/title, ContextManager history, Vec<Msg> chat); every other opened session is an OpenSession slot holding a Box<SessionFile> snapshot. Ctrl-x C-b switches, C-s saves, C-f loads, C-k closes (kill-session) and C-w forks; the M-x names are switch-session/save-session/load-session/kill-session/fork-session.

**References:**
- `crates/comrade-tui/src/session_store.rs`
- `crates/comrade-tui/src/tui.rs`
- `.comrade/memory/0013-tui-sessions-file-backed-switchsaveloadfork-with-ctrl-x-chords.md`

**Notes:**
SessionFile is the on-disk JSON form (version/title/status/plan/delegated/finished/chat/section_collapsed/ctx_*/history/rollup/evicted). Save/load prompt for a file path each time (find-file semantics). Ctrl-x is a prefix key handled in handle_event; PathPrompt and SessionPick are the two modals it drives. AgentSession::restore and ContextManager::from_parts rebuild the live session on load/switch/fork. new-session is non-destructive: it stashes the current session and opens a fresh empty slot (emacs scratch-buffer semantics); kill-session discards the active slot and activates a neighbour and refuses to close the only session.

## session worktree
> The per-session git worktree a TUI session runs in: `comrade/session-<id>`, checked out at `<repo>/.comrade/worktrees/session-<id>` and cut from the branch the repository has checked out at startup (auto-detected, so `main` or `master` both work). Every session gets one, so concurrent sessions (and their delegates, which nest inside it) cannot clobber the repository's own checkout: the session's `ToolContext.project_root`/`cwd`, its `MemoryUndo` root and its system prompt's working directory are all the worktree. Created/reused by `Worktree::open_on_branch` — a leftover worktree or branch from a previous run is REUSED, never destroyed, so unmerged work survives; a non-git project, a detached HEAD or a git failure falls back to sharing the repository directory. Published when the agent judges its work verified, via the approval-gated `git_merge_session` tool, which folds the target branch into the session branch (aborting any conflict inside the worktree) and only then fast-forwards the repository — see ADR 0090. Distinct from the delegate job worktree (a DETACHED worktree at `.comrade/worktrees/<numeric id>`, ADR 0072).

**References:**
- `crates/comrade-core/src/worktree.rs`
- `crates/comrade-tool-git/src/merge.rs`
- `crates/comrade-tui/src/tui.rs`
- `crates/comrade-core/prompts/session-worktree.md`
- `.comrade/memory/0090-every-session-works-in-its-own-worktree-and-merges-back-with-a-fold-then-fast-forward.md`

## show_diagram
> The session tool models call to show an ASCII-art diagram in the chat. `kind="flow"` renders a sequence of `steps` as aligned boxes joined by `-->` arrows (horizontal by default, auto-falling back to vertical when the row is wider than `width`, default 100); `kind="raw"` frames ASCII the model supplies in `ascii`. Lives in crates/comrade-tool-session/src/diagram.rs and is registered by `comrade_tool_session::all()`. It takes no `self_` prefix (like `ask_form`) because it does not change the agent's own session state.

**References:**
- `crates/comrade-tool-session/src/diagram.rs`
- `crates/comrade-tool-session/src/lib.rs`
- `crates/comrade-core/prompts/tools-intro.md`

## side-by-side diff renderer
> The aligned removed(left)/added(right) diff renderer in the TUI chat (crates/comrade-tui/src/tui.rs): extract_diff_sides pulls (removed, added) line lists, edit_diff_label builds the header, lcs_pairs aligns them (dropping unchanged lines, merging a removed+added pair into one row), and build_diff_row/cell_spans draw each row with diff_remove_bg/diff_add_bg. Handles fs_edit (args: literal old/new or embedded diff) and, since ADR #12, git_diff (parsed from the tool result).

**References:**
- `crates/comrade-tui/src/tui.rs`

## skill
> A Claude-format skill: a directory `<name>/SKILL.md` with optional YAML frontmatter (`name`, `description`) and a markdown body of instructions (possibly referencing bundled files beside it). Comrade discovers skills under `.comrade/skills/<name>/SKILL.md` (its default), `.claude/skills/<name>/SKILL.md` (project) and `~/.comrade/skills/<name>/SKILL.md` + `~/.claude/skills/<name>/SKILL.md` (personal), project dirs winning on a name clash. Each skill is exposed to the model as a tool named `skill_<name>` whose description is the skill's one-line blurb; invoking it returns the SKILL.md body (progressive disclosure).

**References:**
- `crates/comrade-tool-skill/src/lib.rs`
- `crates/comrade-tui/src/main.rs`

**Notes:**
Frontmatter is parsed by hand (no YAML crate in the workspace). Discovery/parse live in crates/comrade-tool-skill (parse_skill_md, discover_in, discover, all); the TUI registers the tools in main.rs build_tools/delegate_registry/advise_registry, which now take the project root.

## stale reference check
> How `stale_memory` decides a reference is missing (crates/comrade-tool-memory/src/store.rs): it extracts BACKTICK-QUOTED spans (`extract_paths`) that look like paths (contain `/` or end in a source/doc extension, and have no spaces/globs/placeholders), then reports those that don't exist under the project root. It ignores a token that starts with `~`, `$` or `/` (home-relative/absolute — not repo paths) and strips a trailing `:line` or `:line:col` suffix (`repo_path`), so a trailing `:line` or `:line:col` suffix is stripped before the existence check. So the memory convention is: a real file reference is written as a backticked repo-relative path; prose mentions, placeholders (`<name>.rs`) and locations outside the repo are not checked.

**References:**
- `crates/comrade-tool-memory/src/store.rs`

## stall nudge (root)
> The root agent's one-shot steering nudge: after `STALL_NUDGE_AT` (20 = `READ_GUARD_THRESHOLD`) consecutive non-progress CALLS following the first workspace change, `LoopTracker::needs_stall_nudge()` fires once (crates/comrade-core/src/agent.rs) and the caller delivers `STALL_NUDGE` as a harness note telling the model to finish. It is ADVICE ONLY — there is no longer any call-count gate that ENDS the run (the old `STALL_END_AT` = 40 stop was removed, ADR #91), so a root run is bounded only by `[agent].max_iterations` (default 30) and `run_timeout_secs`. `idle` counts calls, not turns: every non-progress call extends the run, so a batch of parallel calls can advance it several times in one turn, and it is reset by any progress call (`is_progress`).

**References:**
- `crates/comrade-core/src/agent.rs`
- `.comrade/memory/0091-no-call-count-stall-gate-a-root-run-is-bounded-by-max-iterations-not-a-spin-counter.md`

**Notes:**
A pre-first-change read-only exploration never arms it (`progress > 0` is required). Do not re-introduce a call-count stop next to it: stuckness is judged by the guardrail (ADR #91, ADR #0079).

## summarise
> A tech-lead-only tool (crates/comrade-core/src/summarise.rs, `SummariseTool`) that runs ONE command whose output is expected to be large/noisy and returns a DELEGATE-WRITTEN summary of that output instead of the raw text. Takes EITHER `command` (raw shell, run behind the same policy + approval gate as `shell`: comrade_tool::check_command + ctx.confirm) OR `task` (a project task verb/alias resolved via `comrade_tool::TaskRunner`, optionally scoped with `subproject`/`ecosystem`, run WITHOUT approval like pom_run_task) - the two are mutually exclusive. Full output (uncapped) is saved to `<root>/.comrade/artifacts/<secs>-<slug>.txt` (gitignored) and its path returned, then a [[delegates]] model summarises it (one LlmClient::chat). The delegate is chosen by `model`, else auto-picked (best-fit blurb); an `approval = \"deny\"` delegate is refused. Denied for delegates.

**References:**
- `crates/comrade-core/src/summarise.rs`
- `crates/comrade-tool/src/task_runner.rs`
- `crates/comrade-tool-project/src/lib.rs (ProjectTaskRunner)`
- `crates/comrade-tui/src/main.rs (build_tools)`
- `.comrade/memory/0085-harness-tool-additions-background-jobs-search-web-git-check-tools-summarise-and-show-diagram.md`

**Notes:**
Auto-pick: pick_default_model scores each enabled delegate on name+llm.model+description against SUMMARISER_HINTS (summar/cheap/fast/quick/small/light/econom/budget), highest wins, config order breaks ties, falls back to the first.

## TaggedEvent
> TaggedEvent = (u64, AgentEvent) (crates/comrade-tui/src/main.rs): an agent event tagged with the id of the session that produced it. Each session has its own bounded run-facing sender relayed by spawn_tagged_relay, which forwards (id, event) into the App's single central unbounded queue (App::events_rx); the select loop dispatches via App::on_agent_event_for(id, ev).

**References:**
- `crates/comrade-tui/src/main.rs`
- `crates/comrade-tui/src/tui.rs`

## tool name prefixes (fs_/ts_/pom_/self_)
> The model-facing toolset naming convention (ADR #7): every tool's name carries a domain prefix - fs_* for filesystem tools (comrade-tool-fs), ts_* for tree-sitter/code tools (comrade-tool-syntax), pom_* for project/task tools (comrade-tool-project), self_* for session/planning tools the agent runs on itself (comrade-tool-session), memory tools use the adr/glossary families (record_adr/find_adr/read_adr/amend_adr, record_glossary), and ask_user asks the human. apply_edit+apply_patch merged into fs_edit (literal old/new OR diff); find_definition+read_symbol merged into ts_read_symbol (body flag); references_count was removed. Engine helper fns keep internal names. Since ADR #57 fs_edit ADVERTISES the literal mode only - flat schema, required path+old+new, no `diff` property, because an `anyOf` made a local small model emit empty/garbage calls - while the unified-diff patch mode stays an accepted RUNTIME argument for internal callers.

**References:**
- `.comrade/memory/0007-namespaced-consistent-tool-names-fs-ts-pom-self-prefixes-merges.md`
- `.comrade/memory/0057-tool-schemas-stay-flat-no-anyofoneof-a-model-must-choose-from.md`
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

## trust rule (delegate trusts the lead)
> The rule that a delegated sub-agent takes the tech lead's task `Context` (and a plan step's goal/verification/context) as authoritative and judges only whether the information is SUFFICIENT for its step - never whether it is correct. It must not re-run the lead's searches/reads or re-load files the context already holds. Stated in crates/comrade-core/prompts/delegate-system.md ('## Trust the tech lead'), in the readiness prompt (crates/comrade-core/src/advise/tool.rs, 'Do NOT re-run the lead's reconnaissance') and in crates/comrade-core/prompts/advise-system.md. The lead owns orientation, the delegate executes, the harness owns verification (the lead re-runs the step's verification after every delegate reply).

**References:**
- `crates/comrade-core/prompts/delegate-system.md`
- `crates/comrade-core/prompts/advise-system.md`
- `crates/comrade-core/src/advise/tool.rs`
- `crates/comrade-core/prompts/delegate-by-default.md`

**Notes:**
Scoped to reconnaissance: a delegate MUST still read the exact lines it anchors fs_edit's byte-exact `old` on, and must not retype long content from memory (see the 3B measurement ADR #65, which is about writing, not about trusting facts). Motivation: a defensive delegate burned its budget re-verifying what the lead had already verified and stalled on a readiness check.

## ts_test_impact
> Read-only tree-sitter tool in comrade-tool-syntax that maps the files changed since a revision (git diff) to the tests likely to cover them: a test counts as affected if it references a symbol declared in a changed file, or lives in the same crate.

**References:**
- `crates/comrade-tool-syntax/src/lib.rs (TsTestImpact)`
- `crates/comrade-tool-syntax/src/engine.rs`

**Notes:**
Heuristic, not a proof of coverage. Engine helpers: `test_functions`, `decl_names_in_text`, `identifier_tokens`. It also suggests `cargo test -p <crate>` lines.

## two-shape tool
> A model-facing tool whose arguments come in TWO mutually exclusive call shapes rather than one flat shape, so its json_schema carries a top-level `oneOf` over required-subset branches and NO top-level `required`. Members: `delegate` (delegate.rs:332), `delegate_parallel` (uses `required: ["jobs"]` at top level, but each job object is flat), `ask_advise` (advise/tool.rs:86), `summarise` (summarise.rs:136). Contrast with a FLAT tool (fs_edit, self_set_plan, self_update_plan), which advertises every field in a single top-level `required`.

**References:**
- `crates/comrade-core/src/delegate.rs`
- `crates/comrade-core/src/advise/tool.rs`
- `crates/comrade-core/src/summarise.rs`
- `.comrade/memory/0057-tool-schemas-stay-flat-no-anyofoneof-a-model-must-choose-from.md`

**Notes:**
A prompt audit (2026-09-16) flagged the two-shape tools as violating ADR 0057, which requires flat schemas. They are NOT a defect: the two shapes cannot be expressed by any flat `required` list, the shape is pinned by tests (comrade-core/src/delegate/tests.rs:368, advise/tests.rs:233), and both `delegate step=N` and `ask_advise step=N` formed correct calls through the oneOf on the local OpenAI-compatible provider. ADR 0057 was amended to scope the flat-schema rule to tools whose fields are UNCONDITIONALLY required. Do not "fix" these by flattening them.

## UserInput
> One message from the human: `{ text, images: Vec<ImagePart> }`. It is THE type of a user prompt — the TUI submit/queue/steer path, `run_agent*`, the `Steer` channel, `AgentEvent::User` and `ContextManager::push_user_input` all speak it, so the text and its images always travel as one message.

**References:**
- `crates/comrade-tool/src/attach.rs`
- `crates/comrade-core/src/agent.rs`
- `crates/comrade-tui/src/tui.rs`

**Notes:**
`transcript()` renders what the chat shows (`[image: name]`); `downgrade_images(reason)` moves the attachments into the text as `[image not sent: …]` for a model that cannot see. `From<String>`/`From<&str>` exist, and `run_agent*` takes `impl Into<UserInput>`, so a plain string still works.

## validate_tests (TDD coverage check)
> The lead-only tool `validate_tests` (crates/comrade-core/src/tdd.rs, registered in comrade-tui only when Jev is configured) that scores how well the tests written for a feature cover it before any implementation. Input `{ feature, tests }` (tests is an array, a lone string is also accepted); it sends `state = { feature, tests }` and ONE Jev `score` question over five levels `none / sparse / partial / good / comprehensive`. ACCEPTED when the weighted `score >= 3.0` of 4 (the top two levels). The result is DATA - one line `Test coverage X/4 (label): ACCEPTED|NOT ENOUGH (threshold 3.0/4)`, no imperative - and the prompt section drives the action: ACCEPTED -> delegate the IMPLEMENTATION with the tests as the spec (they must fail first and must not be weakened) and refactor once green; NOT ENOUGH -> strengthen the tests and call again, do not implement. ADVISORY - it returns a verdict and does not gate `delegate`. The score question's instructions also weigh the TEST PYRAMID. The workflow is in the conditional prompt section `crates/comrade-core/prompts/tdd.md` (included by react::build_system_prompt only when the tool is advertised), pointed at from working-style.md steps 4-5; it also demands the LEAST code that makes the accepted tests pass, treats refactoring as ESSENTIAL, and picks test types by cost/coverage (unit cheapest/least, integration middle, functional most expensive/most) with more unit than integration and more integration than functional.

**References:**
- `crates/comrade-core/src/tdd.rs`
- `crates/comrade-core/prompts/tdd.md`
- `crates/comrade-tui/src/main.rs`
- `.comrade/memory/0080-test-first-write-tests-validate-coverage-with-jev-then-delegate-the-implementation.md`

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

## Vision (llm.vision)
> The `[llm] vision = "auto" | "on" | "off"` setting (crates/comrade-core/src/config.rs) that decides whether images may be sent with a user message. `auto` (the default) runs a model-name heuristic, `on`/`off` force it.

**References:**
- `crates/comrade-core/src/config.rs`
- `crates/comrade-core/src/agent.rs`

**Notes:**
Read it through `LlmCfg::supports_vision()` — never test the model name yourself. When it is false the image is replaced by a placeholder in the text and an `AgentEvent::Notice` names the config key. Delegates are always treated as `vision: false`.

## Waiting session
> A session whose in-flight run is paused waiting for human input (a pending ask/dialog: a tool confirmation or a question). Shown as "waiting" in BOTH places: the mode-line session-count label (e.g. "1 running, 1 waiting, 1 idle") and the Ctrl-x C-b switcher ("[waiting]"). A session is waiting iff app.dialogs holds a Dialog with that session's id; running/waiting/idle are a mutually-exclusive partition (waiting takes precedence over running). Each session's TuiUserIo is stamped with its id so PendingAsk/Dialog can be attributed (asks previously came through one shared user io). The internal local variable in session_counts_label is still named `blocked`.

**References:**
- `crates/comrade-tui/src/tui.rs (fn session_status_marker, fn session_counts_label, fn draw_session_pick, struct TuiUserIo, struct Dialog)`

## Worktree (delegate isolation)
> `crates/comrade-core/src/worktree.rs`: a detached git worktree at `<repo>/.comrade/worktrees/<id>` (`git worktree add --detach`). Created per `delegate_parallel` job with `isolate: true` so concurrent delegates cannot clobber each other's files; the job's ToolContext root points at the worktree. Kept when the job changed files (review with `git -C <path> diff`), removed when unchanged.

**References:**
- `crates/comrade-core/src/worktree.rs`
- `crates/comrade-core/src/delegate.rs`
- `.comrade/memory/0072-merge-delegate-parallel-into-delegate-as-a-jobs-only-tool-always-isolated.md`

## Worktree isolation
> Running a delegate_parallel job inside its own detached git worktree (<repo>/.comrade/worktrees/<id>) so parallel jobs cannot clobber each other's files. It is the DEFAULT for parallel jobs; pass isolate=false to share the workspace, and non-git projects fall back to sharing. Changed worktrees are kept for the tech lead to merge back with `git apply --3way`; unchanged ones are removed.

**References:**
- `crates/comrade-core/src/worktree.rs`
- `crates/comrade-core/src/delegate/parallel.rs`
- `crates/comrade-core/prompts/delegate-by-default.md`

