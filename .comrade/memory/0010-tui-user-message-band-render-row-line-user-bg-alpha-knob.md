# 0010 - TUI user-message band: render_row_line + USER_BG_ALPHA knob
status: accepted
tags: comrade-tui, tui, chat-layout, theme, ratatui
summary: User chat messages now render as a padded light band (USER_BG_ALPHA knob); render_row_line() centralizes per-row styling for future agent-message bands

## Context
Emacs-y/org-mode chat styling was started in crates/comrade-tui/src/tui.rs. User messages are now a soft lighter band (dark-theme): each body row of a MsgKind::User message gets bg user_band_bg() (DIFF_BASE_BG 0x13,0x14,0x18 lightened by white at USER_BG_ALPHA=0.08, reusing blend_rgb) plus the existing cyan left rule "| ". Agent/assistant/delegate messages are NOT yet banded — that is the next planned step.

## Decision
1. Row rendering is centralized in fn render_row_line(r: &RenderRow, sel: bool, in_match: bool, band: Option<Color>, width: usize) -> Line (defined just above draw_chat ~tui.rs:1935). It sets bg on the 2-cell gutter ("| ", "> " when selected, "  " otherwise) and on every span, then pads the span area with bg'd spaces to `width` so the band is flush to the border. 2. draw_chat computes band per row via app.row_msg[row] -> chat index -> m.kind == MsgKind::User (stream preview rows have no owner, so never banded). 3. To band other message kinds later, extend the kind filter in draw_chat (User -> match on User/Assistant/Delegate) and tune USER_BG_ALPHA / add a second alpha const next to it in the diff-palette block (~tui.rs:3750) where DIFF_BASE_BG and blend_rgb live.

## Consequences
Terminals cannot report their theme to ratatui, so DIFF_BASE_BG is an assumed dark chat bg; on light themes USER_BG_ALPHA must be re-tuned (darker tint over a light base). Tests: user_band_pads_row_to_width_keeping_rule / unbanded_row_is_not_padded / selected_row_swaps_rule_for_marker / user_band_bg_lightens_the_chat_base in the tui.rs tests module.

