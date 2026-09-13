# 0033 - pom_run_tests returns a failure-only, bounded summary
status: accepted
date: 2026-09-13
tags: pom, tests, context
summary: pom_run_tests now returns a failure-only, hard-bounded summary (Cargo: totals + failing names + 15-line excerpts, 3500-char cap), so its output is never truncated by the agent loop.

## Context
pom_run_tests returns eco.simplify_tests(raw). The Cargo implementation (`simplify_test_output`) kept every non-passing line up to 200 lines and passed each failing test's captured stdout/stderr through unfiltered. A test that prints a lot, or many failing tests, produced output larger than the agent loop's observation cap (max_tool_output_chars, default 5000 chars), so the agent saw `...(truncated, N chars -> 5000)` and repeatedly complained.

## Decision
Rewrite the test summarizers (crates/comrade-tool-project/src/ecosystem.rs) to return a failure-only, bounded summary. Cargo: keep the `test result:` totals, the failing-test names, and a bounded excerpt (header + 15 lines) of each failure's captured output; drop passing tests, build noise and the redundant trailing `failures:` list; cap the whole thing at 3500 chars. JS/TS (jest/vitest): keep the totals lines first, then failing/error lines, capped at 3000 chars. A shared `trim_chars` helper truncates with a marker.

## Rationale
The tool's stated contract is a simplified, failure-focused summary; making it failure-only and hard-bounded fixes the truncation at its source without spending context on every other tool.

## Alternatives considered
(a) Raise the agent observation cap (max_tool_output_chars) — rejected: it costs context on EVERY tool call, and raw `cargo test` output is unbounded, so it only moves the threshold. (b) Keep the old 200-line scheme — rejected: the truncation is exactly the bug. (c) Add a separate `only_failures` argument — rejected as unnecessary: a failure-only summary is what the tool should always return.

## Scope
Covers the test-summary rendering for the Cargo and npm ecosystems only. Does NOT change the agent observation cap, the raw task runner, or diagnostic parsing.

## Impact
pom_run_tests output is now small and never truncated by the agent loop, and shows only failing tests (plus per-binary totals). Cost: passing-test detail is no longer shown, and very large per-test stdout is cut to 15 lines per failure (the panic message, which cargo prints in a separate stderr block, is preserved).

