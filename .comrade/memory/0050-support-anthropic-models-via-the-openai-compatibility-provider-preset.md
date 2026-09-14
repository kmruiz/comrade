# 0050 - Support Anthropic models via the OpenAI-compatibility provider preset
status: accepted
date: 2026-09-14
tags: llm, provider, config, anthropic

## Context
Request: "Add support for anthropic models using an API key." Comrade speaks to every provider through one OpenAI-compatible LlmClient (Bearer auth, /chat/completions, native tool calls), and provider names resolve to preset base URLs in config::provider_base_url. ADR #0039 had already rejected the native Anthropic content-block format (cache_control) to keep the single OpenAI-compatible client. Anthropic ships an official OpenAI-compatibility layer at https://api.anthropic.com/v1 that speaks /chat/completions with the OpenAI SDK's Bearer api_key.

## Decision
Add `anthropic` (with `claude` as an alias) to provider_base_url -> https://api.anthropic.com/v1, so `[llm] provider = "anthropic"` + `api_key = "sk-ant-..."` routes Claude models through the existing OpenAI-compatible client with no new client code. Also add a `claude` -> 200_000 fallback in llm::context_window::heuristic_context (the compat layer may not advertise /models context), and document the provider in README.

## Rationale
Consistent with the project's single-OpenAI-compatible-client architecture and ADR #0039; a working Claude integration in ~30 lines versus a large new module. The compat layer is documented as functional and non-breaking; its caveats (no prompt caching, non-strict tool schema, system-message hoisting, temperature capped at 1) are acceptable for this agent's use.

## Alternatives considered
Native Anthropic Messages API client (new module: /v1/messages, x-api-key + anthropic-version headers, content/tool_use block translation, SSE parsing, ~500 lines) — rejected for now as a much larger, riskier change; Anthropic itself labels the compat layer test-oriented but non-breaking. Doing nothing / requiring a custom base_url — rejected: no discoverability and no preset.

## Scope
Config provider presets, the context-window heuristic fallback, and docs. Not a native Messages API client; no special handling of Anthropic-only features (prompt caching, thinking, citations) and no env-var expansion for llm.api_key.

## Impact
Users can run Claude (and delegate to it) with just `provider = "anthropic"` + an API key. Follow-up if reliability/features matter: implement a native /v1/messages client and honour `[llm] prompt_caching` with Anthropic cache_control. The compat layer drops prompt caching, so `prompt_caching = true` is a no-op there.

