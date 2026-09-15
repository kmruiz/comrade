# 0059 - Never split an assistant tool-call turn: ContextManager merges nudges into the trailing user or tool result
status: accepted
date: 2026-09-15
tags: context, llm, lm-studio, small-model, bugfix
summary: Agent-loop nudges/steers are appended by ContextManager::push_user_merged, which merges into a trailing user message or tool result and never creates a second consecutive user turn or an assistant-tool-call turn without its results.

## Context
Against LM Studio (Mistral chat template) the run died with HTTP 400 "Cannot continue an assistant message that contains tool calls." and 500 "Jinja Exception: After the optional system message, conversation roles must alternate user and assistant roles except for tool calls and results." Cause: the loop's stall/verify nudges were pushed as fresh user messages, producing either two consecutive user turns or a user turn right after a `Role::Tool` result, and the tool result itself was sometimes never appended because the nudge was pushed first.

## Decision
All agent-loop nudges and steering notes go through ContextManager::push_user_merged (crates/comrade-core/src/context.rs): it appends onto a trailing Role::User message, or onto a trailing Role::Tool result, and if the last turn is an assistant message with pending tool calls it neither merges into it nor leaves it unanswered. The invariant is that the history always satisfies the template's role alternation.

## Rationale
The history is a single shared object; enforcing the invariant in one place beats every nudge site remembering the rule, and it is exactly the invariant the local template enforces.

## Alternatives considered
(1) Push a user message and hope the provider tolerates it - the cloud providers do, LM Studio and the strict Mistral template do not. (2) Coalesce messages only when serialising to the API - rejected: the in-memory history would still diverge from what a ReAct text render produces, and compaction reads it too.

## Scope
ContextManager message assembly and every nudge/steer in the agent loop and the delegate sub-agent loop. Pinned by the `user_notes_never_double_up_the_user_role` test.

## Impact
Local OpenAI-compatible endpoints with a strict role-alternation template no longer 400/500 on a nudge. Any new nudge must use push_user_merged rather than pushing a User turn directly.

