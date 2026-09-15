# 0062 - Small-model support is the shared prompt path, not a config profile - and stays polyglot
status: accepted
date: 2026-09-15
tags: prompts, small-model, polyglot, config, delegation
summary: The small-model recipe lives in the shared model-facing prompts (strict numbered delegate runbook + finishing stop condition) and applies to all models and delegates - there is no small-model config profile - and all shared prompt wording is deliberately polyglot (no Rust-only test/file idioms), asserting a flat tool schema and semantic_search for every model.

## Context
The task was to make Comrade usable with mistralai/ministral-3-3b (3B) for a basic feature end to end. The first attempt added a "small model" profile knob (a config Profile enum plus a prompts/small-model.md section), which the user rejected: the recipe must apply to ALL models and delegates, and `semantic_search` should be available to every model as a cheap way to locate code. Separately, Comrade is polyglot (Cargo + npm), so prompt wording that assumes Rust misleads an agent in an npm repo.

## Decision
No small-model config knob and no per-model prompt section: the small-model recipe lives in the SHARED model-facing prompts and applies to every model and delegate - a strict numbered runbook in prompts/delegate-system.md (read the named file -> one fs_edit with all three keys -> pom_run_tests -> stop), the "one step, one file, one change" rule and the tool-usage rule "never use shell to edit or to run tests" in the same file, and the finishing.md stop condition. `semantic_search` is advertised to every model, including in the delegate CORE registry (crates/comrade-tui/src/main.rs). Prompt and ToolSpec wording is language-neutral: no `#[cfg(test)] mod tests`, no `.rs`, no `cargo test` as the stand-in for "run the tests" - test guidance says "the test block that file already uses" and verification says pom_run_tests, and descriptions name both Cargo and npm where an example is needed.

## Rationale
The low-cost models that need a strict runbook are exactly the ones whose users will not discover and set a knob, so the safe default must BE the shared prompt; and a prompt that names one language is wrong for the other half of the ecosystem the same codebase supports.

## Alternatives considered
(1) A `[llm] profile = "small"` config knob selecting an extra prompt section - implemented first, then rejected by the user: the recipe must apply to all delegates, and a knob nobody sets is dead weight. (2) Weakening the prompts only for local/base_url endpoints - rejected for the same reason. (3) Keeping Rust examples because Comrade itself is Rust - rejected: the prompts drive arbitrary user repositories and Comrade is polyglot (Cargo + npm, ADR #30).

## Scope
Model-facing prompt sections (crates/comrade-core/prompts/*.md) and ToolSpec.description strings. Not the tool implementations, which may legitimately emit language-specific commands derived from the files in play.

## Impact
One prompt path to maintain and no config surface to explain. Language-specific hints are only allowed inside a language-aware tool (e.g. ts_test_impact emitting `cargo test -p <pkg>` for a Rust crate, which is computed from the file being changed), never in shared prose. Pinned by the wording tests that keep the delegate prompt's semantic contracts, and by keeping every prompt file free of Rust-only phrasing.

