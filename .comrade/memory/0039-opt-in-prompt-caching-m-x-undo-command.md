# 0039 - Opt-in prompt caching + M-x undo command
status: accepted
date: 2026-09-13
tags: llm, prompt-caching, tui, undo, roadmap
summary: Add opt-in prompt caching (Anthropic-style cache_control marker on the system message + last tool) and an M-x `undo` command that restores files from the session undo log.

## Context
Two small roadmap items: prompt-cache support (E2, Anthropic cache_control / provider caching) and binding the already-existing UndoLog to a user command (C3, core/undo.rs existed but was unreferenced from the TUI).

## Decision
(E2) Add `[llm] prompt_caching` (default false). When on, ChatRequest serializes messages as loose JSON and adds a `cache_control: {type: "ephemeral"}` marker to the system message and the last tool definition; providers that don't support it ignore the unknown field. (C3) Add an `MxCommand::Undo` ("undo", M-x only) that runs the active session's `ToolContext.undo.undo_last()` in a background task and reports how many files were restored.

## Rationale
Both are low-risk, opt-in additions that fit existing structures (the OpenAI-compatible request builder; the session undo log already wired into ToolContext).

## Alternatives considered
Anthropic native content-block format for cache_control (correct for Anthropic but breaks OpenAI-compatible servers that expect string content) — rejected: the client is OpenAI-compatible. Making caching always-on — rejected: unknown fields may be rejected by strict servers, so it stays opt-in. A dedicated undo tool exposed to the model — rejected: undo is a human recovery action, so a TUI command is the right surface.

## Scope
LLM request building and the TUI command palette. Not a general-purpose cache accounting feature.

## Impact
Opt-in prompt caching can cut cost/latency on providers that support it; `M-x undo` lets a human roll back the last mutating tool writes. No behaviour change when flags are left at defaults. Follow-up: verify cache markers are accepted by the specific provider in use (they are non-standard on OpenAI-compatible endpoints).

