# 0012 - Render git_diff results with the shared side-by-side diff renderer
status: accepted
date: 2026-09-13
tags: tui, diff, git_diff
summary: Extend extract_diff_sides/edit_diff_label with a `result` param and a git_diff arm so git_diff tool cards render side-by-side exactly like fs_edit.

## Context
The TUI already renders edit-tool cards as aligned side-by-side diffs (build_diff_row / cell_spans / lcs_pairs in crates/comrade-tui/src/tui.rs). That path only recognised fs_edit: extract_diff_sides parsed its args (literal old/new, or an embedded unified diff). The git_diff tool, however, delivers its unified diff in the tool RESULT string, not in args, so git_diff cards fell through to the plain "result:" text block and were not shown side-by-side.

## Decision
Thread the tool result into the diff extractor: extract_diff_sides(name, args_json, result) and edit_diff_label already took a `result` param. Add a `git_diff` match arm to both that parses the unified diff text from the result (skip diff --git/index/mode/rename/---/+++/@@/\\ No newline headers; '+' -> added, '-' -> removed). If no +/- lines are found (e.g. the "(no diff)" body) return None so the card falls back to the normal result block. Reuse the existing renderer unchanged.

## Rationale
git_diff output is a plain unified diff, the same shape fs_edit already parses, so one shared extractor keeps a single side-by-side renderer instead of a second code path. Reusing the existing widgets minimises surface and keeps alignment/colouring consistent.

## Alternatives considered
A separate git-diff renderer was rejected (duplicated LCS/alignment logic). Running git in the TUI to get numstat was rejected (the diff is already in the card result; no extra process needed).

## Scope
Covers git_diff cards in the TUI chat. Not git_show (commit diffs) and not the approval-dialog diff previews, which keep their existing rendering.

## Impact
git_diff cards now show aligned red/green rows with a `diff <file> <hunk>` label, capped at 200 rows. Behaviour for fs_edit is unchanged (new `result` argument); all call sites and tests updated. Adds 5 unit tests in mod diff_tests.

