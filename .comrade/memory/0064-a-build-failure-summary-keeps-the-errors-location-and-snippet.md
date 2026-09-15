# 0064 - A build-failure summary keeps the error's location and snippet
status: accepted
date: 2026-09-15
tags: pom_run_tests, diagnostics, small-model, reliability
summary: pom_run_tests' build-failure summary keeps each error's `--> file:line` location and snippet lines instead of only the error header line.

## Context
A delegate's compile error arrived as just `error: could not compile `smoke` (lib) due to 1 previous error` - the actual cause (`this file contains an unclosed delimiter` / `expected identifier, found `;``) and its `--> src/lib.rs:9:20` location live on the PRECEDING lines and in the snippet that follows. Without the location the 3B model could not tell where the problem was, so it rewrote the whole file (deleting code, see the destructive-write gate) and re-rolled until it ran out of iterations.

## Decision
The build-failure fallback in the ecosystem test summariser (`generic_error_lines`, used by `compose_test_summary` when a test run reports no results) keeps each error header AND up to 6 of its continuation lines: the `-->`/`|-->` location line and the `N | code` snippet lines that follow it. The message that wraps them now also says each error names the file:line to edit.

## Rationale
The location is the single most actionable part of a compiler error for a small model, and it is cheap: at most six extra lines per error, still far inside the observation cap.

## Alternatives considered
(1) Only the header line of an error - the previous behaviour; measured insufficient, the model rewrote the file blind until max_iterations. (2) Send the whole raw tool output - rejected: it is exactly the "noisy output" the compaction and truncation machinery exists to avoid, and a 3B model drowns in it.

## Scope
The generic (non-JSON) diagnostic path of comrade-tool-project's test summariser, which is what `pom_run_tests` falls back to when a build fails before emitting structured output. Cargo's JSON path (`pom_check`) already carried file:line:col.

## Impact
A failing build now tells a small model WHERE, which is the difference between a targeted fs_edit and a blind whole-file rewrite. The summary stays bounded (cap on errors + 6 lines each) and build noise (Compiling/Finished) is still dropped.

