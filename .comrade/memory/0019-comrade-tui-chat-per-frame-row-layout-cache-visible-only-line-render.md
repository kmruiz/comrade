# 0019 - comrade-tui chat: per-frame row-layout cache + visible-only Line render
status: accepted
tags: comrade-tui, tui, chat-layout, performance, ratatui, cache
summary: comrade-tui chat perf: App-level ChatRowsCache keyed on (epoch,width) reuses the chat row layout between frames; draw_chat renders only visible rows. ~200x faster frames on long transcripts.

## Context
Complaint: chat window is slow with many messages. Measured (release, 150 msgs -> 1950 rows): every draw rebuilt the FULL transcript layout — layout_chat_rows re-tokenises/wraps every message body via md_to_lines — then draw_chat built a Line for EVERY row (~2.6ms/frame); frames run after each terminal event AND each agent event (streaming deltas), so a long session stutters. Fixed in crates/comrade-tui/src/tui.rs.

## Decision
App now holds a ChatRowsCache (rows/owner/ranges + epoch + width) built by pure layout_chat_rows over chat ONLY (stream preview excluded, relaid fresh per frame). App.chat_epoch (u64) is bumped inside every chat-content/collapse mutator — push_msg, last_tool_mut, expand_section_at, toggle_section_at, toggle_tool, expand_run, fold_completed, goto_search_match — so those rare frames rebuild; all other frames (typing, scroll, sel, search nav, streaming Deltas which only touch app.stream) reuse the cache. draw_chat renders ONLY rows in [offset, offset+height): cached RenderRows through render_row_line (band/select/match logic unchanged) and preview rows inline ("  " + spans, never banded) — Paragraph gets the visible slice with scroll (0,0) instead of the whole history with scroll=offset. Visible-only rendering keeps per-frame cost O(screen); cached layout makes chat frames ~2.6ms -> ~12us (200x) at 150 msgs. Pure layout_chat_rows and its unit tests are untouched; layout_messages wrapper was deleted (only draw_chat used it). Unit tests: chat_cache_reuses_rows_while_epoch_and_width_are_stable / chat_cache_relayouts_on_width_change_and_epoch_bump.

## Consequences
1) Any NEW code path that mutates chat content or section_collapsed must bump App.chat_epoch or it will render stale rows (audit list above is exhaustive today). 2) The stream preview is still md_to_lines'd in full every frame while streaming (bounded by the 40k-char stream cap) — if very long single answers lag, cache the preview keyed on prefix-length next. 3) Search keystrokes call goto_search_match which bumps epoch -> one full relayout per search keypress (no worse than the pre-cache behaviour). 4) A live terminal visual check is still worth doing (selection/search/match highlights + user band on a long transcript, resize, collapse/expand).

