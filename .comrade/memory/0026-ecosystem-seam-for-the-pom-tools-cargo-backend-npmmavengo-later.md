# 0026 - Ecosystem seam for the pom_* tools (Cargo backend, npm/Maven/Go later)
status: superseded
date: 2026-09-13
tags: project, ecosystem, architecture, tools
summary: Route all `pom_*` tools through an `Ecosystem` trait (`detect` picks Cargo by manifest); generalize `CommandLine` to `Program{program,args}` and let backends own verb resolution, the check command, and diagnostics parsing.

## Context
The `pom_*` tools (pom_model, pom_run_task, pom_run_tests, pom_check, pom_format_code) and their helpers in `pom.rs`/`tasks.rs` were written Cargo-only: hardcoded `cargo` verbs, `.cargo/config.toml` aliases, `cargo check --message-format=json` diagnostics, and a Cargo-specific test-output simplifier. The user wants the same tools to work for other build ecosystems (npm/Maven/Go) later, without rewriting each tool.

## Decision
Introduce a `Ecosystem` trait in `crates/comrade-tool-project/src/ecosystem.rs` as the single seam. It exposes: `name`/`manifest`, `model(root)`, `supports(root, verb)` (verbs + aliases), `resolve(root, verb, subproject, extra) -> Resolved`, `format_command(root)`, `check_command(root, subproject, all_targets, extra) -> Option<CommandLine>` (default `None`), `parse_diagnostics(raw, max) -> (Vec<String>, usize)` (default = generic error-line scan), `simplify_tests(raw)` (default = passthrough) and `is_test_command(line)`. `detect(root)` returns `Box<dyn Ecosystem>` (Cargo today, keyed on the manifest; new backends slot in there in priority order). `CommandLine` was generalized from `Cargo { args }` to `Program { program, args }` (plus `Shell`) so a backend emits its own tool, and `tasks.rs` now holds only the ecosystem-neutral process runner (`exec`/`run`) plus the Cargo resolver. All five `pom_*` tools go through `detect(...)` and the trait.

## Rationale
One trait with a `detect` dispatcher keeps ecosystem knowledge in one place, lets tools stay identical, and defaults (`check_command`/`parse_diagnostics`/`simplify_tests`) mean a minimal backend only implements `model`/`resolve`/`format_command`. Generalizing `CommandLine` removes the "Cargo" naming lie for future backends.

## Alternatives considered
Keeping the cargo-only code and adding per-ecosystem `if` branches inside each tool was rejected: it spreads ecosystem knowledge across every tool. A plugin/trait-object registry discovered dynamically was rejected as over-engineered for a compile-time-known set. Naming the variant `CommandLine::Cargo` and having non-cargo backends emit shell strings was considered but rejected as dishonest.

## Scope
Covers the `pom_*` build/run/check/format surface of `comrade-tool-project`. Does not add any non-Cargo backend yet (detection returns an error listing `cargo` as the only supported ecosystem), and leaves `pom.rs` (the Cargo model parser) as the Cargo backend's internals.

## Impact
Adding npm/Maven/Go later is "implement `Ecosystem` + one arm in `detect`" with no `pom_*` tool change; `pom_check` degrades gracefully via `generic_error_lines` for toolchains without structured diagnostics. Cargo behaviour is unchanged (existing tests still pass, plus new ecosystem tests). The trait is a compile-time seam (no dynamic loading).


## Note
merged into #0030
