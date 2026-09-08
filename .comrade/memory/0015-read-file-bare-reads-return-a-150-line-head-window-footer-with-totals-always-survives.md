# 0015 - read_file: bare reads return a 150-line head window, footer with totals always survives
status: accepted
tags: tool-fs, read_file, context-economy, prompt-tools
summary: read_file bare reads now capped to 150-line head window with always-visible total-line footer

## Context
Complaint: agents call read_file without a window on big files, wasting context. Root causes found in crates/comrade-tool-fs/src/lib.rs: a bare read returned the whole file clamped at MAX_OUTPUT_CHARS=6000, and the clamp cut the 'N lines' footer off, so models never learned file size or how to window.

## Decision
1. Bare read_file (no start_line/end_line) on a file > MAX_UNWINDOWED_LINES=150 now returns only lines 1..150; explicit windows (start and/or end) are honoured as before. 2. Content is clamped BEFORE appending the footer so '-- a..b of N lines --' is always visible; when the default cap kicked in, the footer adds '(file longer than the 150-line default window: pass start_line/end_line or use read_ranges to read the rest)'. 3. READ_FILE_SPEC description now tells models that bare reads of long files return a head window and to prefer windows/read_ranges/read_symbol/structural_map.

## Consequences
Any agent expecting a whole big file from a bare read_file now only gets the first 150 lines + totals; tell it to pass start_line/end_line (uncapped but still 6000-char clamped) or use read_ranges to continue. Small files (<=150 lines) are unchanged, and the footer of every read_file now always survives the 6000-char clamp because content is clamped before the footer is appended.

