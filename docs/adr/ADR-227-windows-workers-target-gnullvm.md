# ADR-227: Windows Workers Target `*-pc-windows-gnullvm`, Cross-Built on Linux

**Date:** 2026-10-04
**Status:** Accepted
**Affects:** `.github/workflows/ci.yml`, `.cargo/config.toml`, `docs/worker-crates.md`, `docs/system-architecture.md` (12.1), `docs/j2534-0404-architecture.md`

## Context

Workers are built for two Windows ABIs, x64 and x86 (design 12.1). CI built them as
`x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` on `windows-latest` runners, because
cargo-zigbuild, which cross-builds the Linux workers, cannot target MSVC. In a private
repository, Windows runner minutes are billed at twice the Linux rate, and the two Windows
release builds were among the most expensive jobs in a CI run.

The `-sys` crates already carry committed bindings for `x86_64-pc-windows-gnullvm` and
`i686-pc-windows-gnullvm` (and the worker code is checked for both), so the LLVM/MinGW ABI was
already a supported target.

The worker talks to vendor libraries only through their C API, loaded at run time. The vendor
DLLs are typically built with MSVC, but nothing in that API passes C runtime objects across the
boundary: callers supply buffers, and memory a library allocates (D-PDU API items) is released by
that library's own functions. So the worker's own C runtime does not have to match the vendor
library's.

## Decision

1. The Windows worker targets are `x86_64-pc-windows-gnullvm` (win-x64) and
   `i686-pc-windows-gnullvm` (win-x86). The `*-pc-windows-msvc` targets are not built, released
   or checked by CI. Their committed bindings stay in place (generated code; removing them is a
   separate decision).
2. CI cross-builds both targets on `ubuntu-latest` with the llvm-mingw toolchain (clang, lld and
   the mingw-w64 import libraries, UCRT variant).
3. `.cargo/config.toml` builds both targets with `-C target-feature=+crt-static`, which links
   the LLVM unwinder statically. Without it a gnullvm binary needs `libunwind.dll` next to it. A
   CI step lists each built executable's DLL imports and fails if any DLL is not one Windows
   itself provides.

## Consequences

- The Windows worker builds run at the Linux rate and no longer need a Windows runner.
- On Windows the D-PDU API enum newtypes are now `c_uint`, not `c_int` (ADR-108). The enums are
  4 bytes either way, so the ABI is unchanged, and ADR-108 already requires code not to assume a
  signedness.
- The `core-windows` test job still builds and runs the tests with the runner's default MSVC
  host toolchain, so the Windows tests do not yet exercise the gnullvm ABI the workers ship with.
- llvm-mingw is downloaded from its GitHub releases on every run, pinned to one release in
  `ci.yml`; updating it is a deliberate change there.
