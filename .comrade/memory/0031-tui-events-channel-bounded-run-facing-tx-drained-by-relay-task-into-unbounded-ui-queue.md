# 0031 - TUI events channel: bounded run-facing tx drained by relay task into unbounded UI queue
status: accepted
tags: tui, freeze, channels, non-blocking, agent-events, architecture
summary: comrade-tui main.rs new_session now spawns spawn_event_relay: run task streams AgentEvents into cap-512 mpsc tx (awaited sends in core), a dedicated tokio task drains rx and forwards to an unbounded channel consumed by the UI select loop (App.events_rx is UnboundedReceiver).

## Context
Freeze notes #25/#29/#30 (see .comrade/memory/): a wedged/busy UI repaint filled the cap-512 events channel and parked the run task on tx.send at turn end, making the app look dead on fast local runs. 7b9ee8d made the delta forwarder try_send + capped repaints at 30fps; this change removes the remaining class where a UI that is merely slow can back-pressure the run task.

## Decision
1. main.rs::new_session: keep the bounded (tx,rx) channel(512) that AgentSession/run_agent_with_history send into, but do NOT hand rx to the UI. Spawn spawn_event_relay(rx, ui_tx) where ui_tx is unbounded; the relay loop is `while let Some(ev) = rx.recv().await { if ui_tx.send(ev).is_err() { break; } }`. Return (bundle, tx, ui_rx) from new_session. 2. App.events_rx type is now mpsc::UnboundedReceiver<AgentEvent> (tui.rs ~563); events_tx stays mpsc::Sender for the run/watchdog/balance. 3. Post-run balance send uses try_send (cosmetic, never parks the run). 4. Watchdog RunEnd (tui.rs ~1063) still awaits its send to the bounded tx: it is load-bearing (UI must clear "running") and the relay drains it in microseconds. 5. All channel ops that can wait are now on background tasks/threads (relay, watchdog, git spawn, ask on run task, crossterm std::thread); the UI select task only does sync unbounded/oneshot sends and async recv inside tokio::select. VERIFY: cargo check + `cargo test --manifest-path=crates/comrade-tui/Cargo.toml` (86 tests incl. event_relay_keeps_awaited_senders_unblocked which fires 2000 awaited sends through a cap-4 channel and asserts all arrive at the UI queue).

## Consequences
Trade-off: when the UI task is genuinely wedged the unbounded UI queue can grow (memory) instead of applying back-pressure to the run — accepted per the freeze history (the model must keep making progress). Ordering is preserved (single relay consumer). clippy on comrade-tui currently fails on PRE-EXISTING lint errors in comrade-tool-syntax (untouched crate), not on this diff.

