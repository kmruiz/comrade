#!/usr/bin/env bash
#
# release-notes.sh - print the GitHub release body for a release tag.
#
# The description is authored by the agent that cuts the release and lives in
# the *body* of the commit the tag points at, so it is version-controlled next
# to the code and says exactly what changed. This script only extracts that
# body and appends the usual compare link.
#
#   usage: release-notes.sh <tag>        # e.g. release-notes.sh v0.3.0
#
# Exit codes: 0 = notes printed, 1 = the tagged commit has no description,
#             2 = bad usage. Requires: git, sed. Honours GITHUB_REPOSITORY.
set -euo pipefail

ref="${1:-${GITHUB_REF_NAME:-}}"
if [ -z "$ref" ]; then
  echo "usage: $(basename "$0") <tag>" >&2
  exit 2
fi

# The whole commit body (everything after the subject line) is the description.
body="$(git log -1 --format=%b "$ref")"
if [ -z "${body//[[:space:]]/}" ]; then
  cat >&2 <<EOF
error: the commit tagged $ref has no release description.

Release descriptions are authored, not generated: commit the description of
everything that changed since the previous tag *before* tagging, e.g.

  git commit --allow-empty -m "release: $ref" -m "## What's Changed ..."

See the header of ./release.sh.
EOF
  exit 1
fi
printf '%s\n' "$body"

# owner/repo, from Actions or from the origin remote (ssh or https URL).
slug="${GITHUB_REPOSITORY:-}"
if [ -z "$slug" ]; then
  url="$(git remote get-url origin 2>/dev/null || true)"
  slug="$(printf '%s' "$url" | sed -n 's#.*github\.com[:/]##p' | sed 's#\.git$##')"
fi

# Previous tag reachable from the tagged commit, if any.
prev="$(git describe --tags --abbrev=0 "${ref}^" 2>/dev/null || true)"
if [ -n "$slug" ] && [ -n "$prev" ]; then
  printf '\n**Full Changelog**: https://github.com/%s/compare/%s...%s\n' "$slug" "$prev" "$ref"
fi
