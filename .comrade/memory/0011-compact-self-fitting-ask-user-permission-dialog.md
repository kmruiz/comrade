# 0011 - Compact self-fitting ask_user / permission dialog
status: accepted
date: 2026-09-10
tags: tui, ux, tools
summary: Make the ask_user/permission dialog wrap to its width, size to content, and anchor top; nudge the model to one short question with <=4 options.

## Context
The question/permission dialog cut off its own text: the body was wrapped at a hard-coded 96 columns regardless of the (possibly narrower) popup, the height used body.len()+3 while the layout needs body.len()+4 (2 borders + input + hint) so the first line was always scrolled away, and the body was anchored to the bottom, hiding the top of the question.

## Decision
Keep the ask_user/permission dialog compact and self-fitting: wrap body and options to the popup's real inner width, size height to content (body+4) up to the terminal, and anchor the body to the top so the question/action is always visible. Correspondingly, the ask_user tool description and JSON schema instruct the model to ask one short question with at most 4 short options, since the modal space is limited.

## Rationale
Fixing the wrapping/sizing makes the whole question visible on any terminal; hinting the model at the tool boundary keeps the common case within the limited space without hard-truncating content.

## Alternatives considered
Render the body with Paragraph wrap + line_count; keep bottom-anchored scrolling; cap question/options at runtime by truncating (rejected: silently drops content the human needs).

## Scope
Covers the TUI dialog rendering and the ask_user tool spec. Does not enforce a hard limit at runtime; headless/other UserIo implementations are unaffected.

## Impact
Models are nudged (not forced) to keep questions/options short; long prompts still render but may be clipped if taller than the terminal. The tool contract (single question, <=4 options) is now a documented expectation future changes should preserve.

