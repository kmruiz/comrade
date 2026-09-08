# 0005 - Session context persists across tasks; auto-compaction near budget
status: accepted
tags: context, session, compaction, budget, tui
summary: TUI keeps one ContextManager per session: tasks append, not wipe; compaction trims at 90% of budget, not only past it

## Context
User complaint: finishing a task wiped the whole context (run_agent built a fresh ContextManager per prompt). Fix keeps context across prompts inside one running TUI session (explicitly NOT across restarts - disk persistence was declined for now). Automatic compaction near the budget was requested too: history was only trimmed once past the hard budget.

## Decision
1. Agent entry points now: build_session_context(cfg, project_root:&str, tools) -> ContextManager (index 0 = system prompt, budget = effective_budget), run_agent_with_history(cfg, client, ctx, tools, user_input, history:&mut ContextManager, tx, stop) which pushes the user msg, calls history.enforce_budget(), then run_agent_loop(&mut ctxm). One-shot run_agent(cfg,client,ctx,tools,user_input,tx,stop) = build fresh manager + run_agent_with_history, unchanged for headless/tests. All exported from comrade_core (lib.rs). run_agent_loop's ctxm param changed from owned ContextManager to &mut ContextManager.\n2. In comrade-tui, App owns history: Arc<tokio::sync::Mutex<ContextManager>> created once in tui::run from build_session_context(&cfg, root, &tools); start_run locks it inside the spawned task and calls run_agent_with_history. Runs are serialized by App.running so the guard never contends; holding it across the whole run is intentional.\n3. ContextManager compaction watermark (crates/comrade-core/src/context.rs): over_budget() now compares against trim_target() = budget - budget/10 when budget >= HEADROOM_MIN_BUDGET (4000), else plain budget. So history is rolled up ("Earlier context (compacted)", stub big old observations, keep recent window + system) as it APPROACHES the cap, leaving ~10% headroom for the model's answer/tool output. Delegate sub-agents and headless one-shots still get fresh contexts per run (unchanged).

## Consequences
Delegates still wipe their own sub-context per delegation - fine because each returns a deliverable into the main context. Plan/session state (AgentSession) already lived across runs and still does. Natural next step if wanted: disk-backed persistence per project dir for cross-restart continuity (declined this round). Tests to extend when touching: context.rs compaction_start_below_hard_budget / no_compaction_below_trim_target, agent.rs run_agent suite, tui start_run wiring (compile-only).

