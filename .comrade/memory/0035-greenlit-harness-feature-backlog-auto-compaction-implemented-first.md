# 0035 - Greenlit harness feature backlog (auto-compaction implemented first)
status: accepted
date: 2026-09-13
tags: roadmap, planning, harness
summary: Greenlit harness backlog across reliability, safety, UX, extensibility and observability; A1 auto-compaction implemented first (#34).

## Context
Asked 'what other features should we implement in the harness?' I surveyed the workspace (crates/comrade-*) and found the following gaps. The human greenlit the scope and chose auto-compaction (A1) to implement first; A1 is now implemented (see #34). The rest remain unimplemented.

## Decision
Approved feature backlog for the Comrade harness, grouped by theme. Reliability/control: A1 auto-compaction (DONE, #34), A2 cost & token budget (Usage is tracked but never priced; add $-estimate meter + optional hard cap), A3 per-tool timeout + global run time budget, A4 auto verify loop (after edits run pom_run_tests and feed failures back). Safety: B1 workspace path confinement for fs tools, B2 shell sandbox (bwrap/firejail) or command allow/deny list, B3 secret redaction in tool output, B4 append-only audit log of approvals + mutating calls, B5 read-only / plan mode blocking is_mutating tools. UX: C1 diff review pane + approve-all, C2 session export + resume of an interrupted run, C3 bind the existing UndoLog to an M-x undo command (core/undo.rs exists but is unbound), C4 finish notification, C5 slash-commands/prompt templates + fuzzy M-x palette. Extensibility: D1 hooks (pre/post tool), D2 git worktree isolation for parallel delegates, D3 plugin loader for non-MCP tools, D4 supervisor/team roles. Observability: E1 OTel/JSONL run trace export, E2 prompt-cache support (Anthropic cache_control etc.).

## Rationale
The harness is already deep on tools/UX; the biggest gaps are in reliability under long runs (compaction, cost control, verify loop) and hard safety guarantees (confinement, sandbox, audit). Those are where the highest leverage is.

## Alternatives considered
Recording nothing (lose the backlog) — rejected: the survey work would be redone next session. One giant ADR per feature — rejected: premature; most items have not been designed yet.

## Scope
Product/architecture direction for the harness. Not a design for any individual item except A1 (#34). Items may be split, re-scoped or dropped as they are designed.

## Impact
A prioritised backlog exists so future sessions can pick items without re-surveying. Each item names the subsystem it touches (crate/file hints above). No code impact beyond A1.

