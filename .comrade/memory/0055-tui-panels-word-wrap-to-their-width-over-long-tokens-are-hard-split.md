# 0055 - TUI panels word-wrap to their width; over-long tokens are hard-split
status: accepted
date: 2026-09-14
tags: tui, rendering, wrapping
summary: TUI panels word-wrap to their width (sensors queue and background-jobs list were truncating) and the shared wrappers hard-split any token longer than the panel.

## Context
GitHub issue #2 (bug, reported on v0.5.0): long model/sensor/plan/background-job descriptions overflow the right-hand panels and are unreadable; the documented repro is starting a background job with a long command. The plan panel and the model panel's delegate block already wrapped, so the two truncating panels were the sensors queue (`truncate_preview` to one line) and the background-jobs list (`.take(budget)` on the command). Separately, the chat/tool rows word-wrapped on spaces only, so a single token longer than the panel (a long URL, path or command) was clipped at the buffer edge.

## Decision
Every right-hand TUI panel wraps to its width instead of truncating, and no rendered row may exceed the available width. Concretely, in crates/comrade-tui/src/tui.rs: (a) the shared wrappers `plain_wrap`, `wrap_toks` and `wrap_styled` hard-split any word longer than the width via the existing `hard_cut` helper (clamped to width >= 1); (b) the background-jobs panel renders each job with `job_lines(job, width)` (id, command, status/duration, word-wrapped) and its panel height is the summed wrapped row count; (c) the sensors queue renders each entry with `sensor_toks(e)` through `wrap_toks`, tracks the start row of each entry so the selection highlight covers all its rows and the scroll keeps the whole entry in view, and its height is likewise the summed wrapped row count; (d) the now-unused `truncate_preview` was deleted. Both panel heights are capped by the available height of the right-hand column.

## Rationale
Wrapping at the shared primitives fixes the reported cases and prevents the whole class of overflow bugs (any panel reusing `plain_wrap`/`wrap_toks`/`wrap_styled` now hard-breaks long tokens), while keeping the panels' existing per-row styling, selection highlight and scroll behaviour intact.

## Alternatives considered
(1) Keep truncating with an ellipsis (`truncate_preview`, `.take(budget)`) — rejected: the issue is exactly that a long command/goal becomes unreadable. (2) Two-line cap with ellipsis — rejected as a half-measure: it still hides the tail of a long command. (3) Rely on ratatui's `Paragraph::wrap` — rejected: the panels build pre-styled `Line`s (per-row selection highlight, scroll offsets, search bands) and need the wrapped row count before drawing to size the panel; `wrap_styled`/`wrap_toks`/`plain_wrap` already existed and are reused.

## Scope
The TUI panel renderers in crates/comrade-tui/src/tui.rs and the shared text-wrapping helpers they use. Does not change the deferred done-step collapse line (deliberately one ellipsised row), the model-name label, or the markdown table renderer.

## Impact
Long commands, sensor names and summaries are now fully readable. Trade-off: a panel can grow to show a wrapped command, so the plan panel shrinks on small terminals (capped by the column height). Adding a panel's content now means adding its row count to `jobs_h`/`sensors_h`, otherwise the last rows are clipped. Follow-up: the model panel's model label and the "collapsed done step" line still truncate with an ellipsis on purpose (one-line checklist look); revisit only if it becomes a complaint.

