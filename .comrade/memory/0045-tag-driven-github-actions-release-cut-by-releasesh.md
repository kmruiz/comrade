# 0045 - Tag-driven GitHub Actions release, cut by ./release.sh
status: accepted
date: 2026-09-13
tags: ci, release, github-actions
summary: Releases are cut by pushing a `vX.Y.Z` ref; a GitHub Actions workflow builds the `comrade` binary in --release on Linux/macOS/Windows and creates the GitHub release with auto-generated notes, and ./release.sh {patch|minor|major} bumps the latest v-tag.

## Context
The repo shipped no CI, no tags and no release tooling. A repeatable, one-command release process was needed that produces prebuilt binaries for the three desktop OSes and a GitHub release whose notes list the commits since the previous release.

## Decision
Add .github/workflows/release.yml triggered on push of a ref named `v[0-9]*` (tag OR branch). Job `build` runs a matrix of ubuntu-latest / macos-latest / windows-latest, each doing `cargo build --release --bin comrade` (single bin `comrade`, defined in crates/comrade-tui), packaging the binary as `comrade-<ref>-<platform>.tar.gz` (unix) or `.zip` (Windows), and uploading it as an artifact. Job `release` (needs: build, contents: write) checks out full history, downloads all artifacts, and runs `gh release create <ref_name> --target <sha> --generate-notes`, so the notes are the commits since the last release and the artifacts are attached. Add ./release.sh {patch|minor|major}: it takes the highest existing `v[0-9]*.[0-9]*.[0-9]*` tag (git `--sort=-v:refname`, fallback v0.0.0), applies the semver bump, creates an annotated tag and `git push origin <tag>`, which fires the workflow via the tag path.

## Rationale
Pushing a tag/branch named after the version keeps the human's trigger ("push a new branch named vTAG") while being the conventional tag-driven release. Supporting both `tags:` and `branches:` filters means either push style works; when a branch is pushed, `gh release create --target` creates the matching tag. `--generate-notes` gives "commits since the last release" for free. Building each OS natively (no --target triple) is simplest and needs no cross toolchain; the heavy release profile (ADR #31: opt-level=z, fat LTO, strip) is exercised exactly as intended.

## Alternatives considered
Hand-written changelog per release (rejected: --generate-notes is automatic). Cross-compiling from one runner with `cargo build --target` (rejected: needs cross toolchains and, for macOS, Apple SDKs). Triggering only on tags (rejected: the request explicitly named a branch). A separate release.sh that also pushes a branch (rejected: leaves a stray branch; a tag is the right ref).

## Scope
Covers the release trigger, the build matrix, artifact packaging/upload, the GitHub release creation and the version bump script. Does NOT cover code signing, notarization, publish to crates.io, or a per-commit CI (test/clippy) workflow.

## Impact
Releasing is `./release.sh major|minor|patch`; the workflow then publishes three archives on the GitHub release. Requires the repo to have a git `origin` remote (none configured yet) and `permissions: contents: write` (granted in the workflow). macOS runners are arm64-only at present (macos-latest), so Intel macOS binaries are not produced.

