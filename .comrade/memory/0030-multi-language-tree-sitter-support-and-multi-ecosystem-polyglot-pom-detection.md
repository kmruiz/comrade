# 0030 - Multi-language tree-sitter support and multi-ecosystem (polyglot) POM detection
status: accepted
date: 2026-09-13
tags: tree-sitter, languages, pom, ecosystem, npm
summary: Tree-sitter tools now parse Rust/JS/TS/TSX/CSS/HTML via a per-language descriptor; the POM detects ALL ecosystems (Cargo + npm) and `pick` selects one for polyglot repos.

## Context
Comrade only parsed Rust in the tree-sitter tools (crates/comrade-tool-syntax) and only understood Cargo in the POM/task tools (crates/comrade-tool-project). The user asked to support JavaScript/TypeScript/CSS/HTML so Comrade can work on web apps, explicitly including Node.js projects (package.json) and repositories that mix several languages/ecosystems in one tree.

## Decision
comrade-tool-syntax: add a `LangId` abstraction in engine.rs (per language: tree-sitter grammar, `ident_kinds` for occurrence search, `decl_label` kind->label table, `container_body`, `decl_name`) covering Rust, JS/JSX/MJS/CJS, TS/MTS/CTS, TSX, CSS, HTML. File walking uses one supported-extension set (`walk_sources`), so a repo may mix languages; `ts_*` tools and semantic chunking (chunks.rs) now emit declaration chunks per language. `find_decl`/`collect_decl_rows` had a latent bug (they `return`ed after the first file) which is fixed so multi-file projects are fully scanned. comrade-tool-project: new `node` module (package.json: name/deps/scripts/workspaces) and a `Node` Ecosystem; detection is now `detect_all(root)` returning EVERY present ecosystem in priority order (Cargo, then npm); `pick(root, ecosystem, verb)` selects one — explicit name, or the sole backend, or the one that supports the verb, else an error asking for an explicit choice. `pom_model` renders all detected ecosystems; pom_run_task/pom_run_tests/pom_check/pom_format_code gained an optional `ecosystem` argument.

## Rationale
A single per-language descriptor table keeps one engine for all grammars and localises future additions to one place. Returning ALL ecosystems (instead of one) is the only way a mixed repo's layout can be reported truthfully and its tasks dispatched without guessing; `pick` keeps the common single-ecosystem case friction-free.

## Alternatives considered
For tree-sitter: (a) a separate crate per language — rejected, wants one engine; (b) hardcoding JS/TS alongside Rust in each function — rejected, unmaintainable as grammars grow. For the POM: (a) keep a single `detect` and require one ecosystem per repo — rejected, the user needs mixed repos; (b) add an `ecosystem` arg to every tool with no auto-selection — rejected as needless friction for the common single-ecosystem case.

## Scope
Covers language detection/parsing/chunking in comrade-tool-syntax and ecosystem detection/selection plus the npm backend in comrade-tool-project. Does NOT add JS/TS semantic (LSP-grade) resolution, JS/TS test discovery, bundler-specific tasks, or a web-server tool.

## Impact
ts_find_references/rename/list_symbols/structural_map/read_symbol/find_symbol and semantic code search now cover web sources; a mixed .rs+.ts+.css+.html repo is indexed in one pass. pom_* tools work on npm projects and on polyglot repos (pass `ecosystem` when ambiguous). Behaviour change: on a repo with BOTH manifests, `detect` prefers Cargo and a task supported by both (e.g. build) is ambiguous through `pick` and requires `ecosystem`. Rust-only paths (test_functions/decl_names_in_text) are guarded to Rust. Follow-ups: HTML elements are chunked at the outermost element only; JS/TS test discovery is not yet implemented (Rust `#[test]` only).


## Note
Follow-up resolved: JS/TS test discovery is now implemented. engine::test_functions finds JS/TS `it`/`test`/`specify` cases (incl. member forms `it.only`/`test.skip`/… and `xit`/`xtest`/`fit`; `describe` is treated as a container, not a case) and `decl_names_in_text` is language-aware (picks the grammar by file extension). ts_test_impact now maps changed files of any supported language, adds a same-directory heuristic for colocated tests, and lists affected JS/TS test files in its suggested runs. See crates/comrade-tool-syntax/src/engine.rs (js_tests) and src/lib.rs (ts_test_impact).

## Merged from #0026 - Ecosystem seam for the pom_* tools (Cargo backend, npm/Maven/Go later)
status: accepted
date: 2026-09-13
tags: project, ecosystem, architecture, tools
summary: Route all `pom_*` tools through an `Ecosystem` trait (`detect` picks Cargo by manifest); generalize `CommandLine` to `Program{program,args}` and let backends own verb resolution, the check command, and diagnostics parsing.

## Context
The `pom_*` tools (pom_model, pom_run_task, pom_run_tests, pom_check, pom_format_code) and their helpers in `crates/comrade-tool-project/src/pom.rs`/`crates/comrade-tool-project/src/tasks.rs` were written Cargo-only: hardcoded `cargo` verbs, config-file task aliases, `cargo check --message-format=json` diagnostics, and a Cargo-specific test-output simplifier. The user wants the same tools to work for other build ecosystems (npm/Maven/Go) later, without rewriting each tool.

## Decision
Introduce a `Ecosystem` trait in `crates/comrade-tool-project/src/ecosystem/mod.rs` as the single seam. It exposes: `name`/`manifest`, `model(root)`, `supports(root, verb)` (verbs + aliases), `resolve(root, verb, subproject, extra) -> Resolved`, `format_command(root)`, `check_command(root, subproject, all_targets, extra) -> Option<CommandLine>` (default `None`), `parse_diagnostics(raw, max) -> (Vec<String>, usize)` (default = generic error-line scan), `simplify_tests(raw)` (default = passthrough) and `is_test_command(line)`. `detect(root)` returns `Box<dyn Ecosystem>` (Cargo today, keyed on the manifest; new backends slot in there in priority order). `CommandLine` was generalized from `Cargo { args }` to `Program { program, args }` (plus `Shell`) so a backend emits its own tool, and `crates/comrade-tool-project/src/tasks.rs` now holds only the ecosystem-neutral process runner (`exec`/`run`) plus the Cargo resolver. All five `pom_*` tools go through `detect(...)` and the trait.

## Rationale
One trait with a `detect` dispatcher keeps ecosystem knowledge in one place, lets tools stay identical, and defaults (`check_command`/`parse_diagnostics`/`simplify_tests`) mean a minimal backend only implements `model`/`resolve`/`format_command`. Generalizing `CommandLine` removes the "Cargo" naming lie for future backends.

## Alternatives considered
Keeping the cargo-only code and adding per-ecosystem `if` branches inside each tool was rejected: it spreads ecosystem knowledge across every tool. A plugin/trait-object registry discovered dynamically was rejected as over-engineered for a compile-time-known set. Naming the variant `CommandLine::Cargo` and having non-cargo backends emit shell strings was considered but rejected as dishonest.

## Scope
Covers the `pom_*` build/run/check/format surface of `comrade-tool-project`. Does not add any non-Cargo backend yet (detection returns an error listing `cargo` as the only supported ecosystem), and leaves `crates/comrade-tool-project/src/pom.rs` (the Cargo model parser) as the Cargo backend's internals.

## Impact
Adding npm/Maven/Go later is "implement `Ecosystem` + one arm in `detect`" with no `pom_*` tool change; `pom_check` degrades gracefully via `generic_error_lines` for toolchains without structured diagnostics. Cargo behaviour is unchanged (existing tests still pass, plus new ecosystem tests). The trait is a compile-time seam (no dynamic loading).

## Note
Rollup of the POM multi-ecosystem work: this ADR adds multi-language tree-sitter support and all-ecosystem (Cargo + npm) detection; #0026 introduced the Ecosystem trait seam the detection is built on. Body preserved under "Merged from".
