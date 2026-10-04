# ADR-176: SAE J2534-2 Device Configuration (Phase 14)

**Date:** 2026-08-13
**Status:** Accepted (the `IODeviceConfigList`/`IODeviceConfigEntry` `DataItem` shape superseded by ADR-178 — re-expressed as a hand-packed byte payload inside the pre-existing `bytearray_data` carrier; every other Decision item, including the IOCTL commands, native/mock layer, and persistence semantics, is unaffected and remains in force)
**Affects:** `vci-service-interface` (proto), `j2534-0404` (safe wrapper), `j2534-0404-service` (RPC dispatch, names, ComParam-adjacent IOCTL surface), `j2534-0404-mock`

## Context

SAE J2534-2 clause 18 adds two `PassThruIoctl` commands, `GET_DEVICE_CONFIG`
(`0x00008007`) and `SET_DEVICE_CONFIG` (`0x00008008`), that read and write
ten 4-byte non-volatile storage slots (`NON_VOLATILE_STORE_1`..`_10`, native
parameter ids `0x0000C001`-`0x0000C00A`) on the pass-thru interface itself.
Both calls take `DeviceID` as their native handle, not `ChannelID` — the
first J2534-2 feature this service brings into scope with no protocol,
channel, or CLL involved at all. Table 73's own notes require the stored
values to survive power loss, firmware programming, and persist a minimum
of 6 months per write; `SET_DEVICE_CONFIG` must itself return within 10
seconds.

Both IOCTLs marshal their parameters through `SCONFIG_LIST`/`SCONFIG` — the
exact same array-of-`{Parameter, Value}` shape `SET_CONFIG`/`GET_CONFIG`
already use for channel ComParams — so no new FFI struct or header work is
needed; Phase 0 already added the IoctlID and parameter-id constants to
`j2534_v0404.h` and regenerated bindings for all 5 targets (confirmed by
direct inspection of the committed bindings; this ADR adds no bindgen
work). What Phase 0 did not settle is the D-PDU-facing shape: this is the
first IOCTL in this codebase whose native payload is an *array* of
`{parameter, value}` pairs carried in a single call (every prior IOCTL's
`DataItem` is either a scalar, a byte array, or one dedicated struct), and
the plan's own §6 table rates it `design-advisor: No` as a coarse
up-front assessment — this ADR resolves the actual shape directly, per
the precedent Phase 4/6/9/10/11 already set for a "No" that still needed a
real decision made somewhere.

## Decision

**New `ChannelProtocol`/resource-table work: none.** Clause 18 has no
`ProtocolID`, pin space, or ComParam of its own — it is not protocol-scoped
at all, so none of this service's per-protocol machinery
(`names.rs`/`resources.rs`/`comparam_support.rs`) is touched.

**New DeviceID-scoped D-PDU IOCTL commands, mirroring Phase 13's
`READ_J1962PIN_VOLTAGE` (module-scoped `M`) precedent exactly.**
`PDU_IOCTL_GET_DEVICE_CONFIG`/`PDU_IOCTL_SET_DEVICE_CONFIG`
(`PDU_IOCTL_BASE + 0x18`/`+ 0x19`, the 24th/25th D-PDU commands this
adapter implements) resolve `module_handle -> device_id` via the existing
`require_connected_device_for` helper — the same one `READ_J1962PIN_VOLTAGE`
already uses to get from a module-scoped D-PDU handle to the native
`DeviceId` these two native IOCTLs require. Gated on the connecting
module's SAE J2534-2 opt-in (clause 5), the same `discovery::is_j2534_2_opted_in`
check `READ_J1962PIN_VOLTAGE` already applies.

**New `DataItem` shape: `IODeviceConfigList`, a repeated list of
`{parameter_id, value}` pairs — the D-PDU mirror of native `SCONFIG_LIST`.**

```proto
message IODeviceConfigEntry {
    uint32 parameter_id = 1;
    uint32 value = 2;
}
message IODeviceConfigList {
    repeated IODeviceConfigEntry entries = 1;
}
```

added as `DataItem.data`'s 12th oneof variant, `device_config_list`. One
message serves both directions, matching how native `SCONFIG` itself is
one struct reused for both `GET_DEVICE_CONFIG`'s output and
`SET_DEVICE_CONFIG`'s input (Table 71/72's own text): a `GET_DEVICE_CONFIG`
request's `input_data` carries entries with `parameter_id` set and `value`
ignored (which slots the caller wants read); its response's `output_data`
carries the same list with `value` populated. A `SET_DEVICE_CONFIG`
request's `input_data` carries both fields as input; it has no
`output_data`. This lets a client batch multiple `NON_VOLATILE_STORE_x`
reads/writes in one round trip, matching clause 18.4's own usage guidance
that these parameters should be set infrequently and multiple ones
combined into as few native calls as possible, to conserve non-volatile
write cycles — a single-scalar `DataItem`
variant per parameter would have made batching impossible at the D-PDU
layer even though the native call supports it.

**Parameter-id range validation happens at the native/mock layer, not the
service layer — matching `READ_J1962PIN_VOLTAGE`'s own precedent
(Phase 13), not `is_param_allowed`'s ComParam-allowlist precedent.**
Table 71/72's own text says an unrecognized parameter id is rejected
natively with `ERR_INVALID_IOCTL_PARAM_ID` — a *native* rejection, the
same shape `READ_J1962PIN_VOLTAGE`'s pin-range check already has: that
handler forwards `pin_number` to the native call with no service-side
range check at all, trusting the adapter (or, in this codebase,
`j2534-0404-mock`) to validate and return the documented native error,
which the existing `self.check(...)`/`Error`-to-`Status` path maps —
`ERR_INVALID_IOCTL_PARAM_ID` gets its own arm in
`j2534-0404-service/src/error.rs::pdu_error_for` (added by this phase,
alongside `ERR_INVALID_IOCTL_ID`/`ERR_INVALID_PROTOCOL_ID`'s existing
`PduErrIdNotSupported` mapping — the same "unrecognized id for an
otherwise-understood request" category), not left to the generic
`PduErrFctFailed` fallback every other unmapped native code gets.
`NON_VOLATILE_STORE_1..10`
has no D-PDU ComParam behind it at all — unlike `SET_CONFIG`/`GET_CONFIG`,
whose service-side `is_param_allowed` allowlist exists to enforce
*ComParam-to-protocol* validity, a concern this raw native-parameter-id
IOCTL doesn't have — so there is no analogous service-layer allowlist to
reuse or extend here; duplicating the native range check in the service
layer would just be a second copy of the same closed set the mock must
already carry to behave like a real adapter. An empty `entries` list is
accepted as a no-op (0 slots requested/written) rather than rejected —
matching `SCONFIG_LIST`'s own well-defined `NumOfParams: 0` case,
consistent with `get_config`/`set_config`'s existing zero-length handling.

**`j2534-0404` wrapper: `get_device_config`/`set_device_config`, mirroring
`get_config`/`set_config` verbatim except for taking `DeviceId` instead of
`ChannelId`.** Same `SCONFIG_LIST` marshaling, same `IOCTL_GET_DEVICE_CONFIG`/
`IOCTL_SET_DEVICE_CONFIG` ids. No new abstraction needed — `get_device_info`/
`get_protocol_info` already established that a `DeviceId.0` can be passed
as `PassThruIoctl`'s handle argument exactly like a `ChannelId.0` can.

**Mock persistence semantics: the non-volatile store survives
`PassThruClose`/`PassThruOpen` (mirroring "survives power loss"), but is
wiped by the `__mock_reset` test backdoor (mirroring every other piece of
`MockState`, not a simulated device lifecycle event).** The store lives on
`MockState` (the mock's single process-global state, `j2534-0404-mock/src/lib.rs`),
not `ChannelState` (torn down on connect/disconnect) — a `[u32; 10]`
defaulting to all-zero per Table 73's own default column. `__mock_reset` is
a test-harness-only control this codebase already uses to give every
`#[tokio::test]` case a clean slate against one shared-process mock
instance (every other `MockState` field it wipe follows the same rule);
treating it as a simulated "power loss" event that must NOT clear the
store would leak state across test cases sharing the same test binary,
the opposite of what real persistence exists to guarantee for an actual
device. The mock cannot and does not attempt to simulate the spec's
6-month/firmware-update persistence guarantees literally — those are real
non-volatile-hardware properties no in-memory mock can test.

**Discovery-cache connect-time wiring deferred, matching every prior
phase's precedent.** Clause 18.5 requires the device to advertise support
through the Discovery mechanism (clause 25); this phase adds no
`DEVICE_INFO_*`-gated enforcement at IOCTL-dispatch time, the same
deferral every protocol phase before it has recorded.

## Consequences

- Establishes the first DeviceID-scoped (not ChannelID/CLL-scoped) J2534-2
  IOCTL pair this service implements, and the first D-PDU `DataItem`
  carrying a batched array of native parameter/value pairs — both
  precedents available to any future phase needing the same shape (Device
  Configuration is clause 18's only feature area; no other planned phase
  is DeviceID-scoped as of this writing).
- `IODeviceConfigList`'s single-message-both-directions shape is safe only
  because `SCONFIG` itself has this same input/output-context-dependent
  meaning for `Value` in the native spec (Table 71 vs. 72) — a future
  reader adding a new batched-array IOCTL should re-verify the native
  shape has the same property rather than assuming this pattern always
  applies.
- Accepted residual: Discovery-cache wiring for clause 18.5's advertised
  support bit remains open, tracked in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
  alongside every other phase's identical deferral.
