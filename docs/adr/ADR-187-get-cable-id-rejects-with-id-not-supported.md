# ADR-187: GET_CABLE_ID Rejects With PDU_ERR_ID_NOT_SUPPORTED

**Date:** 2026-08-23
**Status:** Accepted, superseding ADR-135's Decision (GET_CABLE_ID's rejected `PduError`); ADR-079's underlying no-hardware-capability rejection is unchanged
**Affects:**
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs`
- `j2534-0404-service/docs/iso22900-2-conformance-audit.md` (B23)

## Context

ADR-135 corrected ADR-079's original `PDU_IOCTL_GET_CABLE_ID` rejection code from `PduErrCableUnknown` to `PduErrFctFailed`, based entirely on the ISO 22900-2:2009(E) edition's Table 53: that table's `PDU_ERR_FCT_FAILED` row carried a command-specific description for this one IOCTL (an adapter lacking cable-detection capability), unique among the edition's IOCTL tables — every other table's `PDU_ERR_FCT_FAILED` row means only generic command failure. ADR-135 flagged, but did not resolve, that this reasoning was verified only against the 2009(E) edition, and pre-committed to revisiting if the 2022 edition changed the picture (its own Consequences section, and 2026-08-11 addendum).

The 2026-08-11 addendum confirmed the trigger: the 2022 edition's renumbered Table 54 (`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:3797-3808`) gives `PDU_ERR_FCT_FAILED` the same generic "command failed" description every other IOCTL table in that edition uses — the command-specific wording ADR-135's entire Decision rested on does not exist in the 2022 edition, which is the edition this workspace targets (`docs/j2534-0404-architecture.md:3`).

A design-advisor consult (2026-08-23) re-derived the correct `PduError` from the 2022 edition directly, rather than assuming a reversion to `PduErrCableUnknown`:

- `PduErrCableUnknown`/`PduErrNoCableDetected`'s 2022 Annex D definitions (lines 10316-10319) are substantively unchanged from 2009: both presuppose a real cable-detection attempt ran (found an unrecognized cable / found nothing attached). This adapter never runs cable detection at all under J2534 v04.04 — reverting to either code would reintroduce the exact misread ADR-135 fixed in ADR-079.
- The 2022 edition affirmatively supports `PduErrIdNotSupported` for this case in three places ADR-135 never weighed:
  - Table 12 (`PDUIoCtl`'s function-level return-code set, line 1511) frames `PDU_ERR_ID_NOT_SUPPORTED` as the IOCTL ID not being supported *by this particular MVCI protocol module* — a per-module capability gap, not merely an unrecognized command string.
  - Clause 8.5.25 (line 4285) directs that invoking `TLS_SET_CERTIFICATE` on a module lacking support returns `PDU_ERR_ID_NOT_SUPPORTED`, even though that IOCTL's own Table 68 (lines 4300-4309) does not list the code among its per-command return set. This is the spec's own demonstration that a specific IOCTL's own table omitting `ID_NOT_SUPPORTED` does not bar using it for a module-capability gap — Table 12's function-level contract governs. This directly neutralizes ADR-135's stated reason for excluding it here ("Table 53/54 doesn't list it for this command").
  - The DoIP NOTEs (lines 16451, 16475) use `ID_NOT_SUPPORTED` as the standard signal for a module-level capability gap.
- `PduErrIdNotSupported`'s prior exclusion in ADR-135 ("the command resolves via `GetObjectId`, so it's recognized, not unsupported") proves too much: the three sibling capability-gap rejections this adapter already uses — `GENERIC`, `SEND_BREAK`, `READ_IGNITION_SENSE_STATE` (`rpc_misc.rs`) — also resolve via `GetObjectId` and already use `PduErrIdNotSupported`. The only thing that ever justified `GET_CABLE_ID` diverging from those three was the 2009(E) Table 53's special-cased row; under the 2022 target edition, that row no longer exists and the divergence has no remaining basis.

## Decision

`PDU_IOCTL_GET_CABLE_ID` now rejects with `PduError::PduErrIdNotSupported` and message `"PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_GET_CABLE_ID is not supported by this adapter"`, replacing `PduError::PduErrFctFailed`. The gRPC status code stays `Code::Unimplemented`, unchanged — this restores the identical code/message shape the three sibling capability-gap IOCTLs (`GENERIC`, `SEND_BREAK`, `READ_IGNITION_SENSE_STATE`) already use.

ADR-079's underlying decision — reject `GET_CABLE_ID` outright, since this adapter has no cable-detection hardware capability under J2534 v04.04 — remains unchanged. Only the `PduError` communicating that rejection changes, for the second time.

## Consequences

- A client matching on `PduErrFctFailed` for this specific rejection must switch to matching on `PduErrIdNotSupported` — the same kind of transition ADR-135 itself already introduced once.
- All four capability-gap module-scoped IOCTLs (`GENERIC`/`GET_CABLE_ID`/`SEND_BREAK`/`READ_IGNITION_SENSE_STATE`) now share one uniform `PduError`, restoring the consistency ADR-135's 2009-edition-driven exception had broken.
- `PDU_ERR_ID_NOT_SUPPORTED` is not itself listed in the 2022 edition's Table 54 (the `GET_CABLE_ID`-specific IOCTL row), only in Table 12's function-level return set. A client validating strictly against Table 54's own enumerated codes could see this as an out-of-table response; this is accepted on the strength of the clause 8.5.25/`TLS_SET_CERTIFICATE` precedent, which the spec itself uses the identical shape for.
- 2009(E)-edition clients that adopted ADR-135's `PduErrFctFailed` lose that mapping; this workspace's declared target is the 2022 edition, so this divergence from the 2009 text is accepted, matching this repo's established edition-target policy.
- Tests: `tests/grpc_mock/pdu_ioctl.rs`'s unsupported-commands coverage for `GET_CABLE_ID` now pins `PDU_ERR_ID_NOT_SUPPORTED`/`PduError::PduErrIdNotSupported` instead of `PDU_ERR_FCT_FAILED`/`PduError::PduErrFctFailed`.
