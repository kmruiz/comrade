# 0020 - comrade-tui plan panel: constant title + spinner/tick/cross glyphs
status: accepted
tags: comrade-tui, tui, plan-window, glyphs, spinner, ratatui
summary: comrade-tui plan panel: title always " plan "; glyphs are braille spinner (in-progress, wall-clock animated via 100ms run-gated redraw tick), \u{2713} done, \u{2717} blocked; pending stays "-"

## Context
User UI pass on the plan panel in crates/comrade-tui/src/tui.rs: drop the " plan ok " header (finished state still reaches the user as a chat meta via AgentEvent::PlanFinished), and give plan steps real status glyphs. The TUI redraws only on events, so an animated spinner needed a periodic wake-up in run()'s tokio select loop.

## Decision
draw_plan title is now always " plan ". Status glyphs come from pure fn plan_glyph(status, now_ms): Pending "-", InProgress = braille SPINNER_FRAMES[8] frame advanced every SPINNER_FRAME_MS=100ms by wall clock, Done "\u{2713}", Blocked "\u{2717}". run() creates a 100ms tokio::time::interval with MissedTickBehavior::Delay and selects on `_ = spin.tick(), if app.running => {}` so the plan spinner rotates during silent tool calls; the loop's shared redraw below the select renders it. collapsed_done_toks and the done-collapse tests updated from "+" to "\u{2713}". Unit test plan_glyphs_tick_cross_dash_and_spinner. VERIFY: cargo test -p comrade-tui (82 tests green); visual check: run comrade-tui, start a plan, watch the yellow braille glyph rotate in the plan panel even while a tool runs silently.

## Consequences
Glyphs are unicode width-1 (braille, \u{2713}, \u{2717}) so wrapped plan rows keep constant width; if a font lacks them they degrade. The 100ms interval only wakes while app.running (guard), so idle CPU is unchanged; draws are cheap thanks to the chat row cache (#19). PlanStatus has NO Failed variant — "failed" is Blocked, which renders the red \u{2717}.

