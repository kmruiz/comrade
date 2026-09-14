# 0046 - Source files stay under 1000 lines; oversized modules become directory trees
status: accepted
date: 2026-09-14
tags: refactor, convention, module-layout
summary: Refactoring convention: every source file stays under 1000 lines, achieved by turning an oversized `<name>.rs` into a module tree (`<name>/mod.rs` + cohesive submodules, or the modern `<name>.rs` + `<name>/<sub>.rs` layout); long match/table functions are kept intact when they read better that way; comments are pruned to only what the code cannot express.

## Context
The workspace had grown to ~43.5k lines, with a 11.5k-line TUI monolith (crates/comrade-tui/src/tui.rs) and several 1-3k-line modules. This makes the code hard to navigate for humans and for the agent's own tools. The user asked for a refactor that keeps files under 1000 lines with small, easy-to-follow functions, and for redundant comments to be removed.

## Decision
Adopt a module-tree convention for any file over 1000 lines. Move cohesive clusters out of `<name>.rs` into submodules. Two equivalent layouts are allowed: (a) `<name>/mod.rs` holding the module docs, the shared `use` block and the type definitions, with `mod chat; mod draw; pub use chat::*; pub use draw::*;`; or (b) keep `<name>.rs` as the module root and add `<name>/<sub>.rs` etc. — Rust 2018+ resolves `mod sub;` in `<name>.rs` to `<name>/<sub>.rs`, which avoids having to delete/rename the original file. Each moved submodule starts with `use super::*;` so it sees the parent's items and `use` bindings. Items that other submodules or the rest of the crate reach through the parent's re-export must be at least `pub(crate)`, because a private item cannot be re-exported (this is the main mechanical fix-up needed after a move). Prefer keeping type definitions in the module root so descendant submodules can still touch their private fields (privacy in Rust is visible to the defining module and its descendants). Test modules are moved to their own files and declared `#[cfg(test)] mod tests;`. Do NOT change behaviour during a split; it is movement plus visibility only. `include_str!`/`include_bytes!` paths must gain one `../` per added directory level. Long functions that are a single big `match`/table stay as they are.

## Rationale
Directory modules keep `crate::foo::X` paths and the public API unchanged, so the split is safe and reviewable, and each commit can be verified by `cargo check` + tests. Layout (b) is preferred when a whole-file rename is awkward, since it needs no file deletion. Keeping type definitions in the root preserves the existing (private-field) encapsulation without widening every field to pub(crate).

## Alternatives considered
Splitting every file into flat sibling modules (rejected: loses grouping and churns `use` paths). Widening all struct fields to pub(crate) (rejected: needless encapsulation loss). Rewriting logic while splitting (rejected: makes review and bisecting impossible). Leaving long `match` functions split for their own sake (rejected: a big match is easier to read whole).

## Scope
Applies to every crate in the workspace, source and test files alike. Does NOT mandate a specific function length for match/table functions, and does not cover code-gen or vendored files.

## Impact
Done so far (all tests green): comrade-core/src/advise.rs -> advise/ (mod.rs, tool.rs, readiness.rs, tests.rs); comrade-tool-project/src/ecosystem.rs -> ecosystem/; comrade-tool-memory/src/semantic.rs -> semantic/; comrade-tool-session/src/lib.rs + tests.rs; comrade-core/src/llm.rs grew a llm/ollama.rs submodule (llm.rs still 1593 lines). Still OVER 1000 lines and to be split: crates/comrade-tui/src/tui.rs (11561 - the big one), comrade-core/src/delegate.rs (2949), comrade-core/src/agent.rs (2931), comrade-tool-fs/src/lib.rs (1771), comrade-core/src/llm.rs (1593), comrade-tool-syntax/src/lib.rs (1241), comrade-tool-syntax/src/engine.rs (1206). Note comrade-core/src/llm.rs and comrade-tool-syntax/src/lib.rs contain Rust/JSON fixtures inside raw strings, so any mechanical test-extraction must brace-match with literal awareness; and comrade-tool-syntax/src/lib.rs has a second `mod tests` in a fixture string.


## Note
Progress update. DONE (each committed with cargo check --all-targets clean, tests green before the final llm split): advise.rs -> advise/ (mod.rs, tool.rs, readiness.rs, tests.rs); ecosystem.rs -> ecosystem/; semantic.rs -> semantic/; comrade-tool-session lib.rs -> + tests.rs; llm.rs -> 890 lines with llm/{context_window.rs + 8 test files}. STILL OVER 1000 LINES and TODO: crates/comrade-tui/src/tui.rs (11561), crates/comrade-core/src/delegate.rs (2949), crates/comrade-core/src/agent.rs (2931), crates/comrade-tool-fs/src/lib.rs (1771), crates/comrade-tool-syntax/src/lib.rs (1241), crates/comrade-tool-syntax/src/engine.rs (1206). Reusable splitter that makes this mechanical: /tmp/split.py (regenerate it; it is a scratch tool, not committed) with `tests SRC OUTDIR` (moves every top-level `#[cfg(test)] mod N {..}` in place to OUTDIR/N.rs and leaves `#[cfg(test)] mod N;`) and `cut SRC SPEC` (SPEC JSON {move:[[a,b]], groups:{name:[[a,b]]}}); it brace/literal-matches so `mod tests` inside a raw-string fixture is ignored, which matters for comrade-tool-syntax/src/lib.rs. After any move run `cargo fmt --all` (moved bodies are dedented), then `cargo check --workspace --all-targets`, and widen re-exported items to pub(crate) as the compiler asks.

## Note
Exact remaining plan (line ranges are 1-based, from the pre-split files; use /tmp/split.py which brace-matches with literal awareness):

crates/comrade-core/src/agent.rs (2931): cut [[18,315]] -> agent/guards.rs (tool-classification consts + LoopTracker) and [[1257,1306]] -> agent/headless.rs (run_headless); then `tests agent.rs agent` moves the top-level test mods (tests @1308, loop_tests @2636, loop_tracker_tests @2811, read_guard_tests @2846, monitors_tests @2881, usage_tests @2904) into agent/. Leftover agent.rs = ~958 lines.

crates/comrade-tool-fs/src/lib.rs (1771): cut [[697,924]] -> fs/grep.rs (Matcher/GrepHit/grep/validate/IGNORED_DIRS/walk/glob_*); then `tests lib.rs <crate>/src` moves tests (@1108), patch_tests (@1640), confine_tests (@1682). Leftover ~878 lines.

crates/comrade-tool-syntax/src/lib.rs (1241): just `tests lib.rs <crate>/src` (single top-level `mod tests` @831; a second `mod tests` at 1187 is INSIDE a raw-string fixture and must be skipped). Leftover ~830 lines.

crates/comrade-tool-syntax/src/engine.rs (1206): cut [[14,140]] -> engine/model.rs (MAX_FILE_BYTES/Occurrence/FileEdits/walk_sources/LangId/SUPPORTED_EXTS/lang_of/grammar/language_for/ident_kinds + the RUST/JS/CSS/HTML kind consts); then `tests engine.rs engine`. Leftover ~938 lines.

crates/comrade-tui/src/tui.rs (11561) - NOT STARTED, the big one. Keep the `App` struct definition in the root file so descendant submodules can read its private fields.

NOTE: the extracted test files themselves can exceed 1000 lines (e.g. delegate/tests.rs 1761, agent tests ~1600); the user's "under 1000" rule was applied to source files - decide with the human whether test files must also be split.
