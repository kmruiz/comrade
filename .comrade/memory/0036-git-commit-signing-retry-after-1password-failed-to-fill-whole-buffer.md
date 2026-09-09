# 0036 - git commit signing: retry after "1Password: failed to fill whole buffer"
status: accepted
tags: git, 1password, signing, ops
summary: git commits are ssh-signed via 1Password; a commit can fail with "1Password: failed to fill whole buffer" — retrying (even with sleep 2) after the human unlocks usually lands it

## Context
In this repo commit.gpgsign=true and gpg.format=ssh, signing through the 1Password SSH agent. Observed 3 consecutive 60s commit failures with the error "1Password: failed to fill whole buffer / fatal: failed to write commit object", then a successful commit on the next attempt.

## Decision
When a git_commit/shell git commit fails with "1Password: failed to fill whole buffer", do not change the file or retry-loop blindly: confirm the human has 1Password unlocked, then retry the exact same commit command (a short `sleep 2` first seems to help). If it keeps failing after a few tries, offer `git -c commit.gpgsign=false commit` or ask the human to commit manually — never silently commit unsigned while gpgsign is true.

## Consequences
Transient 1Password SSH-agent hiccup; no code impact. The failing commits were never created (exit 128), so retry is idempotent.

