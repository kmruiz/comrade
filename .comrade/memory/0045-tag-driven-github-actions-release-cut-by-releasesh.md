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


## Note
Follow-up: the "does NOT cover a per-commit CI workflow" gap is now closed by `.github/workflows/ci.yml` — a mandatory `ci` job (name "fmt + test") running `cargo fmt --all -- --check` and `cargo test --workspace` on push to main/master and on every pull_request, with `permissions: contents: read`. Mark the `ci` job as a required status check in branch protection to make it mandatory. Uses the same toolchain/cache actions as release.yml (dtolnay/rust-toolchain@stable with the rustfmt component, Swatinem/rust-cache@v2).

## Note
Enforcement applied (kmruiz/comrade): branch protection on `main` now requires the status check context "fmt + test" (strict=false, enforce_admins=false, no PR requirement), set with `gh api -X PUT repos/kmruiz/comrade/branches/main/protection`. The required context must match the check-run name, which is the job's `name:` (not the workflow `name: ci`), hence "fmt + test". The ci.yml commit was pushed to origin/main and its run 34790172929 completed green.

## Note
Release notes now lead with a short functionality summary: the static header lives in `.github/release-notes-header.md` and is passed to `gh release create` via `--notes-file` alongside `--generate-notes` (gh appends the auto-generated changelog after the provided notes). This was also applied retroactively to the v0.1.0 release with `gh release edit --notes-file`. Separately, all actions bumped off Node.js 20 (the runners force Node 24 and warn): actions/checkout@v4 -> v5, actions/upload-artifact@v4 -> v5, actions/download-artifact@v4 -> v5 in ci.yml and release.yml; Swatinem/rust-cache@v2 and dtolnay/rust-toolchain@stable did not warn and were left as-is. Verified on ci run 34791184250 (success, zero deprecation lines).

## Merged from #0031 - Tune the release profile for minimum static binary size
status: accepted
date: 2026-09-13
tags: release, size, profile, static-binary
summary: Release profile tuned for minimum static binary size (opt-level=z, fat LTO, codegen-units=1, panic=abort, strip), shrinking the binary ~13.8% (70->60 MB) with no dynamic libraries.

## Context
The release binary embeds a ~34 MB int8 ONNX embedding model (deflated to ~24 MB by crates/comrade-tool-memory/build.rs) and statically links ONNX Runtime, so it is large (72,518,632 bytes / ~70 MB on Linux x86-64). The previous [profile.release] only set `strip = true`. The user asked to minimise the app size as far as possible without using dynamic libraries.

## Decision
Set `[profile.release]` in the root Cargo.toml to opt-level = "z", lto = "fat", codegen-units = 1, panic = "abort", strip = true. The binary stays fully static (only glibc is linked dynamically, as before; no crate dylibs, no `-C prefer-dynamic`).

## Rationale
These are the strongest size levers a Cargo profile offers while keeping a static binary: `z` shrinks codegen, fat LTO + codegen-units=1 let the linker remove cross-crate dead code, `panic=abort` drops unwinding tables/landing pads, and `strip` removes symbols. None require changing dependencies or linking dynamically.

## Alternatives considered
(a) opt-level = "s" — slightly faster but larger than "z"; rejected since min size is the goal. (b) lto = "thin" — much faster builds but less dead-code elimination; rejected. (c) Keep panic = unwind to allow `cargo test --release`; rejected because the codebase has no catch_unwind/panic hooks and dev-profile tests are unaffected, so abort is a free size win. (d) Dynamic/shared libraries or `-C prefer-dynamic` to shrink the binary — explicitly rejected by the requirement (static binary). (e) Statically linking musl libc — out of scope and larger, not a profile setting.

## Scope
Covers only the release build profile in the root Cargo.toml. Does NOT change dependencies, features, the embedded model, or link dynamically, and does not touch debug/dev profiles.

## Impact
Release binary 72,518,632 B -> 62,493,216 B (-10,025,416 B, ~13.8%; 70 MB -> 60 MB). Trade-offs: release builds are slower (fat LTO + 1 codegen unit; the observed build was ~1m19s) and the binary may run marginally slower (opt-level z). panic=abort means no unwinding in release builds, so `cargo test --release` will not build (dev `cargo test` uses the test profile and is unaffected); do not reintroduce catch_unwind/panic-recovery in release-only paths. The embedded model (~24 MB deflated) and ONNX Runtime dominate the remainder and are not reducible by profile settings.

## Note
Rollup of the release process: this ADR adds the tag-driven GitHub Actions release cut; #0031 tunes the release profile for a minimum-size static binary. Body preserved under "Merged from".
