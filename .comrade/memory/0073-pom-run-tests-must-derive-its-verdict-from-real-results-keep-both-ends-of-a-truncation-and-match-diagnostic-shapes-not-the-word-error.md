# 0073 - pom_run_tests must derive its verdict from real results: keep both ends of a truncation and match diagnostic shapes, not the word "error"
status: accepted
date: 2026-09-15
tags: pom, tests, truncation, diagnostics, bugfix
summary: `pom_run_tests` falsely reported a green suite as "could not build" because `cap` kept only the head (dropping every `test result:` line) and the error scan matched any line containing the substring "error" (matching a passing test's NAME); the cap now keeps both ends and error detection requires a real diagnostic shape.

## Context
`pom_run_tests` advertises "when it reports all tests passing, the change is verified: stop re-running it", so its summary is load-bearing: the lead trusts it and stops verifying. In practice it reported `the tests could not build - fix these errors first` on a fully GREEN workspace suite, listing the passing test `test delegate::tests::context_overflow_errors_are_recognised ... ok` as the single "error".

Two independent defects compounded (confirmed empirically, not by reading alone):

1. `tasks::cap` kept only the FIRST `max` (9000) chars of the raw output and dropped the rest. For a workspace-wide `cargo test`, cargo prints one `test NAME ... ok` line per test and the `test result: ...` totals only AFTER them, so the first `test result:` line in this repo sits at byte 15646 - well past the cap. Every total was discarded, so `simplify_test_output` (which keys on lines starting with `test result:`) returned an empty summary.
2. `compose_test_summary` interprets an empty summary as "the build failed" and falls back to `generic_error_lines`, which classified a line as an error via a bare lowercase substring test: `l.contains("error")`. The only "error" in the retained head was the NAME of a passing test.

Net effect: defect 1 triggered the fallback path, and defect 2 made that path lie. A green suite was reported as a build failure, which is worse than no summary at all - it sends the lead to "fix" a non-existent compile error.

## Decision
Two rules, in `crates/comrade-tool-project`:

1. TRUNCATION KEEPS BOTH ENDS. `tasks::cap` now keeps the head (a third of the budget is reserved for the tail, minus the elision marker) and elides the middle with an explicit `... (output truncated) ...` marker, instead of keeping the head only. The two ends carry different information and each verb needs a different one: the head has the compile errors, the tail has the `test result:` totals the simplifier parses.

2. DIAGNOSTICS MUST MATCH A SHAPE, NOT A SUBSTRING. `ecosystem::is_error_header` (private, in `ecosystem/mod.rs`) replaces the `contains("error")` test. It rejects test-runner output outright (lines starting with `test ` or `test result:`) and otherwise accepts only real diagnostic shapes: `error:` / `error[`, a diagnostic code (`error[e`), or one of the stock failure phrases (`cannot find`, `mismatched types`, `could not compile`, `failed to compile`).

The structured cargo JSON path (`parse_check_json`) is unchanged and still tried first.

## Rationale
Both fixes are needed and neither alone is sufficient: only fixing the classifier leaves the summary empty (so the lead is told "no summary lines" and learns nothing about a green suite), while only fixing the cap leaves the false-positive classifier armed for any other toolchain whose output mentions "error" without being a diagnostic. Keeping both ends of a truncation is cheap and right for every verb, because the informative part of a build log is its head and the informative part of a test log is its tail. Requiring a diagnostic SHAPE is the actual invariant - the old rule never expressed "is this a compiler error?", only "does this line mention error?".

## Alternatives considered
Keep the loose `contains("error")` scan (rejected: a passing test name is not a diagnostic). Drop the diagnostics fallback entirely so an empty summary says "no summary lines" (rejected: discards the genuinely useful build-failure case that the fallback exists for). Raise the 9000-char cap so all output fits (rejected: unbounded, and the model only reads a summary anyway). Cap the tail only (rejected: loses the compile errors). Use the structured cargo `--message-format=json` diagnostics on this path (rejected: `cargo test` does not emit them for the test-run phase, and the generic scan is the documented path for toolchains without a structured format).

## Scope
The output cap in `comrade-tool-project`'s task runner and the generic (non-JSON) error scan in its ecosystem module, plus their tests. Does NOT change the cargo `--message-format=json` structured path, the tool descriptions/schemas, the agent loop's own observation cap, or the `summarise` tool (which is delegate-written, not deterministic code).

## Impact
`pom_run_tests` once again reports real pass/fail totals on a large workspace suite, so its "all passing means verified" contract holds. Both `pom_run_tests` and `pom_run_task` share `cap`, so a truncated task output now shows its tail too (e.g. a final summary line that used to be cut). `generic_error_lines` is stricter, so a toolchain that reports failures in an unrecognised shape could now yield no diagnostics where it previously yielded a false one - the fallback message is still emitted, but only for a genuinely empty summary. Regression tests (all verified to FAIL on the pre-fix code): `error_scan_ignores_passing_tests_that_mention_error` (ecosystem/tests.rs), `cap_keeps_the_tail_so_test_totals_survive` and `a_capped_workspace_test_run_still_reports_its_totals` (tasks.rs).

