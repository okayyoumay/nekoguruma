# ADR-108: Bindgen Enum-Newtype Underlying Type Is Target-ABI-Dependent — Always Cast, Never Assume `u32`

**Date:** 2026-07-22
**Status:** Accepted
**Affects:** `iso22900-sys` (all pre-committed target binding files), `iso22900` safety.rs, lib.rs, item/data/event.rs, item/data/io.rs

## Context

`iso22900-sys` uses `bindgen` with `.newtype_enum("E_PDU_.*")` to wrap the D-PDU API's plain C `enum` types (e.g. `E_PDU_STATUS`, `E_PDU_ERROR`) as single-field tuple structs. The D-PDU header declares these as ordinary `enum { ... }` with no explicit underlying-type annotation, so the C/C++ standard leaves the underlying integer type implementation-defined, and `bindgen` resolves it per target ABI rather than per header content:

- On the Itanium-derived ABIs used for `x86_64-unknown-linux-gnu`, the `*-pc-windows-gnullvm` targets, and `armv5te-unknown-linux-gnueabi`, `bindgen` picked `c_uint` for these enums (all D-PDU status/error values are non-negative and fit `unsigned int`).
- On `x86_64-pc-windows-msvc`, the Microsoft ABI resolves plain C enums to `c_int` by default, so the pre-committed `x86_64-pc-windows-msvc.rs` bindings represent the same enums as `c_int`.

`iso22900`'s safe wrapper layer (`safety.rs`, `lib.rs`, `item/data/event.rs`, `item/data/io.rs`) read the inner `.0` field of these newtypes and fed it directly into `UNUM32` (`u32`)-typed positions — `DPduApiError::PduError`, `PduStatus`, `ErrorEventCode`, and the `status()`/`filter_type()` accessors. This compiled fine on every previously-built target because their bindings happened to use `c_uint`, but `cargo build --target x86_64-pc-windows-msvc` fails with `E0308: expected u32, found i32` at all five call sites, since that target's bindings use `c_int`.

## Decision

Cast explicitly (`as UNUM32`) at every point where an enum newtype's inner field is read as a plain `u32`, instead of relying on the field's declared type matching `u32`. The cast is a same-width bit-reinterpretation (all D-PDU status/error codes are non-negative 32-bit values, so `i32 as u32` and `c_uint` already in use are numerically identical) and is a no-op on targets where the field is already `c_uint`.

We do not force `bindgen` to emit a fixed underlying type across all targets. The generated type reflects each target's real C-enum ABI, which is what a genuine cross-target FFI binding should do; papering over it with a uniform typedef would make the generated bindings lie about the native library's actual on-the-wire/ABI representation for that platform.

## Consequences

- `x86_64-pc-windows-msvc` now builds through `iso22900`, `j2534-0404`, `j2534-0500`, and the three gRPC services (verified via `cargo build --workspace --target x86_64-pc-windows-msvc`, which now fails only on this sandbox's missing MSVC linker/Windows SDK — an environment limitation, not a code defect).
- Any future accessor that reads an `E_PDU_*`-derived newtype's `.0` field into a `u32`-typed slot must cast explicitly rather than rely on field-type equality; this is now called out in `iso22900-sys/docs/implementation-notes.md` and `iso22900/docs/implementation-notes.md`.
- This sandbox cannot fully validate the `x86_64-pc-windows-msvc` link step (no `link.exe`, no Windows SDK headers for `rquickjs-sys`'s bindgen build script in `framework`). Full link-level validation requires either a real Windows/MSVC build machine or an `xwin`-based cross toolchain, neither of which is currently set up in this repository.
