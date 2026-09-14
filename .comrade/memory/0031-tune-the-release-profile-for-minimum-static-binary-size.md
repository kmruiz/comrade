# 0031 - Tune the release profile for minimum static binary size
status: superseded
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
merged into #0045
