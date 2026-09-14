# 0018 - Per-delegate `enabled` flag to disable a model without removing it
status: superseded
date: 2026-09-13
tags: config, delegate, ask_advise, tui, models
summary: Each [[delegates]] entry gained `enabled = true|false` (default true); a disabled delegate is dropped from delegate/ask_advise targets + model enum + advertised listing (and from the Ctrl-A picker), but kept in config and shown dimmed in the model panel.

## Context
Users wanted to temporarily turn a configured delegate off without deleting its [[delegates]] entry from config.toml (and having to re-add provider/model/key later). ADR #3 already added a per-delegate `approval` policy on the same struct, so the config vocabulary for "which delegates may run" was established there.

## Decision
Added `pub enabled: bool` to DelegateCfg (crates/comrade-core/src/config.rs), defaulting to `true` (serde(default) + manual Default impl) so existing configs are unaffected. Semantics: a delegate with `enabled == false` is treated as if not configured for the agent. `build_targets` skips disabled entries BEFORE any validation (no name/model check, no HTTP client); a disabled entry may therefore be blank/duplicate without breaking the build. `DelegateTool::new` and `AskAdviseTool::new` return `Ok(None)` when no enabled delegate remains, and build the advertised listing from enabled entries only, so disabled models are absent from the `model` enum and the tool description. In the TUI (crates/comrade-tui/src/tui.rs) `delegate_panel_rows` still shows disabled delegates, dimmed with a trailing ` (disabled)` marker, and the Ctrl-A model picker filters them out. Disabling affects BOTH `delegate` and `ask_advise` (a disabled model is disabled everywhere), unlike `approval = "deny"` which keeps the model listed but refuses it at run time.

## Rationale
Reuses the existing per-delegate config surface and the same shared `build_targets`/listing path as ADR #3, so delegate and ask_advise stay consistent with no new plumbing. Defaulting to true preserves today's behaviour. Filtering (rather than listing-but-refusing like `deny`) keeps the tech lead's option list truthful — it never sees or tries a name it cannot use — while the TUI panel keeps the human informed the entry is configured but off.

## Alternatives considered
1) Reuse `approval = "deny"`: keeps the model advertised and only refuses at run time, so the lead still sees/selects a model that can never run — not what "disabled" should mean. 2) Remove the entry from config: the user explicitly did not want to lose the settings. 3) Hide disabled delegates from the TUI panel too: rejected — the human should still see a configured-but-off model. 4) Skip provider/base_url validation for disabled entries in apply_provider: rejected for now (kept simple; the entry's provider is still resolved so the panel can display it).

## Scope
Covers the config field, the fill/parse behaviour, build_targets/DelegateTool::new/AskAdviseTool::new filtering, the TUI panel marker + picker filter, and new tests in comrade-core (config, delegate, advise) and comrade-tui. Does not touch `approval`, `security.autonomy`, the delegate sub-agent internals, or MCP servers.

## Impact
A user writes `enabled = false` on a [[delegates]] entry to park a model: it disappears from the delegate/ask_advise options and the Ctrl-A picker but stays visible (dimmed, ` (disabled)`) in the model panel and remains in config.toml to flip back on. Applies to both `delegate` and `ask_advise`. Follow-up: the TUI model panel test covers the marker; a docs/config example could mention `enabled`.


## Note
merged into #0001
