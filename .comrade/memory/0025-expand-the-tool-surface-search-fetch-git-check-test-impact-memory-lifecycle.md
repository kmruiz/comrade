# 0025 - Expand the tool surface: search, fetch, git, check, test-impact, memory lifecycle
status: accepted
date: 2026-09-13
tags: tools, git, memory, verification
summary: Add regex/scoped fs_rgrep, web_fetch, git blame/stash/branch/checkout + diff ranges + pickaxe, pom_check (N errors), ts_test_impact, and memory lifecycle tools, each with an explicit read-only/mutating/delegate-deny classification.

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

