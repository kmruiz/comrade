## What is Comrade

Comrade is a terminal cockpit that drives an LLM coding agent through real work in your repository. It ships as a single native binary.

- **Agent loop** — ReAct-style tool use with a live, scrollable plan, streaming output, reasoning and a focus mode.
- **Models** — Anthropic-compatible clients with retry/backoff, optional prompt caching, and configurable delegates (parallel jobs, isolated worktrees).
- **Code tools** — tree-sitter powered find/read symbol, references, rename and structural map, plus filesystem read/write/edit/rgrep.
- **Project tools** — detect and run the project's tasks (Cargo & npm), background jobs, git operations, web search/fetch.
- **Memory** — ADR decisions and a glossary under `.comrade/memory/`, with semantic search across memory and code.
- **Extensible** — MCP servers, Claude-format skills, and per-tool approval policies.

## Installation

See the install commands in the README; each archive below contains the `comrade` binary for that platform.
