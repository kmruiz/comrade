# 0014 - TUI Ctrl-R / M-x reload-config re-reads config without restart
status: accepted
tags: comrade-tui, tui, keybindings, config, reload, M-x
summary: comrade-tui Ctrl-R (and M-x reload-config) re-reads the config file live: App::reload_config swaps cfg/client/tools Arcs, rebuilt via LlmClient::new + crate::build_tools.

## Context
Users wanted to change config.toml (llm model, [[delegates]], autonomy, budget) without restarting the TUI. Commit 24f5630. Before this, Deps/App held Arc<Config>/Arc<LlmClient>/Arc<ToolRegistry> built once in main.rs build_deps; delegates only become tools via DelegateTool in build_tools, so a reload had to rebuild the whole tool registry.

## Decision
1. Deps gained config_source: Option<PathBuf> (LoadedConfig.source, the path Config::load actually read; None = defaults) and auto_forced: bool (cli.auto, re-applied after reload).\n2. App stores both; App::reload_config() is called from handle_event on Ctrl-R and from run_command(MxCommand::ReloadConfig) (also in MxCommand::ALL/name/keys=C-r/desc).\n3. reload_config refuses while self.running (meta msg). It re-runs comrade_core::Config::load(config_source), preserves cli --auto, carries the startup-detected llm.context_window/model_version over when the new config leaves them None, then LlmClient::new(&cfg.llm)? and crate::build_tools(&cfg)? (private fn in main.rs, visible as crate::build_tools). On any error it pushes a Meta chat line and keeps the old config.\n4. On success it updates ctx_base.auto_approve = cfg.auto_approve(), ctx_budget = cfg.effective_budget(), then swaps self.cfg/client/tools Arcs, and pushes a Meta line with the source path. Future runs use the new registry/delegates.\n\nVERIFY: cargo check + cargo test --workspace green; Ctrl-R in TUI prints \"config reloaded from <path>\"; clippy only pre-existing warnings.

## Consequences
Reload is best-effort: an in-flight run keeps its old config; the seeded history/system-prompt ContextManager is not rebuilt; balance/model_version re-fetch at startup is skipped on reload. Next keybinding must also be added to MxCommand::ALL/name/keys/desc/run_command (they are exhaustive matches).

