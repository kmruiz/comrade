# 0030 - Freeze fix implemented: coalesce per-token deltas + cap TUI repaint rate (commit 7b9ee8d)
status: accepted
tags: tui, freeze, streaming, delta, perf, bugfix
summary: The #25/#29 "fast local run freezes after final reply" wedge is addressed in commit 7b9ee8d: agent delta forwarder now batches (1 Delta per 33ms/1024 chars) and uses try_send (never blocks run on full UI channel); TUI caps event repaints at 30fps while running.

## Context
#25/#29 suspected a fast local model (~120 tok/s, KV-cache hot) streaming one AgentEvent::Delta per token into the cap-512 events channel (main.rs:172): UI repainted per event and the run task could wedge awaiting the delta forwarder (old agent.rs send().await at turn end), so the app looked dead with no further model requests. Memories said capture gdb before guessing; no backtrace was ever captured, but the streaming-throughput fix is now in place.

## Decision
1. agent.rs run_agent_loop (was ~466-476): the spawned delta forwarder now drains the unbounded delta channel into a buf and ships one AgentEvent::Delta per 33ms interval tick or when buf >= 1024 chars, via events_tx.try_send (drop-on-full, never block). On delta channel close it flushes the tail and breaks; the run still awaits forwarder at turn end, which is now unblockable. Deltas are purely cosmetic: full text is committed by ToolCall/FinalAnswer events, so dropped batches lose no content. 2. tui.rs App gained last_draw: Instant (init in the run() App literal); the main select loop (tui.rs ~1940) skips terminal.draw when app.running && <33ms since last_draw. Idle frames and the 100ms spin tick (always >33ms apart) are never throttled. 3. .gitignore now has /logs.txt (98k-line LM Studio paste must not be committed). 4. Committed the previously-untracked memory files 0028/0029.

## Consequences
If the freeze still reproduces (shape (a): spinner AND keys dead = UI task wedged inside a single terminal.draw/layout call), the remaining suspect is a pathological/content-dependent md_to_lines or layout case (#25 suspect iii) - capture gdb -p <pid> -batch -ex 'thread apply all bt' while frozen and expect the UI thread in ratatui draw/layout. No gdb backtrace was captured for this fix; it is a targeted mitigation of the streaming-pressure hypothesis, not proof. Workspace tests are green (96 core).

