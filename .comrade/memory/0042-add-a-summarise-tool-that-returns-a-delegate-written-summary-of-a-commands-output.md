# 0042 - Add a `summarise` tool that returns a delegate-written summary of a command's output
status: accepted
date: 2026-09-13
tags: tools, delegate, context, summarise
summary: New `summarise` tool: run a command and return a delegate-written summary of its output (full output saved to .comrade/artifacts/), to keep noisy output out of the tech lead's context.

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
