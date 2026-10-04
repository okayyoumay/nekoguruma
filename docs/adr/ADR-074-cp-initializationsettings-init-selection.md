# ADR-074: CP_InitializationSettings Drives K-line Init Sequence Selection

**Date:** 2026-07-10
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`handle_start_comm`,
`run_protocol_init`, new `select_init_sequence`/`InitSequence`),
`j2534-0404-service/src/service/rpc_link.rs` (`rpc_set_com_param`),
`j2534-0404-service/src/service/comparam_id.rs` (new `is_valid_init_settings`),
`j2534-0404-mock/src/lib.rs` (`IOCTL_FAST_INIT` handler's protocol guard,
verification-pass fix -- see Consequences)

## Context

`CoptStartcomm` on a K-line protocol (ISO9141/ISO14230) must issue either a
J2534 5-baud init (`PassThruIoctl IOCTL_FIVE_BAUD_INIT`) or a fast-init
(`PassThruIoctl IOCTL_FAST_INIT`) before normal communication can begin.
D-PDU's `CP_InitializationSettings` (ComParamId `0x8090`, `PDU_PC_COM` class)
exists precisely to let a caller choose which one — but before this change
`j2534-0404-service` stored the ComParam (seeded by several K-line
`comparam_defaults.rs` presets, settable via `SetComParam`) without ever
reading it back. `handle_start_comm`'s `run_protocol_init` instead used a
hardcoded heuristic: ISO9141, or a single-byte `init_data`, always selected
5-baud init; everything else selected fast-init. This heuristic could not be
overridden by a client and had no way to request "no init sequence at all,"
a mode some already-initialized or pass-through K-line setups need.

This gap was also the last piece of a previously-documented limitation:
ADR-069 fixed the combined K-line resource's *connect* protocol to `ISO9141`
unconditionally and noted that "K-line 5-baud-vs-fast-init selection remains
a documented limitation, per ADR-017" — i.e. this service had no mechanism,
analogous to ADR-070's J1850 VPW/PWM active-probe, to let the *init sequence*
itself be resolved per-link rather than hardcoded. Unlike the J1850 case,
J2534-1 exposes no protocol-level way to auto-probe 5-baud-vs-fast-init; the
correct fix is to honor the D-PDU ComParam the standard already provides for
exactly this choice, not to add another autodetect mechanism.

## Decision

`handle_start_comm`'s init step now reads `CP_InitializationSettings` from
the COP's bound ComParam snapshot (`binding.resolved()` — the Active
snapshot normally, or the Working snapshot borrowed for the init transaction
when `temp_param_update` is set, per ADR-067) and selects one of three
`InitSequence` values, per the project's authoritative value definition:

- `1` → `InitSequence::FiveBaud`: calls `five_baud_init` with `init_data[0]`
  (any additional bytes are ignored, with a warning — the ISO 22900-2
  5-baud-init payload is always a single address byte). **Superseded in part
  by ADR-076:** as of that ADR, `init_data[0]` is only the *absent-param*
  legacy heuristic's address source (see the bullet below); the explicit
  `CP_InitializationSettings == 1` path now takes its address from
  `CP_5BaudAddressPhys`/`CP_5BaudAddressFunc` (per `CP_RequestAddrMode`) and
  requires an empty `cop_data` (a non-empty one is rejected synchronously) —
  the "any additional bytes are ignored" behavior described here no longer
  applies to that path.
> **Amended by ADR-077:** for `2` specifically, `select_init_sequence`
> resolving to `Fast` is not by itself sufficient for fast-init to actually
> run when `init_data` is empty — every call site historically (and still,
> for the absent-param legacy heuristic) treated empty `cop_data` as "skip
> init entirely" regardless of the selected sequence. ADR-077 adds a single,
> narrow exception: an **explicit** `CP_InitializationSettings == 2` with
> empty `cop_data` on a K-line link now runs a wakeup-only fast-init (the
> D-PDU spec's fast-init service request is optional). The legacy
> (absent-param) path's empty-`cop_data` behavior is unchanged.

- `2` → `InitSequence::Fast`: builds the wakeup `PassThruMessage` and calls
  `fast_init`, exactly as the old "not five-baud" branch did — with **no**
  protocol-specific special-casing. Per J2534-1 v04.04, `FAST_INIT` is valid
  on both K-line protocols (ISO9141 and ISO14230), so this is not a special
  or unusual case: an ISO9141 link with `CP_InitializationSettings=2` simply
  fast-inits. **Superseded in part by ADR-075:** as of that ADR, `init_data`
  (`cop_data`) is payload-only, not the full wakeup frame — the KWP header is
  now constructed at `StartComPrimitive` call time from ComParams/the
  UniqueRespIdTable and carried to `run_protocol_init` as a separate
  pre-built frame; `run_protocol_init` itself is otherwise unchanged.
- `3` → `InitSequence::None`: the init call is skipped entirely — no
  `five_baud_init`, no `fast_init`, no response bytes pushed to the receive
  buffer — and the COP proceeds exactly as if init had succeeded (tester-
  present start, `comm_started = true`, `PduCopstFinished`).
- **Absent from the bound set** → falls back to the pre-existing legacy
  heuristic (ISO9141, or a single-byte `init_data`, selects 5-baud;
  otherwise fast-init). This keeps existing behavior for any link whose
  ComParam set predates this ComParam.
- **Any other value** → defensively warns (logging the offending value) and
  falls back to the same legacy heuristic. This is expected to be
  unreachable once `SetComParam` validation (below) is in place, since every
  seeded default only ever writes `1` or `2`.

`SetComParam` now rejects a `CP_InitializationSettings` value outside
`1..=3` with `Status::invalid_argument` (`comparam_id::is_valid_init_settings`,
mirroring the existing `CP_UartConfig`/`CP_Parity` range-check style from
ADR-071/ADR-027), rather than silently accepting a value that would only
ever hit the legacy-heuristic fallback at `CoptStartcomm` time.

## Consequences

- Resolves the ADR-069/ADR-017 "K-line 5-baud-vs-fast-init selection remains
  a documented limitation" note: a caller can now select the init sequence
  explicitly via `CP_InitializationSettings`, or request none at all,
  independently of the connect-time protocol fixed by the combined K-line
  resource. Supersedes ADR-069's matching Consequences bullet on this point;
  ADR-069's separate decision to fix `ISO9141` as the connect protocol for
  the combined K-line resource is unchanged.
- **Absent-param fallback is a deliberate compatibility shim, not the long-
  term contract.** A link created without any K-line ComParam preset (e.g. a
  direct protocol-id create with no matching resource row) still gets the
  old heuristic. Any future preset that stops seeding
  `CP_InitializationSettings` would silently fall back to it too.
- **`SetComParam` now range-validates `CP_InitializationSettings` to
  `1..=3`.** A client that was previously setting an out-of-range value
  (which had no observable effect before this change, since the ComParam was
  never read) will now see `SetComParam` itself reject the call.
- **No silent ISO9141-forces-5-baud reroute remains for explicit
  `CP_InitializationSettings=2`.** Before this change, `run_protocol_init`
  always chose 5-baud for ISO9141 regardless of anything else. A caller that
  explicitly requests fast-init on an ISO9141-connected link (e.g. via
  `SetComParam(CP_InitializationSettings, 2)`) now gets exactly that request
  forwarded to the adapter as `IOCTL_FAST_INIT` on either K-line protocol —
  per J2534-1 v04.04 this is a normal, successful path on ISO9141, not an
  error case.
- **Verification-pass fix: the mock adapter (`j2534-0404-mock`) previously
  rejected `IOCTL_FAST_INIT` on ISO9141 with `ERR_NOT_SUPPORTED`,
  incorrectly modeling fast-init as ISO14230-only.** This was caught while
  testing this ADR's `resource_0213_default_path_...`/
  `iso9141_explicit_fast_init_setting_...` tests, corrected per an
  authoritative statement from the project owner that J2534-1 v04.04 permits
  `FAST_INIT` on both K-line protocols, and is unrelated to the
  `CP_InitializationSettings` selection logic itself (which forwards
  whatever the ComParam says regardless of adapter behavior). The
  `iso_obd_on_k_line` preset (resource `0x0213`) keeps its
  `CP_InitializationSettings = 2` (fast-init) default from ADR-069 unchanged
  — it was never wrong.
