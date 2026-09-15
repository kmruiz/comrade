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
# the description committed just before the tag (see below).
#
# The release description is authored, not generated: before tagging, commit the
# text that describes everything that changed since the previous tag, with the
# subject `release: vX.Y.Z` and that text as the commit body:
#
#   git commit --allow-empty -m "release: v0.3.0" -m "## What's Changed
#   ..."
#
# release.sh refuses to tag any other commit, and prints the description it is
# about to publish. Pass --no-notes to skip the check and preview.
#
# Requirements: git, and a configured `origin` remote you may push to.
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") {patch|minor|major} [--no-notes]" >&2
  exit 2
}

skip_notes=0
bump=""

for arg in "$@"; do
  case "$arg" in
    --no-notes) skip_notes=1 ;;
    patch | minor | major) bump="$arg" ;;
    *) usage ;;
  esac
done

if [ -z "$bump" ]; then
  usage
fi

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

if [ "$skip_notes" -eq 0 ]; then
  notes_subject="$(git log -1 --format=%s)"
  if [ "$notes_subject" != "release: ${next}" ]; then
    cat >&2 <<EOF
error: HEAD is not the release-notes commit for ${next}.
       expected subject: release: ${next}
       found:            ${notes_subject}

       Describe what changed since ${latest} in a commit body first:

         git commit --allow-empty -m "release: ${next}" -m "## What's Changed
         ..."

       Then re-run ./release.sh ${bump}.  (--no-notes skips this check.)
EOF
    exit 1
  fi
  if [ -z "$(git log -1 --format=%b | tr -d '[:space:]')" ]; then
    echo "error: the release-notes commit has an empty body; write what changed since ${latest}" >&2
    exit 1
  fi
fi

git tag -a "${next}" -m "Release ${next}"

if [ "$skip_notes" -eq 0 ]; then
  echo "---- release description ----"
  if ! bash .github/scripts/release-notes.sh "${next}"; then
    echo "error: could not generate the release description; tag ${next} not pushed and removed locally" >&2
    git tag -d "${next}" >/dev/null
    exit 1
  fi
  echo "-----------------------------"
fi

git push origin "${next}"
git push origin main
echo "pushed ${next}; the release workflow is now building the binaries."
