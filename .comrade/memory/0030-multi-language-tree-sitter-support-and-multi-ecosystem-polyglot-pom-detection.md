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

