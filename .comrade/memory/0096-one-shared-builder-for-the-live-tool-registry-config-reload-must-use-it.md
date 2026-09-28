# 0096 - One shared builder for the live tool registry; config reload must use it
status: accepted
date: 2026-09-28
tags: tui, mcp, config
summary: The live tool registry is built in exactly one place — the async `build_tools` (which connects the MCP servers) — and both startup and a config reload must use it, installing the result on the main loop through the reload channel.

## Context
A Ctrl-R config reload rebuilt the live tool registry with `build_tools` alone, while startup additionally called `comrade_tool_mcp::connect_all`. The two paths drifted, so a reload silently dropped every MCP tool and the servers "reported no tool". A rule is needed so the two paths cannot diverge again.

## Decision
`build_tools` (crates/comrade-tui/src/main.rs) is the SINGLE builder of the live `ToolRegistry`: the built-in tools plus one adapter per tool advertised by every configured MCP server (`connect_all`). It is `async` and connects the MCP servers itself. Both startup (`build_deps`) and a config reload (`reload_from_disk` in crates/comrade-tui/src/tui.rs) MUST build the registry through it; no call site may append tools by hand. Because the builder is async, a reload cannot run inline in the key handler: `App::reload_config` spawns the rebuild on a task and the main loop installs the `ReloadOutcome` received on `App::reload_rx` via `App::install_reload`. A server that fails to connect is skipped with a warning (`connect_all`), never fatal; a reload that fails leaves the live config, client and tools untouched.

## Rationale
One builder means the startup and reload paths share the exact same registry assembly, which is the only way to guarantee an MCP (or any future) tool source shows up in both. It also keeps the rebuild off the UI thread, where reconnecting a server must not block the event loop.

## Alternatives considered
Keep `connect_all` at each call site (already drifted once — startup called it, reload did not); have the reload mutate the live registry in place (rejected: `ToolRegistry`'s tool list is not interior-mutable, and the MCP modal's disabled-set handle would go stale); block the UI thread on the async connect with `block_in_place` (rejected: freezes the UI and panics under the current-thread runtime tests use).

## Scope
Covers how the comrade-tui binary assembles its live tool registry and how a config reload rebuilds it. Does not cover which MCP servers are configured, the MCP client itself, or the headless runner's one-shot registry.

## Impact
A new tool source (a new built-in family, a new MCP transport, a skill kind) must be registered inside `build_tools`, not appended at a call site, or the reload path will miss it again. New work during a reload belongs in `reload_from_disk` / `App::install_reload`, never inline in the Ctrl-R key handler. Tests: `crates/comrade-tui/src/tui.rs` (`ctrl_r_reload_keeps_mcp_tools`, `reload_reconnects_mcp_servers_so_their_tools_survive`).

