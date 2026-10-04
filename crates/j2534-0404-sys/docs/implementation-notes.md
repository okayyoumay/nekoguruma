# j2534-0404-sys implementation note

This crate provides a low-level native interface for SAE J2534-1 API version 04.04.

Design goals:
- Match workspace patterns used by iso22900-sys.
- Keep ABI-facing definitions in a single C header: src/bindings/j2534_v0404.h.
- Support pre-generated per-target bindings by default.
- Support regeneration via bindgen feature for maintenance.

Primary API scope (04.04):
- PassThruOpen / PassThruClose
- PassThruConnect / PassThruDisconnect
- PassThruReadMsgs / PassThruWriteMsgs
- PassThruStartPeriodicMsg / PassThruStopPeriodicMsg
- PassThruStartMsgFilter / PassThruStopMsgFilter
- PassThruSetProgrammingVoltage
- PassThruReadVersion
- PassThruGetLastError
- PassThruIoctl

## SAE J2534-2 constants (ADR-152)

`src/bindings/j2534_v0404.h` also carries every SAE J2534-2 (DEC2020,
"Optional Pass-Thru Features") ProtocolID, IOCTL ID, `SCONFIG`/device-config
parameter ID, the six genuinely-new error codes, and the new
`REPEAT_MSG_SETUP`/`SPARAM`/`SPARAM_LIST`/`NDIS_ADAPTER_INFORMATION` structs
— added in one pass per `docs/j2534-2-support-plan.md`'s Phase 0, so later
phases reference existing constants instead of repeatedly re-touching this
FFI layer. Of these, the six error codes are referenced downstream (Phase 0):
`j2534-0404/src/error.rs` maps each to a `StatusCode` `Display` arm and
`j2534-0404/src/lib.rs` re-exports all six from the crate root alongside the
existing J2534-1 error constants, so callers can match on them without a
direct `j2534-0404-sys` dependency. Phase 1 (ADR-153) additionally exercises
the `SPARAM`/`SPARAM_LIST` structs, `IOCTL_GET_DEVICE_INFO`/
`IOCTL_GET_PROTOCOL_INFO`, and a subset of the `DEVICE_INFO_*`/
`PROTOCOL_INFO_*` parameter IDs (imported directly by `j2534-0404-mock`,
which is not gated by `build.rs`'s allowlists since it depends on this crate
without the `bindgen` feature) — see `j2534-0404/docs/implementation-notes.md`
and `j2534-0404-service/docs/implementation-notes.md` for how. Every other new
symbol (the remaining ProtocolIDs, IOCTL IDs, `SCONFIG`/`DEVICE_INFO_*`/
`PROTOCOL_INFO_*` parameter IDs, and the `REPEAT_MSG_SETUP`/
`NDIS_ADAPTER_INFORMATION` structs) is still dormant: no safe-wrapper or
service code references them yet, and none of the 14 functions above
changes signature. When adding a phase's first real use of one of these
constants, double check it (and any sibling constant the same phase needs)
is covered by `build.rs`'s `allowlist_type`/`allowlist_var` regexes for the
*generated bindings* consumers actually use (`j2534-0404-sys`'s own
`bindings` module, used by `j2534-0404` and `j2534-0404-mock` alike) —
bindgen silently drops anything that doesn't match rather than erroring.

**Known pre-existing gap (not a J2534-2 regression):** `RX_TX_MSG_TYPE`
(base J2534-1, `0x00000001u`) has never matched `allowlist_var`'s
`RX_FLAG_.*` alternative — it's named `RX_TX_.*`, not `RX_FLAG_.*` — so it
has never appeared in any committed binding. Found during Phase 0's
edge-case review; not fixed here since it predates and is unrelated to
J2534-2 work. Fix by widening the regex (or renaming, if nothing already
depends on the literal `RX_TX_MSG_TYPE` name) when a phase actually needs
this constant.
