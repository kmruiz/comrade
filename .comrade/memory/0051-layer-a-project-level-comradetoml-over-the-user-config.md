# 0051 - Layer a project-level .comrade.toml over the user config
status: accepted
date: 2026-09-14
tags: config, layering, comrade-toml, tui

## Context
Request: allow a repo-level `.comrade.toml` at the project root that works like the user config (`~/.config/comrade/config.toml` or `--config`) but supersedes it. Today `Config::load(Option<Path>)` reads exactly one TOML. The TUI (`crates/comrade-tui/src/main.rs::build_deps`, and `tui.rs::reload_config`) is the only config loader; it computes the project root from `--dir` or the cwd.

## Decision
Add `Config::load_layered(base, project_root)`: parse the base file and, if `<project_root>/.comrade.toml` exists, layer it on top by merging the two `toml::Value` trees — tables merge per key (project wins), the named lists `[[delegates]]` and `[[mcp.servers]]` merge by `name` (a project entry with the same name replaces the user's in place; new names append), and every other value (scalar/plain array) is taken from the project file wholesale. The merged `Value` is deserialized into `Config`; `apply_provider` now inspects the merged `Value` (not a re-parsed raw string) to decide whether a provider preset should fill `base_url`. `Config::load(path)` is kept as `load_layered(path, None)`. `LoadedConfig` gains `repo_source`. The TUI passes the project root and, on reload, re-reads `.comrade.toml` from `App.root`.

## Rationale
Merging `toml::Value` trees preserves "was this key set?" for free (a key is present only if some file wrote it), which the struct-level merge cannot do, and it needs no extra dependency. Doing the merge before deserialization keeps `apply_provider`'s explicit-base_url detection correct, because the merged value is exactly what is deserialized. `serve.rs`/`mod tests` are unaffected.

## Alternatives considered
Deep-merge the deserialized `Config` structs (rejected: can't distinguish an explicitly-set field from a defaulted one — `LlmCfg.base_url` defaults to a real URL — so "who set base_url" would be lost). Merging with a crate like `config` or `toml_edit` (rejected: extra dependency/complexity for a small, well-defined merge). Whole-file replacement by `.comrade.toml` (rejected: the user asked for per-key supersede with same-name delegate override). Per-client native provider handling (out of scope).

## Scope
Config loading and the two TUI call sites. Not a general config-inheritance system: only two layers (user then project), only the project root is searched (no parent walk), and there is no `.comrade/` directory form — the file is exactly `.comrade.toml`.

## Impact
Repos can pin a model, provider or delegates without touching the user's global config. A repo-supplied `.comrade.toml` can also set `[security]` and `[hooks]` (which run shell commands), so cloning an untrusted repo is a trust boundary — documented in the README; a future guard (e.g. require confirmation before honouring repo `[hooks]`/`[security]`) is a possible follow-up. The merge is key-based, so a user file that explicitly sets `llm.base_url` still wins that key even when the repo sets `provider`.

