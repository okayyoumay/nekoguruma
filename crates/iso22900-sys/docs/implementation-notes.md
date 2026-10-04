# iso22900-sys Implementation Note

## Scope

Low-level FFI and dynamic loading crate. It provides raw bindings only; higher-level safety remains outside this package.

## Assumptions

- Pre-generated target binding files are the default compilation path.
- Missing binding files are treated as hard build failures unless bindgen generation is explicitly enabled.
- bindgen execution depends on host tooling (Clang/libclang and compatible headers).
- Runtime loading behavior follows generated ABI declarations and libloading semantics.
- Plain C enum newtypes (`.newtype_enum("E_PDU_.*")`) have a target-ABI-dependent underlying integer type: `c_uint` on Itanium-derived ABIs (Linux, `*-pc-windows-gnullvm`, `armv5te-unknown-linux-gnueabi`), `c_int` on `x86_64-pc-windows-msvc` (Microsoft ABI). Consumers must not assume a fixed width/signedness across targets (see ADR-108).

## Implementation Policy

- Keep generated bindings versioned for all supported targets.
- Keep header inputs and allowlists stable and intentional.
- Avoid convenience wrapper logic in this crate.
- Validate ABI-sensitive changes on actively supported architectures.

## Change Checklist

1. Confirm target binding files exist or regeneration path is documented in the same change.
2. Verify ABI-sensitive declarations against consumer expectations, including enum-newtype underlying-type differences across targets (ADR-108).
3. Rebuild dependent crates that consume these bindings.
4. Update target support notes when adding/removing architecture coverage.
