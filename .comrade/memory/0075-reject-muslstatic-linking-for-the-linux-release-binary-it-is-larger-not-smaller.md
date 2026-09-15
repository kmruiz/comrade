# 0075 - Reject musl/static linking for the Linux release binary: it is larger, not smaller
status: accepted
date: 2026-09-15
tags: release, size, static-binary, musl, glibc, onnxruntime, linking, ci
summary: Do not adopt musl: it cannot even build (ONNX Runtime ships no musl prebuilt and the C/C++ deps need a musl toolchain) and static linking was measured to make the binary 2,198,024 B (+3.5%) LARGER — the size is dominated by the embedded 24.5 MB ONNX model, not libc.

## Context
The question was whether linking the Linux release binary against musl (x86_64-unknown-linux-musl, fully static) would make it smaller. Superseded ADR 31 had dismissed musl as "larger" in one line, without measurement, so the claim was re-tested empirically on this tree. Relevant facts: the binary embeds a ~34 MB int8 ONNX model deflated to 24,503,282 B (comrade-tool-memory/build.rs + include_bytes!), fastembed 5 links ONNX Runtime statically through ort's `ort-download-binaries-native-tls` feature, and the release profile is opt-level=z + fat LTO + codegen-units=1 + panic=abort + strip (ADR 31), built natively per OS by the tag-driven workflow (ADR 45).

## Decision
Keep the Linux release artifact on x86_64-unknown-linux-gnu with glibc dynamically linked (current CI). Do NOT switch the build to musl and do NOT make the binary fully static: measured, static linking makes this binary LARGER, not smaller, and musl cannot even be built for this dependency set.

## Rationale
Static linking does not shrink this binary, it relocates code into it. Today libc, libm, libgcc_s and libstdc++ are shared objects that contribute ~0 bytes to the file (the binary only carries ~1.4 MB of .rela.dyn plus PLT stubs). Making them static adds the surviving parts of those libraries (~5.5 MB of shared objects, pruned by LTO/--gc-sections) and only removes the relocation overhead, giving the measured net +2,198,024 B. musl's libc is not enough smaller to flip that sign, and ORT is C++, so musl would additionally require a musl-built static libstdc++. The size of this binary is simply not decided by libc: the embedded ONNX model alone is 24.5 MB (39%) and the prebuilt libonnxruntime.a that the linker prunes from is 105 MB.

## Alternatives considered
(a) Switch the Linux release build to x86_64-unknown-linux-musl (static musl) — REJECTED: it does not even build (ort-sys has no musl prebuilt, and the host lacks a musl C toolchain for the aws-lc-sys/ring/tree-sitter/onig/ORT C and C++ code), and static linking measures ~2.2 MB larger. (b) Fully static glibc via -C target-feature=+crt-static — REJECTED on measurement (+2,198,024 B, +3.5%) and because static glibc binaries break dlopen/NSS-dependent behaviour. (c) -C prefer-dynamic / shipping crate dylibs — REJECTED by the standing requirement of a single self-contained binary (ADR 31). (d) Keep the current glibc-dynamic build and instead attack the real size driver — ACCEPTED as the only route with real upside (see above).

## Scope
Covers the choice of libc / link mode for the Linux release artifact of the single `comrade` binary. Does NOT change the release profile (ADR 31 stays), the embedded model, the crypto provider, or the release workflow (ADR 45) — it only records that musl/static is not the size lever and must not be re-tried without new evidence.

## Impact
No code or CI change; the Linux release stays dynamically linked (`for GNU/Linux 3.2.0`), so users need a reasonably recent glibc/libstdc++ (the portability cost is accepted knowingly). Future size work must target the model and the crypto/TLS stack, not the libc. A future attempt to ship a distro-independent static Linux binary must budget for building ONNX Runtime from source in CI and must expect ~+2 MB, not a shrink. Build-hygiene note: `-C target-feature=+crt-static` must be set via CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS together with an explicit `--target x86_64-unknown-linux-gnu`, otherwise cargo fails with "cannot produce proc-macro ... does not support these crate types"; the throwaway build must use its own CARGO_TARGET_DIR (target-static is NOT covered by .gitignore's `/target`).

