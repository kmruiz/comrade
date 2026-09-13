#!/usr/bin/env bash
#
# release.sh - cut a new release by bumping the latest vX.Y.Z tag with semver.
#
#   ./release.sh patch   # v0.1.0 -> v0.1.1
#   ./release.sh minor   # v0.1.0 -> v0.2.0
#   ./release.sh major   # v0.1.0 -> v1.0.0
#
# It finds the highest existing tag matching vX.Y.Z (v0.0.0 when there is
# none), applies the requested bump, creates an annotated tag and pushes it.
# Pushing the tag triggers .github/workflows/release.yml, which builds the
# --release binary on Linux/macOS/Windows and creates the GitHub release with
# notes generated from the commits since the previous tag.
#
# Requirements: git, and a configured `origin` remote you may push to.
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") {patch|minor|major}" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage

case "$1" in
  patch | minor | major) bump="$1" ;;
  *) usage ;;
esac

# Highest existing vX.Y.Z tag (git sorts -v:refname by version, not lexically),
# falling back to v0.0.0 so a fresh repository releases v0.0.1 first.
latest="$(git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -n1)"
[ -n "$latest" ] || latest="v0.0.0"

version="${latest#v}"
if ! [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  echo "error: latest tag '$latest' is not a vMAJOR.MINOR.PATCH version" >&2
  exit 1
fi

major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"

case "$bump" in
  major)
    major=$((major + 1))
    minor=0
    patch=0
    ;;
  minor)
    minor=$((minor + 1))
    patch=0
    ;;
  patch) patch=$((patch + 1)) ;;
esac

next="v${major}.${minor}.${patch}"

if git rev-parse -q --verify "refs/tags/${next}" >/dev/null; then
  echo "error: tag ${next} already exists" >&2
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "warning: working tree has uncommitted changes; those are NOT released" >&2
fi

echo "latest tag: ${latest}  ->  ${bump} bump  ->  ${next}"

git tag -a "${next}" -m "Release ${next}"
git push origin "${next}"

echo "pushed ${next}; the release workflow is now building the binaries."
