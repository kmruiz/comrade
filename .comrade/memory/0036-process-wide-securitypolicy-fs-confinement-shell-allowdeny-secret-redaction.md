# 0036 - Process-wide SecurityPolicy: fs confinement, shell allow/deny, secret redaction
status: accepted
date: 2026-09-13
tags: security, policy, hooks, roadmap
summary: A process-wide comrade_tool::SecurityPolicy (built from [security], installed at startup) confines fs paths to the project root + extra_roots (symlink-safe), enforces a shell allow/deny list, and a Redactor scrubs secrets from tool output.

## Context
Before this work fs tools wrote anywhere the OS allowed and shell commands had no deny list. The approved harness roadmap (ADR #35) flagged workspace path confinement (B1), shell sandboxing (B2) and secret redaction (B3) as the top safety gaps. Tools are constructed before the config is threaded to them, so a decision was needed on WHERE the policy lives and how it reaches the tool crates.

## Decision
Add a single process-wide guardrail, `comrade_tool::SecurityPolicy { extra_roots, shell_allow, shell_deny }`, stored in a `OnceLock<RwLock<..>>` and read via `comrade_tool::policy()`. It is built from `[security]` (SecurityCfg::to_policy) and installed with `comrade_tool::set_policy` at run start (comrade-core run_agent_with_history) and at TUI startup (main.rs session_bundle). (B1) `comrade_tool::confine(root, base, user_path, policy)` lexically normalises the path, then canonicalises the longest existing ancestor and requires it under a canonicalised allowed root (project root + extra_roots), so `..` AND symlink escapes are rejected; comrade-tool-fs::resolve now delegates to it. (B2) `comrade_tool::check_command(cmd, policy)` refuses any command containing a deny string (deny wins) and, when `shell_allow` is non-empty, requires a prefix match; enforced by the `shell` and `run_bg` tools BEFORE the approval prompt. (B3) `comrade_core::Redactor` (from_env / with_secrets / none) scrubs known secret values and `sk-`/`ghp_`/`AKIA`-style tokens out of tool output just before it is truncated and streamed to the model/transcript.

## Rationale
A global avoids widening ToolContext everywhere while still being configured once from the user's config; the tool crates already depend on comrade-tool, so the check belongs there. Confining the canonicalised ancestor closes the symlink hole the old string-only check left open.

## Alternatives considered
Thread a policy through every ToolContext (cleaner but invasive, since ToolContext is constructed in ~30 places including tests) — rejected for now. Per-tool sandboxing with OS mechanisms (bubblewrap/firejail) — deferred (heavier, needs external binaries); the allow/deny list is the cheap first line. Path confinement via canonicalize-only (rejects non-existent paths) — rejected: tools create files, so we canonicalize the longest existing ancestor and re-append the suffix.

## Scope
Filesystem confinement, shell command allow/deny, and secret redaction. Does NOT implement an OS-level shell sandbox, network egress control, or a persistent audit log (B4 remains open).

## Impact
fs writes/reads can no longer leave the project root (plus configured extra roots), escaped symlinks included; shell/run_bg honour a deny list and optional allow list; credentials are redacted from tool output. Config: `[security] redact_secrets` (default true), `extra_roots`, `shell_allow`, `shell_deny`. Follow-up worth doing: apply the policy to delegate sub-agent tool calls as well, and an OS sandbox for shell.

