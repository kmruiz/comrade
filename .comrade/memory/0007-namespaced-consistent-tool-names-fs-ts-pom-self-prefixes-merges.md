# 0007 - Namespaced, consistent tool names (fs_/ts_/pom_/self_ prefixes + merges)
status: accepted
date: 2026-09-09
tags: tool-rename, naming, toolset, refactor
summary: Renamed all tools with domain prefixes (fs_*, ts_*, pom_*, self_*), merged apply_edit+apply_patch into fs_edit, find_definition+read_symbol into ts_read_symbol (body flag), and dropped references_count; memory tools renamed to record_adr/find_adr/read_adr/amend_adr/record_glossary; ask_question -> ask_user.

## Context
The model-facing tool list was a flat ~42-tool namespace with inconsistent verbs: remember/find_decisions vs remember_glossary/find_glossary, bare edit tools (apply_edit/apply_patch), no signal of a tool's domain, and cargo-specific wording in project-tool descriptions. Two overlapping tools (find_definition + read_symbol) shared an engine flag (want_body) and references_count duplicated find_references with only tallying on top.

## Decision
Every tool gets a domain prefix in its ToolSpec name and Rust identifier: fs_* (comrade-tool-fs), ts_* (comrade-tool-syntax / tree-sitter), pom_* (comrade-tool-project, build-system agnostic), self_* (session/planning tools the agent runs on itself), memory tools renamed to the adr/glossary noun families (record_adr/find_adr/read_adr/amend_adr, record_glossary). ask_question -> ask_user (it asks the human). apply_edit + apply_patch merged into one fs_edit tool accepting either literal old/new or a unified diff; find_definition + read_symbol merged into ts_read_symbol with a body:bool arg (default false); references_count removed entirely (its blast-radius role is covered by ts_rename's preview). Shell stays bare (not a POM concept). Engine helper fns (engine::find_definition/read_symbol/structural_map etc.) keep their internal names.

## Rationale
Prefixes make a tool's domain obvious at a glance and group like tools in listings; consistent verb/noun families (record/find/read/amend over adr) remove guesswork for the model. Merges cut surface area without losing capability (the merged tools dispatch on arguments, and the deleted one duplicated find_references). The cargo-free wording keeps descriptions accurate if the task layer later supports non-Cargo build systems.

## Alternatives considered
Keep bare memorable names (rgrep, shell, rename); rejected because consistency with the rest of the family won. Merge the two read tools into find_symbol-style search vs read split; rejected because find_symbol is a substring search (different operation) while find_definition/read_symbol are the same exact-lookup differing only in body inclusion. Use a decision-verb family (record_decision/find_decision...) instead of adr nouns; rejected: adr matches the storage (ADR decisions) and reads shorter.

## Scope
All seven tool crates plus the comrade-core static tables (MUTATING_TOOLS/APPROVAL_GATED_TOOLS/READ_ONLY_TOOLS/CODE_CHANGES), delegate/advise deny/read-only lists and tests, the 9 model-facing prompt .md files and react.rs prompt assertions, comrade-tui name-keyed rendering (icons, read-tool dimming, digest, diff previews), and the .comrade glossary prose. .comrade ADR files (historical records) were intentionally left as written.

## Impact
Model-facing tool list shrinks ~42 -> ~35 tools. Any code or prompt that names a tool must use the new name; the old names no longer resolve. Internal engine fn names and test doubles were deliberately left alone. Follow-ups: keep new prompts/specs in sync when tools change; the merged fs_edit/ts_read_symbol descriptions document both modes so the model picks the right arguments.

