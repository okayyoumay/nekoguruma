# iso22900 Implementation Note

## Scope

Safe Rust wrapper layer for D-PDU operations over low-level bindings from iso22900-sys.

## Assumptions

- Calling convention is target-sensitive (stdcall on 32-bit Windows, C ABI otherwise).
- `iso22900-sys` enum-newtype inner fields (e.g. `T_PDU_STATUS(.0)`, `T_PDU_ERROR(.0)`) have a target-dependent signedness (`c_int` on `x86_64-pc-windows-msvc`, `c_uint` elsewhere, see ADR-108); always cast explicitly (`as UNUM32`) when reading them into `u32`-typed positions, never rely on the field type matching.
- Unsafe pointer interactions are normalized into typed Rust structures before wider use.
- Event callback dispatch remains keyed by module and logical-link handles.

## Implementation Policy

- Keep unsafe code tightly scoped at FFI boundaries and preserve safe wrapper contracts.
- Preserve strong handle newtypes to avoid cross-handle misuse.
- Require tests for new encode/decode paths and error mapping branches.
- Keep callback trampoline signatures synchronized with mock and sys crates.
- The event callback trampoline (`src/events.rs`) is passed to `PDURegisterEventCallback` as the bindings' `CALLBACKFNC` type, so a calling-convention or signature mismatch with the bindings is a compile error on that target. CI's `worker-check` job type-checks `iso22900-service`, and with it this crate, on all six worker targets, so no separate runtime ABI test is needed for the trampoline. The mock's exports are checked against the bindings separately by `crates/iso22900-mock/tests/abi_parity.rs`.

## Change Checklist

1. Verify ABI compatibility on supported targets after signature-level edits.
2. Add or update item conversion tests for all new/changed item variants.
3. Confirm callback registration/unregistration paths remain race-safe.
4. Review native error-to-Rust error mapping consistency.
