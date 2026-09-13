# 0044 - Make the plan panel scrollable
status: accepted
date: 2026-09-13
tags: tui, plan, scroll
summary: The plan panel scrolls independently of the chat via App.plan_scroll, driven by the mouse wheel over the panel and PageUp/PageDown (M-x scroll-plan-up/down).

## Context
The plan panel (right sidebar, draw_plan in crates/comrade-tui/src/tui.rs) rendered every step as a wrapped paragraph with no scrolling, so a long plan (many steps, notes and verification lines) was clipped at the panel bottom and older steps could not be reached. The chat already had its own scroll model (App/LiveState scroll_top, wheel over chat_rect, follow/autoscroll).

## Decision
Give the plan its own vertical scroll offset, mirroring the chat's model: a per-session `plan_scroll: u16` field on both App (active session) and LiveState (parked session), swapped in `swap_live` like `scroll_top`. `draw_plan` now takes `&mut App`, records the whole panel rect in `App.plan_rect`, clamps the offset to the wrapped content height (`lines.len() - inner.height`) and renders `Paragraph::new(lines).scroll((plan_scroll, 0))`. Input: the mouse wheel over `plan_rect` scrolls it (handle_mouse, ±3 rows, consumed before the chat branch); PageUp/PageDown in the main key handler call `App::scroll_plan(∓3)`; and two M-x commands `scroll-plan-up`/`scroll-plan-down` (keys pgup/pgdn) do the same. `scroll_plan` uses saturating arithmetic; the draw clamps to content.

## Rationale
Reusing the existing per-session scroll pattern (App + LiveState + swap_live) keeps plan scroll state correct across session switch/park without new plumbing, and mirrors the chat so behaviour is predictable. Clamping in the draw (where the content height is known) rather than in the input handler avoids needing the viewport at input time and makes the offset self-correcting.

## Scope
Applies to the TUI plan panel only (crates/comrade-tui/src/tui.rs). Does not add horizontal scrolling, does not auto-follow the active step, and does not change the chat, jobs or model panels.

## Impact
Long plans are now fully reachable. New per-session field must be initialised at all App/LiveState construction sites (it is: App::new live-state literal, the two LiveState literals, build_app's App literal). Tests: tui::tests::scroll_plan_saturates_at_zero_and_moves and tui::tests::mx_scroll_plan_commands_metadata.

