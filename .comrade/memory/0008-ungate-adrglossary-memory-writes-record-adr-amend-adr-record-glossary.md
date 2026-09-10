# 0008 - Ungate ADR/glossary memory writes (record_adr, amend_adr, record_glossary)
status: accepted
date: 2026-09-09
tags: approval, memory, tools, autonomy
summary: record_adr, amend_adr and record_glossary are fully ungated: removed from APPROVAL_GATED_TOOLS and their ctx.confirm dialogs deleted, since they only write the agent's own .comrade/memory.

## Context
The ADR/glossary memory tools (record_adr, amend_adr, record_glossary) were double-gated like delegate once was (ADR #2): listed in APPROVAL_GATED_TOOLS so the loop demanded a Justification, and each tool called ctx.confirm before writing. Their writes touch only .comrade/memory/, the agent's own durable memory, so a human checkpoint was deemed unnecessary. The user asked to remove the approval gate for ADR and glossary tasks.

## Decision
record_adr, amend_adr and record_glossary are no longer approval-gated: removed from APPROVAL_GATED_TOOLS (agent.rs) so no Justification is required, and the ctx.confirm prompts inside each tool's invoke were deleted so they write .comrade/memory directly. They remain in MUTATING_TOOLS (still count as state changes for the loop tracker and read guard) and remain non-browsable to advisors (advise.rs). Tool descriptions, the memory crate module doc, and prompts/protocol.md were reworded from 'Approval-gated' to 'runs directly without approval'; protocol.md's approval-gated list is now just fs_write_file, ts_rename, shell.

## Rationale
Consistency with the delegate ungating (ADR #2): where the tool's side effects are confined to state the agent owns (.comrade/memory/), the human gate adds friction without protecting user files. Memory writes are also fully undoable via the same ctx.undo capture retained before each write.

## Alternatives considered
1) Keep the Justification gate and only delete the internal ctx.confirm dialogs - rejected: the user asked for a fully ungated write, and the ctx.confirm was the actual human pause, so removing only the classification would not stop the dialogs. 2) Gate on an autonomy config knob - rejected: overkill for writes confined to the agent's own .comrade/memory directory. 3) Leave approval-gated - rejected by request.

## Scope
Covers the three memory tools' approval classification in agent.rs (APPROVAL_GATED_TOOLS), their internal ctx.confirm prompts in comrade-tool-memory/src/lib.rs, the tool descriptions/module doc, and prompts/protocol.md wording. Does not change MUTATING_TOOLS membership, the advisor deny-list (advise.rs), delegate access to these tools, or the other approval-gated tools.

## Impact
The agent can now record and amend ADRs and glossary terms with no human checkpoint, matching git_commit/pom_run_tests/delegate. The remaining approval-gated tools are fs_write_file, ts_rename and shell. If unattended memory writes ever need a checkpoint, a config knob (like the delegate approval policy in ADR #3) is the follow-up.

