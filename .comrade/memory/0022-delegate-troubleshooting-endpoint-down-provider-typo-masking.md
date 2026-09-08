# 0022 - Delegate troubleshooting: endpoint down + provider typo masking
status: accepted
tags: delegate, config, troubleshooting, llm
summary: Delegate failures: first check the delegate endpoint is up (they share one server); provider typo is masked by explicit base_url

## Context
While debugging "delegate tool broken" (sub-agents can't do any work / stop working), found the user's ~/.config/comrade/config.toml points BOTH delegates (bonsai-27b, mistral) at http://localhost:1234/v1/ (LM Studio), which was down (connection refused) — any delegation fails at the first LLM request. Their entries also had provider = "opeani" (typo for "openai").

## Decision
Troubleshoot delegation failures in this order: (1) confirm the delegate endpoint answers: curl -s -m 5 http://<base_url>/models; delegates share one server so they all fail together. (2) Check [[delegates]] provider spelling only matters when base_url is omitted — with an explicit base_url an unknown provider is silently accepted (config.rs fill_provider_base_url). (3) If the endpoint is up but delegates loop on one tool call, suspect the run_delegate_subagent no-progress guard (fixed in decision #21) or a model that cannot do native tool calling.

## Consequences
If a user reports delegates doing nothing / stopping: first check `curl -s -m 5 <delegate base_url>/models` (their [[delegates]] entries share base_url — all fail together when one server is down). Note provider typos like "opeani" are silently ignored when base_url is set explicitly (config.rs fill_provider_base_url returns early on has_explicit_url) but become a hard config error ("unknown provider ... Known providers: ollama, openai, ...") when base_url is omitted.

