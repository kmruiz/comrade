# 0094 - fs_read_file returns the whole file; the caller bounds reads, not the tool
status: accepted
date: 2026-09-27
tags: tools, comrade-tool-fs, read-file, context-budget
summary: fs_read_file caps nothing: a bare read returns the whole file, and callers bound reads with start_line/end_line or fs_read_ranges; observation truncation stays in the agent loop.

## Context
fs_read_file used to cap both the line count of a bare read (MAX_UNWINDOWED_LINES = 150) and the rendered output (MAX_OUTPUT_CHARS = 6000 via clamp()). With the line cap gone the char clamp became incoherent: the content was silently cut while the footer still claimed "-- 1..N of N lines".

## Decision
fs_read_file does NOT cap its own output. A bare read (no start_line/end_line) returns the whole file, unbounded; reading in batches is the caller's job via start_line/end_line or fs_read_ranges. Output capping for this tool lives solely in the agent loop (ContextManager::truncate_observation in comrade-core). Listings, searches and explicit-window reads keep MAX_OUTPUT_CHARS (6000) via the shared clamp() helper in comrade-tool-fs.

## Rationale
The caller knows how much it wants to read and has window/range tools to say so; a silent local cap only hides information and lies in the footer. The observation budget is already enforced once, centrally, in the agent loop, so a second cap in the tool is redundant and inconsistent.

## Alternatives considered
Keeping a 150-line head cap + 6000-char clamp in fs_read_file (rejected: the caller cannot tell a truncated read from a whole one when the footer still reports every line, and a bare read of a big file is exactly what the caller asked for). Requiring explicit windows on fs_read_file (rejected: makes the common one-line read clumsy).

## Scope
Covers fs_read_file only (crates/comrade-tool-fs/src/lib.rs). Does not change fs_read_ranges, fs_list_dir, fs_list_files, fs_rgrep or any other tool's clamping.

## Impact
A bare read of a large file can put tens of thousands of characters into the observation, which the agent loop still truncates — so a huge read may be cut at the loop boundary with the harness's own marker rather than the tool's. Do not re-add a cap inside fs_read_file without removing that footer/coherence problem; if a bound is needed, bound the observation in the loop, not the tool.

