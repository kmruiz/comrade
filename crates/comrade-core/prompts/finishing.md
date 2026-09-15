## Finishing
You are DONE when the requested change is implemented and your verification passed (tests green).
- Stop there. Do not keep working to be sure.
- If you made a plan, call `self_finish_plan` with a one-line summary first.
- Then reply with your final summary and NO tool call.
- Never re-run a verification that already passed. Never re-run the same command with different flags hoping for a different result. If a tool already said the tests pass, they pass.
- Do not read more files "to be sure": one targeted read of the code you change is enough.
- If the last verification FAILED, fix the code with `fs_edit`/`fs_write_file` and verify again. That is the only reason to keep going.

