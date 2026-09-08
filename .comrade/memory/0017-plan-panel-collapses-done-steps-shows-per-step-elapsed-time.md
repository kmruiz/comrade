# 0017 - Plan panel collapses done steps + shows per-step elapsed time
status: accepted
tags: comrade-tui, plan, timing, PlanStep, session, draw_plan
summary: Plan window: done steps collapse to one line "+ N. goal  [model]  · 2m 5s" with per-step timing recorded in the PlanStep model (started_at_ms/took_ms via update transitions)

## Context
User: "In the plan window, when a task is done, collapse it and leave it so it only uses one line. Also add how much time it took to finish when it's done." Implemented across three crates. The plan model (comrade-tool PlanStep) previously had no timing; the TUI plan panel (draw_plan) rendered every status identically (goal + note + verify lines).

## Decision
1. Timing lives in the model, not the UI: PlanStep (crates/comrade-tool/src/plan.rs) gained `started_at_ms: Option<u64>` and `took_ms: Option<u64>` (wall-clock ms since UNIX epoch; `#[serde(default)]`, Serialize-only struct) plus two transition methods: `update(status, note)` (uses real clock) and `update_at(status, note, now_ms)` (injected clock, deterministic tests). Timing rules in update_at: note-only transitions (same status) early-return after note set; -> Pending clears started/took; -> InProgress from Done/Blocked restarts then records first start; -> Done/Blocked freezes took = now - started (None if never started). Also `pub fn now_ms()` helper.
2. All real transitions now route through PlanStep::update: comrade-core/src/session.rs update_plan (previously set fields directly) and finish_plan. The comrade-tool-session test StubSession (lib.rs ~580) mirrors this so behaviour matches in its tests. PlanStep literals (session.rs set_plan, tool-session stub with_plan) init both fields to None.
3. Rendering (crates/comrade-tui/src/tui.rs draw_plan): a Done step no longer prints note/verification lines. Instead pure helper `collapsed_done_toks(step, width) -> Vec<Tok>` builds ONE non-wrapping line: "+ N. <goal capped to fit>  [model]  · <fmt_dur_ms(took_ms)>" (duration only when took_ms is Some; fmt_dur_ms gives "420ms"/"3.5s"/"2m 5s"). Goal budget = width - fixed - 1 (reserving the cap() ellipsis). Other statuses keep their old multi-line rendering. PlanStatus text_color var simplified (White for all non-done).
VERIFY: cargo test workspace green (87 core + 9 comrade-tool incl. 5 new update_at timing tests + comrade-tui 79 incl. 4 new collapsed_done_toks tests). cargo fmt --all clean.

## Consequences
Trade-offs: (a) The note and verification text of a Done step are permanently hidden from the panel (goal is truncated too, "…"), so a long goal is not fully readable after completion — revisit if detail-on-demand (click/M-x to expand a done row) is wanted later. (b) A step that reaches Done/Blocked without ever being InProgress (e.g. finish_plan force-finishes Pending steps) shows no duration. (c) Delegated steps that fail to run restore status to Pending, which clears partial timing (update rules) — intended, so aborted attempts aren't counted; note text still records the failure. (d) clippy warnings in comrade-tui are all pre-existing (type_complexity on layout_chat_rows return ~2965/2978 from run book #16, collapsible_if, etc.).

