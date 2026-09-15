# 0058 - fs_edit falls back to a unique whitespace-tolerant whole-line match
status: accepted
date: 2026-09-15
tags: fs_edit, small-model, reliability
summary: When fs_edit's exact `old` block is not found, it retries as a whole-line, indentation-insensitive match that must be UNIQUE, and re-indents the replacement to the matched block.

## Context
A small model (ministral-3-3b) re-types the block it just read and routinely gets indentation or trailing whitespace wrong, so the byte-for-byte `old` match failed - the dominant fs_edit failure in the smoke runs, and it pushed the model into rewriting the whole file with fs_write_file (or into shell flailing).

## Decision
fs_edit literal mode keeps the exact-match fast path, and on zero matches retries through `fuzzy_line_match` (crates/comrade-tool-fs/src/lib.rs): lines compared trimmed-end (and the inner comparison trims both ends), the match must be UNIQUE in the file (more than one candidate -> no fallback, the model gets the normal "not found" error), and the `new` lines' common indentation is swapped for the matched block's own indent so the insertion lands at the right depth. The result string tells the caller the block matched ignoring indentation/trailing spaces, and the undo capture + confirm path is the same as an exact edit.

## Rationale
Indentation is the noise a small model adds most often, and it carries no semantic weight for "which block did you mean" - so ignoring it, while demanding uniqueness, recovers most failures without ever editing the wrong place.

## Alternatives considered
(1) Loosen to a substring/first-match fallback - rejected: silently edits the wrong occurrence. (2) Line-number-based edits (replace lines N..M) - rejected: shifts under any earlier edit and the model miscounts. (3) Return a better error and let the model re-read - rejected: it re-read and failed again in the same way.

## Scope
fs_edit literal mode only (crates/comrade-tool-fs/src/lib.rs, `fuzzy_line_match` + the literal_mode fallback). Not the patch (diff) mode, which is already context-addressed.

## Impact
A re-typed block now applies instead of erroring; a genuinely ambiguous or absent block still errors, so the failure mode stays safe. Pinned by the fuzzy_line_match tests.

