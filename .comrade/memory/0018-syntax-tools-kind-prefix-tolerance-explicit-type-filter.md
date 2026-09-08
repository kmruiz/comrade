# 0018 - Syntax tools: kind prefix tolerance + explicit type filter
status: accepted
tags: comrade-tool-syntax, tree-sitter, find_symbol, type-filter, tools
summary: comrade-tool-syntax: find_symbol/find_definition/read_symbol accept a 'type' kind filter and auto-strip a leading 'fn ' keyword from name/query inputs; fixed collect_decl_rows double-visit bug

## Context
User: the agent misuses the tree-sitter tools, e.g. find_symbol(query="fn my_function") instead of the bare name, and wants an explicit type filter param. Fixed in crates/comrade-tool-syntax (engine.rs + lib.rs).

## Decision
1. Engine (engine.rs): added pub KIND_LABELS (the 9 short labels: fn/struct/enum/trait/impl/mod/type/static/const, moved out of lib.rs) and pub split_kind_prefix(input)->(Option<label>, rest): splits only when the first whitespace-delimited word equals a label case-insensitively AND a non-empty remainder follows ("fn compute"->fn+compute; "typewriter"/"fnfoo"/"fn" alone stay untouched).
2. find_definition/read_symbol/find_decl and search_symbols take an extra trailing `kind: Option<&str>` and internally apply the same prefix->filter merge with a bail when a prefix kind contradicts an explicit kind. find_decl additionally filters by short_kind(node.kind())==kind. search_symbols filters DeclRow.label==kind.
3. Tools (lib.rs): find_symbol, find_definition and read_symbol schemas gained property "type" (enum KIND_LABELS, description 'optional declaration kind...'); their descriptions now tell the agent to pass the bare NAME and note a leading kind keyword is auto-stripped. Args are serde(rename="type") Option<String>, validated via normalize_kind (case/whitespace-insensitive, canonical label). Outputs echo the stripped bare name. find_references/references_count/rename also split_kind_prefix their symbol before the identifier token scan (rename validates the bare identifier).
VERIFY: cargo test -p comrade-tool-syntax (13 green incl. new split_kind_prefix/type-filter/prefix-search/conflict-bail/normalize_kind tests); full workspace cargo test green; cargo fmt clean. Clippy on the crate still fails only on the 2 pre-existing never_loop denies.


## Note
Bonus fix: collect_decl_rows used a descend/ascend walk that re-processed each declaration node after climbing back onto it, so search_symbols/list_symbols returned every row twice (pre-existing; .any()-only tests hid it — the new exact-count test pins it). Engine clippy still has the 2 pre-existing deny-level 'loop never actually loops' errors at engine.rs:93 (find_occurrences) and :263 (find_decl) — untouched legacy walks. Trade-off: any query starting "<kind-word> " is interpreted as a filter (decl names never contain spaces, so this is safe); the 'type' param exists only on find_symbol/find_definition/read_symbol.
