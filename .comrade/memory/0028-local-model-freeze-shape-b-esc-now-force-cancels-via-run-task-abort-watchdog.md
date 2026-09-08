# 0028 - Local-model freeze (shape b): Esc now force-cancels via run-task abort watchdog
status: accepted
tags: tui, freeze, cancel, CancellationToken, local-model, lmstudio, watchdog
summary: Esc-cancel of a run stuck on a stalled local LM Studio model: TUI cancel_run watchdog aborts the run task 1.5s after cancel and emits idempotent RunEnd (commit 715b9cb)

## Context
User: with a local OpenAI-compatible LM Studio model the app sometimes "freezes" - can't type, can't exit, Esc can't cancel the job. Asked which shape: SPINNER STILL ANIMATES but keys/Esc do nothing = shape (b): UI task healthy, run task wedged in an await that ignores the CancellationToken. (#24 fixed delegates+root chat; #25 is the OTHER shape (a) whole-UI wedge, still unsolved, needs a gdb backtrace.) Root model chat IS raced/drop-safe (agent.rs select ~478, delegate.rs ~616), but these run-path awaits still ignore stop: tool.invoke (no race), bounded events-channel tx.send/forwarder drain (agent.rs delta forwarder), post-run fetch_account_balance() in the TUI run task (tui.rs start_run). Commit 715b9cb fixed it.

## Decision
1. TUI App gained run_handle: Option<JoinHandle<()>> (tui.rs App struct ~567); start_run stores the spawn handle. 2. cancel_run (tui.rs ~1034) additionally spawns a watchdog: polls handle.is_finished() every 50ms; if the run task has not ended 1.5s after stop.cancel(), calls handle.abort() and sends a synthetic AgentEvent::RunEnd so running=false and the UI regains control - Esc ALWAYS ends the run now, whatever await wedged it. 3. RunEnd handler is idempotent (fold+meta only when was_running) so the watchdog's duplicate RunEnd is harmless; it also clears run_handle. 4. agent.rs: on an errored/interrupted turn the loop returns WITHOUT awaiting the delta forwarder (it can be parked on a full UI channel; dropping delta_tx lets it exit) - previously the interruption itself hung. Verify: cargo test workspace green (96 core tests); no new clippy diagnostics in edited files.

## Consequences
Abort can kill a tool mid-invoke (e.g. a write) if the user force-cancels; acceptable for an explicit cancel. Normal completions never spawn the watchdog. If the freeze report recurs as shape (a) (spinner AND keys dead), that is #25 - capture `gdb -p <pid> -batch -ex 'thread apply all bt'` before killing; do not guess. The remaining unwatched awaits (tool.invoke, balance fetch) are now covered by the abort, not by graceful token checks - a future hardening could race them with stop.

