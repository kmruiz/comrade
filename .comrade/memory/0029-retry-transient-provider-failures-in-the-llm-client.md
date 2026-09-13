# 0029 - Retry transient provider failures in the LLM client
status: accepted
date: 2026-09-13
tags: llm, resilience, comrade-core
summary: LlmClient retries transient send failures (transport errors incl. connection reset, and 408/425/429/500/502/503/504/529 statuses) with exponential backoff, configurable via [llm] max_retries/retry_backoff_ms.

## Context
A single connection reset, timeout, 429 or 5xx from the provider aborted the whole chat turn with a hard error, so the agent loop or delegate stopped and the human had to re-issue. The provider is remote and transient hiccups are routine, so the client - the only layer that knows what a retryable transport failure looks like - should absorb them.

## Decision
In `crates/comrade-core/src/llm.rs` the request paths retry transient failures.
- `send_once` POSTs one request and turns a non-success status into a typed `LlmHttpError` (no longer a formatted string) so it can be classified.
- `is_retryable` walks the anyhow chain: a `reqwest::Error` that is timeout/connect/request/body is retryable, as is a status in 408|425|429|500|502|503|504|529. Everything else (other 4xx, malformed JSON) is permanent.
- `chat` and `chat_turn_once` go through `send_with_retry`; the streaming `chat_turn` retries the send/status phase AND a stream that died before any content delta was emitted (a reset after the first token is not retried, to avoid duplicating what was already shown to the user).
- Backoff is exponential (base = `retry_backoff_ms`, doubling per attempt, capped at 8s).
- Config gains `[llm] max_retries` (default 2, i.e. up to 3 attempts) and `[llm] retry_backoff_ms` (default 500).

## Rationale
The LLM client is the only layer that can classify a transport error or HTTP status and re-issue the identical request, so retrying there keeps callers (agent loop, ReAct adapter, delegate sub-agent) unchanged. Retrying only what is provably transient avoids looping on a bad key or bad request, and refusing to retry once streamed output has started avoids duplicated tokens.

## Alternatives considered
(a) Retry in the agent loop/delegate instead - rejected: they cannot see the typed cause, and a partially streamed turn cannot be re-run without duplicating already-shown output. (b) Retry every failure - rejected: a 401/400 or malformed response would spin uselessly. (c) Retry forever with jitter - rejected: it hides a sustained outage instead of surfacing it after a bounded number of tries.

## Scope
Covers the four chat entry points of `LlmClient` (chat, chat_turn_once, chat_stream, chat_turn). Probe helpers (`fetch_model_version`/`fetch_account_balance`/`fetch_context_window`) are best-effort and unchanged. Does not add circuit-breaking or a jittered schedule.

## Impact
A transient blip no longer fails a turn or a delegation. Trade-off: a genuinely down provider now waits through the retries first (default 3 attempts, backoff 0.5s+1s) before erroring - small next to the 600s request timeout. Tests: `mod retry_tests` in llm.rs runs a loopback flaky server (503/429 then 200) and asserts an unchanged 400 is not retried.

