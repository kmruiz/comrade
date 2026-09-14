# 0033 - pom_run_tests returns a failure-only, bounded summary
status: accepted
date: 2026-09-13
tags: pom, tests, context
summary: pom_run_tests now returns a failure-only, hard-bounded summary (Cargo: totals + failing names + 15-line excerpts, 3500-char cap), so its output is never truncated by the agent loop.

## Context
pom_run_tests returns eco.simplify_tests(raw). The Cargo implementation (`simplify_test_output`) kept every non-passing line up to 200 lines and passed each failing test's captured stdout/stderr through unfiltered. A test that prints a lot, or many failing tests, produced output larger than the agent loop's observation cap (max_tool_output_chars, default 5000 chars), so the agent saw `...(truncated, N chars -> 5000)` and repeatedly complained.

## Decision
Rewrite the test summarizers (crates/comrade-tool-project/src/ecosystem/mod.rs) to return a failure-only, bounded summary. Cargo: keep the `test result:` totals, the failing-test names, and a bounded excerpt (header + 15 lines) of each failure's captured output; drop passing tests, build noise and the redundant trailing `failures:` list; cap the whole thing at 3500 chars. JS/TS (jest/vitest): keep the totals lines first, then failing/error lines, capped at 3000 chars. A shared `trim_chars` helper truncates with a marker.

## Rationale
The tool's stated contract is a simplified, failure-focused summary; making it failure-only and hard-bounded fixes the truncation at its source without spending context on every other tool.

## Alternatives considered
(a) Raise the agent observation cap (max_tool_output_chars) — rejected: it costs context on EVERY tool call, and raw `cargo test` output is unbounded, so it only moves the threshold. (b) Keep the old 200-line scheme — rejected: the truncation is exactly the bug. (c) Add a separate `only_failures` argument — rejected as unnecessary: a failure-only summary is what the tool should always return.

## Scope
Covers the test-summary rendering for the Cargo and npm ecosystems only. Does NOT change the agent observation cap, the raw task runner, or diagnostic parsing.

## Impact
pom_run_tests output is now small and never truncated by the agent loop, and shows only failing tests (plus per-binary totals). Cost: passing-test detail is no longer shown, and very large per-test stdout is cut to 15 lines per failure (the panic message, which cargo prints in a separate stderr block, is preserved).


## Merged from #0048 - pom_run_tests runs the whole Cargo workspace
status: accepted
date: 2026-09-14
tags: pom, cargo, tools, workspace
summary: pom_run_tests adds `--workspace` for a root-level Cargo `test` when the manifest declares a [workspace], so the summary covers every member; subproject-scoped runs keep --manifest-path only.

## Context
pom_run_tests resolves the `test` verb through the Cargo backend; at a workspace root it ran plain `cargo test`, which only tests the root package (and, for a virtual manifest, fails to test any member). A Rust workspace (like comrade itself) wants the whole workspace covered in one summary. The user asked for this before cutting the release.

## Decision
tasks::resolve now prepends `--workspace` to the cargo args for the `test` verb when there is no subproject and the root manifest declares a `[workspace]` table. ProjectModel gained an `is_workspace` field (true when `[workspace]` is present, whether the manifest is virtual or not). A subproject-scoped run is unchanged (still `--manifest-path=<dir>/Cargo.toml`, no `--workspace`). Only the `test` verb is affected: pom_run_task refuses `test`, and the alias-expansion branch is untouched.

## Rationale
The root manifest is the cheapest reliable signal of a workspace and ProjectModel already parses it. Scoping the change to the `test` verb + no-subproject keeps it from perturbing build/check/fmt. `--workspace` is exactly cargo's own way to say "all members", so the summary stays correct.

## Alternatives considered
1) Always pass --workspace for every cargo verb — rejected: changes build/check semantics (forces building every member) far beyond the test path the user asked about. 2) Detect the workspace by shelling out to `cargo metadata` — rejected: heavier, and the root manifest already tells us. 3) Use is_virtual only — rejected: misses a non-virtual workspace (root package + [workspace]).

## Scope
Covers the Cargo backend's verb resolution for `test` (crates/comrade-tool-project/src/tasks.rs), the ProjectModel field (pom.rs) and the pom_run_tests description. Does NOT change npm's test resolution, the is_test_command guard, or non-test verbs.

## Impact
A root-level pom_run_tests on a workspace now returns the aggregate pass/fail + failing test names of every member in one call. Existing subproject-scoped runs and all other verbs are unchanged. Bundled into v0.2.0.

## Note
Rollup of pom_run_tests output: this ADR made the summary failure-only and hard-bounded; #0048 makes a root-level run cover the whole Cargo workspace. Body preserved under "Merged from".
