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
| 🧠 **Agent loop** | ReAct-style tool use with a live, scrollable plan, streaming output, reasoning blocks and a distraction-free focus mode. |
| 🔌 **Models** | Anthropic-compatible clients with retry/backoff and optional prompt caching. Configure **delegates** for parallel jobs and isolated git worktrees. |
| 🌳 **Code tools** | tree-sitter powered `ts_*` tools: find/read symbol, references, rename, structural map — plus `fs_*` read / write / edit / rgrep. |
| 📦 **Project tools** | Detect and run the project's own tasks (Cargo & npm), background jobs, git operations and web search/fetch. |
| 🗂️ **Memory** | ADR decisions and a glossary under `.comrade/memory/`, with semantic search across both memory and code. |
| 🧩 **Extensible** | MCP servers, Claude-format skills (`SKILL.md`), and per-tool approval policies. |

## 📖 Table of contents

- [Install](#-install)
- [Usage](#-usage)
- [Build from source](#-build-from-source)
- [Contributing](#-contributing)
- [Releasing](#-releasing)
- [License](#license)

## 📥 Install

Prebuilt binaries are published on the [releases page](../../releases). Each
command below resolves the latest release tag and installs the `comrade` binary
into `/usr/local/bin` (drop the `sudo` if that directory is writable for you).

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

Comrade reads its configuration and project memory from `.comrade/` in the
working directory (see the `AGENTS.md` at the repo root for project rules that
are injected into the agent's system prompt).

## 🔧 Build from source

```bash
cargo build --release --bin comrade   # -> target/release/comrade
```

Requires a recent stable Rust toolchain (edition 2024).

## 🤝 Contributing

Issues and pull requests are very welcome. To make them easy to act on, please
use the provided templates — GitHub pre-fills them when you open a new
[issue](../../issues/new/choose) or [pull request](../../compare):

- **[Bug report](../../issues/new?template=bug_report.yml)** — what you expected, what happened, and how to reproduce.
- **[Feature request](../../issues/new?template=feature_request.yml)** — the problem, the proposal, and alternatives you considered.
- **[Pull request template](.github/pull_request_template.md)** — a summary, the linked issue, the changes and how you tested them.

Before opening a PR, please make sure the same commands CI runs are green:

```bash
cargo fmt --all -- --check
cargo test --workspace
```

## 🏷️ Releasing

Releases are cut by pushing a `vX.Y.Z` tag, which triggers the release workflow
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
