<div align="center">

# 🤖 Comrade

**An agentic coding cockpit in your terminal.**

A Rust TUI (plus a headless runner) that drives an LLM agent through real work in
your repository — files, git, tests, memory and MCP tools — with a live plan you
can follow while it runs.

[![CI](https://github.com/kmruiz/comrade/actions/workflows/ci.yml/badge.svg)](https://github.com/kmruiz/comrade/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/kmruiz/comrade?sort=semver)](https://github.com/kmruiz/comrade/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)

</div>

---

## ✨ Features

| | |
|---|---|
| 🧠 **Agent loop** | ReAct-style tool use with a live, scrollable plan, streaming output, reasoning blocks and a focus mode that hides tool/meta noise by default. |
| 🔌 **Models** | OpenAI-compatible clients with retry/backoff and optional prompt caching. Configure **delegates** for parallel jobs and isolated git worktrees. |
| 🌳 **Code tools** | tree-sitter powered `ts_*` tools: find/read symbol, references, rename, structural map — plus `fs_*` read / write / edit / rgrep. |
| 📦 **Project tools** | Detect and run the project's own tasks (Cargo & npm), background jobs, git operations and web search/fetch. |
| 🗂️ **Memory** | ADR decisions and a glossary under `.comrade/memory/`, with semantic search across both memory and code (the index warms in the background at startup). |
| 🧩 **Extensible** | MCP servers, Claude-format skills (`SKILL.md`), and per-tool approval policies. |

## 📖 Table of contents

- [Install](#-install)
- [Usage](#-usage)
- [Configuration](#-configuration)
- [Build from source](#-build-from-source)
- [Contributing](#-contributing)
- [Releasing](#-releasing)
- [License](#license)

## 📥 Install

Prebuilt binaries are on the [releases page](../../releases). Each command below
resolves the latest tag and installs the `comrade` binary into `/usr/local/bin`
(drop the `sudo` if that directory is writable for you).

| Platform | Archive |
|---|---|
| 🐧 Linux (x86_64) | `comrade-<tag>-linux-x86_64.tar.gz` |
| 🍎 macOS (Apple silicon) | `comrade-<tag>-macos-arm64.tar.gz` |
| 🪟 Windows (x86_64) | `comrade-<tag>-windows-x86_64.zip` |

### 🐧 Linux (x86_64)

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-linux-x86_64.tar.gz" | sudo tar -xz -C /usr/local/bin comrade
```

### 🍎 macOS (Apple silicon)

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-macos-arm64.tar.gz" | sudo tar -xz -C /usr/local/bin comrade
```

### 🪟 Windows

With Git Bash:

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-windows-x86_64.zip" -o comrade.zip
unzip -o comrade.zip -d "$LOCALAPPDATA/Programs/comrade"
```

Or with PowerShell:

```powershell
$tag = (Invoke-WebRequest -UseBasicParsing https://github.com/kmruiz/comrade/releases/latest).BaseResponse.RequestMessage.RequestUri.Segments[-1]
$dir = "$env:LOCALAPPDATA\Programs\comrade"; New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-windows-x86_64.zip" -OutFile comrade.zip
Expand-Archive comrade.zip -DestinationPath $dir -Force
```

> **Private repository?** The unauthenticated `curl` calls above return 404.
> Authenticate first — e.g. `export GH_TOKEN=$(gh auth token)` and add
> `-H "Authorization: token $GH_TOKEN"` to the `curl` calls, or use
> `gh release download kmruiz/comrade --pattern 'comrade-*-linux-x86_64.tar.gz'`.

## 🚀 Usage

```bash
comrade --version          # print the installed version
comrade                    # start the TUI in the current directory
```

Comrade keeps project memory (decisions and a glossary) in `.comrade/memory/`
inside the project and reads its configuration from a TOML file (see
[Configuration](#-configuration)). If the project root contains an `AGENTS.md`,
its text is injected into the agent's system prompt (user-supplied; this repo
does not ship one).

## ⚙️ Configuration

Configuration is a single TOML file, resolved in this order:

1. `--config <PATH>`
2. `$COMRADE_CONFIG`
3. `$XDG_CONFIG_HOME/comrade/config.toml` — default `~/.config/comrade/config.toml`

When no file is found, built-in defaults apply. Every table below is optional.

A project-level `.comrade.toml` at the project root (the `--dir` directory, else
the current directory) is layered on top; project values supersede it key by key,
while `[[delegates]]`, `[[mcp.servers]]` and `[[sensors]]` entries merge by
`name` (same name replaces the user's, new names are appended) and every other
array is replaced wholesale. Because it comes from the repository, a
`.comrade.toml` can also set `[security]` and `[hooks]` — treat it as trusted
input.

```toml
# .comrade.toml — project overrides, layered on top of the user config
[llm]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
```

### Command line

| Flag | Meaning |
|---|---|
| `comrade [PROMPT]` | Open the TUI, or run `PROMPT` as a one-shot task. |
| `--headless` | Run without the TUI, streaming the run to stdout. |
| `--config <PATH>` | Use a specific config file. |
| `--dir <PATH>` | Project root to operate in (defaults to the current directory). |
| `--auto` | Shorthand for `autonomy = "auto"` — apply changes without asking. |
| `--warm-index` | Build the semantic index for the project and exit. |
| `--version` | Print the installed version. |

### `[llm]`

The model the agent talks to.

| Key | Default | Notes |
|---|---|---|
| `base_url` | `http://localhost:11434/v1` | OpenAI-compatible endpoint. |
| `api_key` | – | Sent as `Authorization: Bearer …` when set (for `provider = "anthropic"`, a Claude API key `sk-ant-…`). |
| `model` | `devstral-small-2` | Model identifier. |
| `provider` | – | One of `ollama`, `openai`, `deepseek`, `mistral`, `anthropic` (alias `claude`), `openrouter`, `groq`, `together`; sets `base_url` unless given explicitly. |
| `temperature` | `0.2` | |
| `timeout_secs` | `600` | Response timeout. |
| `max_retries` | `2` | Retries for transient failures. |
| `retry_backoff_ms` | `500` | Base backoff delay (doubles per attempt, capped at 8s). |
| `protocol` | `auto` | `auto` \| `native` \| `react` tool-calling. |
| `context_window` | auto-detected | Model context window in tokens. |
| `prompt_caching` | `false` | Anthropic-style `cache_control` on the stable prefix. |

### `[[delegates]]`

One table per developer model the tech lead may hand sub-tasks to (the
`delegate` / `ask_advise` tools). Keys: `name`, `description`, `enabled`
(`true`), `approval` (`auto` \| `ask` \| `deny`, default `auto`), plus the inline
`[llm]` keys (`provider`, `model`, `api_key`, `temperature`, …). Every run is
bounded by `[agent].delegate_timeout_secs` (default `300`) seconds of INACTIVITY:
a delegate that completes nothing (no model reply, no tool result) is nudged to
act after that long and stopped at twice it, while any completed request or tool
call resets the clock, so a slow but working delegate is never cut off and a
stuck one can never hang the parent run. While a delegate runs, a guardrail is
consulted every `[agent].delegate_supervise_secs` (default `60`) and returns one
of `continue` (leave it), `steer` (the tech lead model writes a short
correction) or `stop` (end the run and report the reason). The guardrail is Jev
(TypeSafe) when `[guardrails].jev_api_key` is set, and the tech lead model
otherwise. Every run gets at most 5 parent interventions in total, shared with
the context-overflow recovery, so a delegate that drifts off its step — or
outgrows its context window — is steered back instead of being left to loop,
without turning into an endless conversation with the tech lead.

### `[agent]`

| Key | Default | Notes |
|---|---|---|
| `max_iterations` | `30` | Tool-use turns per run. |
| `tool_timeout_secs` | `0` | Kill a single tool after N seconds (`0` = no limit). |
| `run_timeout_secs` | `0` | Stop a whole run after N seconds (`0` = no limit). |
| `delegate_timeout_secs` | `300` | INACTIVITY budget: a delegate that completes nothing for N seconds is nudged to act and stopped at 2N; any completed request or tool result resets the clock (`0` = no limit). |
| `delegate_supervise_secs` | `60` | How often the tech lead re-reads a running delegate's transcript and may steer it back on task. One run gets at most 5 parent interventions in total, shared with context-overflow recovery (`0` = off). |

### `[context]`

| Key | Default | Notes |
|---|---|---|
| `budget_tokens` | `6000` | Rolling chat-history budget. |
| `max_tool_output_chars` | `5000` | Cap on tool output fed back to the model. |
| `auto_compact` | `true` | Summarise the history automatically when it fills the budget. |

### `[security]`

| Key | Default | Notes |
|---|---|---|
| `autonomy` | `ask` | `ask` \| `auto` \| `deny` for mutating tools. |
| `redact_secrets` | `true` | Scrub secret-looking values from tool output. |
| `extra_roots` | `[]` | Extra directories the filesystem tools may touch. |
| `shell_allow` | `[]` | If set, `shell`/`run_bg` commands must start with one of these. |
| `shell_deny` | `[]` | `shell`/`run_bg` commands containing any of these are refused. |

### `[guardrails]`

The guardrail mechanism: an external service that judges a RUNNING delegated
sub-agent and tells the tech lead whether to leave it, correct it or stop it.
When `jev_api_key` is set, Comrade sends the parent's `task` and `context` for
the delegate, plus its last ~20 messages (structured `state`, each message capped
at 20 000 chars) to Jev (TypeSafe's `POST /v1/systemone`) and asks five typed
`noul` (yes/no probability) questions about the situation — is it on task,
looping, blocked, missing context, making progress. Comrade then maps
those probabilities to an action itself. A **loop** stops the run and asks the
tech lead to choose a recovery — do the task itself, split it and delegate
again, or restart it with a better context — which is reported back and shown in
the chat; a **block** stops the run with a reason; a context gap / drift / stall
**steers** it; otherwise it **continues**. `continue` costs no model call;
`steer` makes the tech lead model write the correction; `stop` ends the run and
reports the reason back to the lead. With no key (or if the call fails or times
out) the tech lead model supervises exactly as before. The thresholds live as
named constants in `crates/comrade-core/src/guardrails.rs`.

| Key | Default | Notes |
|---|---|---|
| `enabled` | `true` | Master switch; the guardrail is only active when a key is present. |
| `jev_api_key` | – | TypeSafe/Jev API key, sent as `Authorization: Bearer …`. Setting a non-empty key turns the guardrail on. |
| `jev_url` | `https://api.typesafe.ai/v1/systemone` | Evaluation endpoint. |
| `jev_model` | `jev-latest` | Model alias to evaluate with. |
| `jev_timeout_secs` | `30` | A guardrail call slower than this falls back to the lead model (`0` = off). |
| `interval_secs` | `60` | How often the ROOT agent's guardrail is consulted as an advisory tick (`0` = only the read/stall/verify triggers). |

The same guardrail also advises the **root agent**: at the periodic tick, when
it has made many reads in a row, or when it stalls or stops verifying, Jev's
diagnosis is delivered as a chat notice (`guardrail: …`) and a harness note the
agent can act on. For the root it is **advisory only** — it never refuses a tool
or stops the run, so the agent steers itself (with no key, the previous
read-guard refusals and canned nudges apply unchanged).

```toml
[guardrails]
jev_api_key = "$TYPESAFE_API_KEY"
```

### Test-first (TDD)

The same Jev connection backs the `validate_tests` tool (advertised only when a
`jev_api_key` is set). It makes the lead work test-first: write the tests for a
feature, call `validate_tests` with `feature` and `tests`, and Jev scores how
well the tests cover the feature (a `score` question over `none / sparse /
partial / good / comprehensive`). The top two levels are **ACCEPTED** — the
lead then delegates the implementation (the tests are the spec and must not be
weakened) and refactors once they are green; below that the lead must strengthen
the tests first. It is advisory: the tool returns a verdict, it does not gate the
delegate. The prompt section `Test-first (TDD)` (included only when the tool is
available) also tells the lead to keep the implementation MINIMAL and to refactor
until the least code that passes remains, and to choose test types by cost and
coverage — **unit** (cheapest, least), **integration** (middle), **functional**
(most expensive, most) — following the test pyramid: more unit than integration,
more integration than functional.

### Requirements

Two more Jev tools (advertised only with a `jev_api_key`) make the lead challenge
a feature before it plans:

- **`score_feature`** scores the request 0-3 for **customer value**,
  **technical challenge** and **UX challenge**, plus the risk of a negative
  **architecture** or **product** impact (0-1). The `Requirements` prompt uses
  the scores to decide: raise a high architecture/product risk (≥ 0.6) with the
  user at once, question a low customer value (≤ 1), and gather more information
  when the technical/UX challenge is high (≥ 2).
- **`evaluate_questions`** filters the lead's clarifying questions: it consults a
  delegate (`ask_advise`) for clarity and questions, sends the request as `state`
  and **one `noul` per question** ("is this a good clarifying question?"), and
  the lead asks the user only the accepted **feature-level** ones with
  `ask_form`, always attaching a `recommended` answer. Technical questions score
  low unless the request implies a big architectural change.

### Challenge the approach

With `rank_alternatives` (same Jev key) the lead validates the approach before
committing: it looks for prior art and alternatives (`web_search`/`web_fetch`
and the repo), ranks them with **one Jev `choice` question** over the options,
and presents the **top 3** to the user with its reasoning via `ask_form`. The
user's decision is final — the lead does not reopen it. All three tool results
are data-only; the conditional prompt sections drive the flow, and everything is
skipped for trivial or unambiguous changes.

### `[[mcp.servers]]`

External MCP servers whose tools are bridged into the agent as
`mcp_<name>_<tool>`. Each has a `name` and a `transport`:

```toml
[[mcp.servers]]
name = "files"
transport = { type = "stdio", command = "npx", args = ["-y", "@modelcontextprotocol/server-filesystem", "."] }

[[mcp.servers]]
name = "remote"
transport = { type = "http", url = "https://mcp.example.com/mcp" }
auth = { type = "api_key", key = "$MY_MCP_KEY" }
```

`stdio` takes `command`, `args` and `env`; `http` takes `url`. `auth` is either
`{ type = "api_key", key, header? }` or an OIDC block. `$NAME`-prefixed values are
expanded from the environment.

### `[[hooks.pre_tool]]` / `[[hooks.post_tool]]`

Shell hooks run around every tool call. `on` matches `*`, an exact tool name
(`fs_edit`) or a prefix (`fs_*`); `run` is the command executed via `bash -c`. A
non-zero `pre_tool` exit aborts the call; a non-zero `post_tool` exit only warns.

### `[[sensors]]` — proactive mode

Proactive mode watches external sources (JIRA tickets, GitHub issues, a queue, …)
unprompted. Each `[[sensors]]` table polls a shell **command** or a registered
**tool** (a built-in, a bridged MCP tool, or a skill) on an interval; when the
result changes Comrade queues the request.

| Key | Default | Notes |
|---|---|---|
| `name` | – | Unique sensor id; also names the session Comrade opens. |
| `command` | – | Shell command run through `bash -c`; its stdout is watched. |
| `tool` | – | Registered tool to invoke instead (a built-in, an MCP tool like `mcp_jira_…`, or a `skill_…`); its result is watched. Wins over `command` when both are set. |
| `args` | `{}` | JSON arguments passed to `tool`. |
| `interval_secs` | `300` | Poll period (floored at 10 seconds). |
| `mode` | `ask` | `ask` = queue and wait for you; `auto` = handle it on its own. |
| `prompt` | – | Optional seed prompt for the session Comrade opens. |
| `enabled` | `true` | Set `false` to keep the entry but stop polling it. |

The first poll only establishes a baseline. A change is diffed line by line
(blank lines and whitespace ignored) and reported in the transcript, then pushed
onto the **sensors queue** — a panel under the model panel listing unhandled
requests, oldest (highest priority) first. A request is handled by opening a
session titled `sensor: <name>`, **backed by a temporary file** and seeded with
the detected change; the agent can delegate the triage so the main context stays
lean. In `auto` mode Comrade starts the session as soon as nothing else is
running; in `ask` mode it waits for you.

Sensor sessions are short-lived: when a run finishes Comrade closes it and
deletes its temporary backing file, so recurring runs do not pile up in memory or
on disk. Sessions you open yourself are untouched.

Manage the queue from the M-x palette: `sensors-next` / `sensors-previous` move
the selection, `sensors-priority-up` / `sensors-priority-down` reorder it,
`sensors-discard` drops a request, and `sensors-start` tackles the selected one
now.

```toml
[[sensors]]
name = "gh-issues"
command = "gh issue list --state open"
interval_secs = 120
mode = "ask"

# Poll a JIRA MCP tool instead of a shell command.
[[sensors]]
name = "jira-sprint"
tool = "mcp_jira_list_tickets"
args = { sprint = "S-42", state = "open" }
interval_secs = 300
mode = "auto"
prompt = "Triage these tickets and delegate the small fixes."
```

### Example

```toml
[llm]
provider = "ollama"
model = "devstral-small-2"

[security]
autonomy = "ask"

[[delegates]]
name = "groq"
description = "Cheap, fast model for small edits and translations."
provider = "groq"
model = "llama-3.3-70b-versatile"

[[delegates]]
name = "claude"
description = "Claude for hard reasoning and reviews."
provider = "anthropic"
api_key = "sk-ant-..."
model = "claude-sonnet-4-20250514"
```

## 🔧 Build from source

```bash
cargo build --release --bin comrade   # -> target/release/comrade
```

Requires a recent stable Rust toolchain (edition 2024).

## 🤝 Contributing

Issues and pull requests are very welcome. To make them easy to act on, use the
provided templates — GitHub pre-fills them when you open a new
[issue](../../issues/new/choose) or [pull request](../../compare):

- **[Bug report](../../issues/new?template=bug_report.yml)** — what you expected, what happened, and how to reproduce.
- **[Feature request](../../issues/new?template=feature_request.yml)** — the problem, the proposal, and alternatives you considered.
- **[Pull request template](.github/pull_request_template.md)** — a summary, the linked issue, the changes and how you tested them.

Before opening a PR, make sure the commands CI runs are green:

```bash
cargo fmt --all -- --check
cargo test --workspace
```

## 🏷️ Releasing

Cut a release by pushing a `vX.Y.Z` tag, which triggers the release workflow
(builds the binary on Linux/macOS/Windows and creates the GitHub release with
notes generated from the commits since the previous release):

```bash
./release.sh patch   # v0.1.0 -> v0.1.1
./release.sh minor   # v0.1.0 -> v0.2.0
./release.sh major   # v0.1.0 -> v1.0.0
```

## 📄 License

Licensed under the [Apache License, Version 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be licensed as above, without any additional terms or conditions.
