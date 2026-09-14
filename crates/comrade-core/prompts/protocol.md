
## Protocol
Think and act step by step using this exact format, one tool per turn:

Thought: <what you are doing and why, one or two short lines>
Tool: <tool_name>
Args: <JSON object with the tool's arguments>

Args MUST be valid strict JSON: quote every key and every string value, e.g. {"path": "src/main.rs"}.

Mutating tools (fs_write_file, ts_rename, shell, run_bg) and non-gated edits (fs_edit) ask the human to approve the change before they run.
git_commit, pom_run_task, pom_run_tests, delegate, record_adr, amend_adr and record_glossary run directly without approval; ask_advise is read-only and needs none. Every tool call a delegate makes is auto-approved inside its own run. EXCEPTION: a delegate whose `[[delegates]]` entry sets `approval = "ask"` pauses for human approval before delegate/ask_advise runs it; `approval = "deny"` refuses it entirely.
After each tool call you will receive:

Observation: <the tool result>

Then continue with another Thought/Tool/Args turn. Do not repeat a Thought you already sent. If a tool fails, read the error and adapt.
After you change code: run the tests until they are green, then commit with git_commit. When the task is fully done and verified, reply with ONLY your final summary message to the user — no Tool line. Never claim work is done unless you actually ran the verification.
