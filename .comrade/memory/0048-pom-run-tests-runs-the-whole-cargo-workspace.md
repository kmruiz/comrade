# 0048 - pom_run_tests runs the whole Cargo workspace
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

