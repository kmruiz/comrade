# 0041 - Position semantic_search as a first-class orientation tool, not a fallback
status: accepted
date: 2026-09-13
tags: prompt, tools, semantic_search, agent-behaviour
summary: semantic_search is deliberately a first-class EARLY orientation tool (domain known, symbol/file not), not a last-resort fallback.

## Context
The agent's own tool guidance (crates/comrade-core/prompts/tools-intro.md, working-style.md, delegate-system.md) described semantic_search as a last-resort fallback, gated on "you do not know the exact word", and listed it last. In practice it was called only ~3 times per session, because that framing conditions it on a rarely-noticed state ("I don't know the word") while the common orientation case is "I know the domain but not the symbol/file". The human flagged the underuse.

## Decision
semantic_search is positioned as a first-class, EARLY orientation tool, not a fallback. It is the first choice whenever you know WHAT you want but not WHERE it lives (you can name the domain, not the exact symbol, file or string). tools-intro.md's "Only the meaning" bullet and working-style.md's default loop (step 1, and step 3's leading tool) now say so explicitly; the word "fallback" is gone from its description.

## Rationale
The prompt is the durable lever: sessions are ephemeral, so a behavioural promise changes nothing. Rewording the tool's positioning (WHAT you want known, WHERE it lives unknown) matches the actual orientation situation the agent hits at the start of most tasks and removes the narrow trigger that caused underuse.

## Alternatives considered
(a) Just use it more this session - no durable effect, rejected. (b) Restrict or re-rank the tool's results - unnecessary; the tool works, the guidance was the problem. (c) Also mirror the wording into delegate-system.md - deferred (out of scope of the chosen change); the delegate prompt still lists semantic_search last and is a candidate follow-up.

## Scope
Covers the model-facing prompt wording for semantic_search in tools-intro.md and working-style.md. Does not change the tool's behaviour, its ToolSpec description in crates/comrade-tool-memory/src/semantic.rs, or the delegate system prompt.

## Impact
The agent should reach for semantic_search at the start of exploratory tasks. Follow-up: align the same framing in crates/comrade-core/prompts/delegate-system.md so delegates orient the same way.

