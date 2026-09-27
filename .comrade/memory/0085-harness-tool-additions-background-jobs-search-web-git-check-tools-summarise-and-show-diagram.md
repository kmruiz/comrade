# 0085 - Harness tool additions: background jobs, search/web/git/check tools, summarise and show_diagram
status: accepted
date: 2026-09-27
tags: tools, background-jobs, search, web, git, summarise, diagram, comrade-tool-project
summary: Tool-surface additions across the harness: detached background jobs (run_bg/bg_status/bg_tail/bg_kill) with a shared registry; expanded search/web/git/pom_check/ts_test_impact/memory tools; a delegate-written `summarise` tool that compresses command output; and `show_diagram` for structured ASCII diagrams.

## Context
The harness grew a set of tools beyond the original filesystem/syntax/memory core, each recorded as its own ADR. This rollup consolidates those tool-surface additions.

## Decision
See the merged sections below; each tool and its contract are preserved. Background jobs run as detached `bash -c` processes tracked by a shared registry (safe to start/stop from the TUI). `summarise` returns a delegate-written summary of a command's output (full output saved to `.comrade/artifacts/`). `show_diagram` renders structured ("flow") or framed ("raw") ASCII verbatim in the chat.

## Merged: Background jobs: detached processes with a shared BgHub in comrade-tool-project
## Context
Every tool call was synchronous: a slow `pom_run_tests`/`cargo build` blocked the whole turn. The agent needed to start long-running commands and keep working.

## Decision
Add `run_bg`, `bg_status`, `bg_tail`, `bg_kill` to `comrade-tool-project` (`crates/comrade-tool-project/src/bg.rs`). `BgHub` (jobs map + counter) is created once per `all()` call and shared by the four tools via `Arc`. A job spawns `bash -c <command>` in the project root with piped stdio and `kill_on_drop(true)`; two reader tasks append stdout/stderr to a bounded buffer (200 KB, trimmed from the front on a char boundary), and a monitor task awaits `child.wait()` or a per-job `CancellationToken` (on cancel it `start_kill`s then reaps). `run_bg` is approval-gated (`APPROVAL_GATED_TOOLS`) and calls `ctx.confirm`; `run_bg`/`bg_kill` are mutating for the loop tracker; all four are denied to delegates.

## Rationale
Detached tokio tasks with a CancellationToken are the idiomatic way to own a child process and still be able to kill it; keeping the tools in comrade-tool-project avoids new crate wiring while the `Arc<BgHub>` on the tool structs gives the four tools shared state.

## Alternatives considered
A separate `comrade-tool-bg` crate was rejected to avoid new workspace wiring; the `shell` tool with `&` was rejected because output could not be polled. Holding the child in a shared Mutex (blocking kill) was rejected in favour of a `CancellationToken` + monitor task.

## Scope
Covers starting, polling and killing detached shell jobs. Does not add streaming of job output into the chat as it arrives, nor persistence of jobs across sessions.

## Impact
Long jobs survive across turns and die with the app (kill_on_drop). Main/delegate/advise registries each own an independent BgHub, so jobs started by one are not visible to another. Index dirs and job state are in-memory only (no persistence across restarts).

## Merged: Expand the tool surface: search, fetch, git, check, test-impact, memory lifecycle
## Context
A survey of the tool surface found concrete gaps: literal-only grep, no way to read a fetched page, a thin git surface, no cheap compile-error view, no test-impact view, and no memory lifecycle.

## Decision
In one pass: (1) `fs_rgrep` gained `regex`, `include`/`exclude` glob lists, `context` lines and `max_lines` (regex crate). (2) `web_fetch` added to comrade-tool-web (HTML stripped to text, read-only). (3) comrade-tool-git gained `git_blame` (read-only), `git_stash`/`git_branch`/`git_checkout`, and `git_diff` rev ranges + `git_log` pickaxe/path; the three mutating tree ops ask via `ctx.confirm` and are denied to delegates. (4) `pom_check` added (parses `cargo check --message-format=json`, returns the first N errors; `tasks::exec` exposes uncapped output). (5) `ts_test_impact` added to comrade-tool-syntax (git diff + tree-sitter: a test is affected if it references a symbol declared in a changed file or lives in the same crate). (6) Memory lifecycle: `list_adr`, `merge_adr`, `stale_memory`, `rename_glossary`, `delete_glossary`. Every new tool is classified in agent.rs `READ_ONLY_TOOLS`/`MUTATING_TOOLS` and, where it must not recurse or detach work, added to `DENIED_FOR_DELEGATES`.

## Rationale
Each addition closes a gap found while surveying the surface, and each follows the repo's existing patterns (ToolSpec per tool, `all()` per crate, explicit read-only/mutating/deny classification).

## Alternatives considered
A full LSP integration (rust-analyzer) was considered better but far larger, so it is deferred; tree-sitter heuristics ship now. Putting the git additions behind approval was rejected to match the existing ungated `git_commit`.

## Scope
Covers the navigation/verification/git/memory-lifecycle tools added in this pass. Does not add LSP-grade analysis or semantic code search.

## Impact
The model sees nine more tools; the classifications keep advisors read-only and delegates from committing/switching branches/detaching jobs. `ts_test_impact` and `pom_check` are heuristics, not proofs.

## Merged: Add a `summarise` tool that returns a delegate-written summary of a command's output
## Context
The tech lead sometimes has to run a command whose output is huge and noisy (a full test log, a big diff, a verbose build). Dumping that raw text into its context is expensive and often low-value. The human asked for a \"summariser\": the tech lead asks a delegate to summarise a command's output so the bulk never enters its context. The [[delegates]] + LlmClient infrastructure already exists in comrade-core (delegate.rs, advise.rs) and is wired into the TUI registry in main.rs.

## Decision
Add a `summarise` tool in comrade-core (crates/comrade-core/src/summarise.rs). It runs ONE shell command (bash -c) behind the same policy gate (comrade_tool::check_command) and approval prompt (ctx.confirm) as the `shell` tool; captures stdout+stderr uncapped; writes the full output to <root>/.comrade/artifacts/<secs>-<slug>.txt (gitignored) and returns that path; then asks a [[delegates]] model — a per-call `model` arg defaulting to the first enabled delegate — for a concise summary via one LlmClient::chat round-trip, and returns the summary (plus command, exit code, elapsed and artifact path). An optional `focus` hint steers the summary. A delegate configured approval = \"deny\" is refused. The tool is denied for delegates (DENIED_FOR_DELEGATES) because it spawns an extra model chat, and is registered in comrade-tui build_tools beside delegate/ask_advise.

## Rationale
A dedicated, narrow tool keeps the intent explicit ("the gist matters, not the text") and avoids the tech lead having to reason about capping. Preserving the full output on disk means the summary is a filter, not a lossy sink - the tech lead can read exact detail on demand. Reusing the shell approval gate keeps the security posture identical to running the command directly.

## Alternatives considered
(a) A `summarise` mode on the existing `delegate` tool - rejected: conflates two different jobs and complicates the delegate schema. (b) A text-only summariser (the tech lead pastes output) - no help, since the bloat has already entered the context by then. (c) Always run commands auto-approved - rejected by the human as less safe; reuse the shell gate. (d) Discard the raw output - rejected: losing the detail is unacceptable when the summary is imperfect.

## Scope
Covers a new tech-lead-only tool that runs a command and delegates the summarisation. Does NOT: summarise arbitrary text the tech lead already holds, replace pom_run_tests/pom_check (which already return tight summaries), stream the summariser as a TUI sub-chat, or add a config key for a dedicated summariser model.

## Impact
The tech lead keeps a tight context on noisy commands; the raw output survives on disk for exact follow-ups. Reuses the delegate config and LlmClient. Follow-ups: stream the summariser as a delegate sub-chat in the TUI (currently a silent single call), possibly a config key to pin a dedicated summariser model, and reusing pom_run_task-style named tasks instead of only raw shell commands. Note: a delegate's `ask` approval policy is NOT prompted for (the command approval already gates the call); only `deny` is enforced.


## Note
Extension (2026-09-13). Two follow-ups landed:
(1) ANY command, not just shell: the tool takes EITHER `command` (raw shell, policy + ctx.confirm gated) OR `task` (a project task verb/alias resolved in the build ecosystem, optionally scoped with `subproject`/`ecosystem`, like pom_run_task but WITHOUT capping the output), mutually exclusive. Task execution is abstracted behind a new `comrade_tool::TaskRunner` trait (crates/comrade-tool/src/task_runner.rs, alongside `TaskRun`) implemented by `comrade-tool-project::ProjectTaskRunner`, so comrade-core keeps its stated agnosticism to concrete tool crates; the runner is injected in comrade-tui build_tools. Tasks run WITHOUT approval (they are pre-configured cargo/npm commands, like pom_run_task); only the raw `command` path prompts. Unlike pom_run_task, the test verb is not blocked (summarise is the noisy-output tool).
(2) Best delegate, not the first: when `model` is omitted, pick_default_model scores each enabled delegate on name+llm.model+description against SUMMARISER_HINTS (summar/cheap/fast/quick/small/light/econom/budget), takes the highest, breaks ties by config order, and falls back to the first. The signal is the delegate `description` (the config's stated purpose for the model, the same text the tech lead reads to pick one). There is no cost/price field on LlmCfg, so no other signal exists.

## Merged: `show_diagram`: structured ASCII diagrams, rendered verbatim in the chat
## Context
Models answer "explain this flow/concept" requests far better with a picture, but a model drawing ASCII by hand misaligns boxes as soon as labels differ in length. The chat also word-wrapped every tool-result line, which shreds any ASCII art wider than the pane.

## Decision
Add a `show_diagram` tool to comrade-tool-session (crates/comrade-tool-session/src/diagram.rs) that renders a STRUCTURED description deterministically: kind="flow" lays out a sequence of steps as boxes joined by arrows (horizontal, auto-falling back to vertical when the row exceeds `width`, default 100); kind="raw" frames ASCII the model supplies. The tool wraps its result between two sentinel lines, `--- diagram (ascii) ---` and `--- end diagram ---`. The TUI's `result_rows` (crates/comrade-tui/src/tui.rs) treats a diagram block specially: one chat row per source line, never re-wrapped (ratatui clips an over-wide span), so alignment survives. The tool is advertised to the model by a new bullet in crates/comrade-core/prompts/tools-intro.md.

## Rationale
A structured input plus a deterministic renderer is the only way to guarantee alignment; a sentinel-delimited block is the least invasive way to teach the existing chat renderer to keep it verbatim, and keeping the tool in the session crate matches its no-side-effects nature (like ask_form).

## Alternatives considered
(a) Prompt-only guidance telling the model to draw ASCII by hand — rejected: a model cannot reliably keep boxes aligned as label lengths change. (b) A general graph/tree layout engine over nodes+edges — rejected for now as more layout code and more failure modes than the common pipeline/process case needs. (c) A separate crate comrade-tool-diagram — rejected: nothing here touches the repository or the outside world; it belongs with the session/UI tools.

## Scope
Covers the tool, its two kinds (flow/raw), the verbatim chat rendering and the model-facing guidance. Does NOT cover general graph/tree layout, exporting diagrams to files, or showing diagrams in a dedicated TUI pane.

## Impact
Models can now emit a correct, aligned diagram with one tool call. The tool is not approval-gated (it only formats text). Adding a tree/graph kind later means extending `render_flow`'s neighbours, not the wire format. The wire format is a CONTRACT shared by the tool and the TUI: changing the sentinel strings means changing both diagram::DIAGRAM_OPEN/DIAGRAM_CLOSE and tui::DIAGRAM_OPEN/DIAGRAM_CLOSE. `show_diagram` is NOT in the delegate registry's reduced CORE toolbox.

