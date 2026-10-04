# ADR-135: GET_CABLE_ID Rejects With PDU_ERR_FCT_FAILED

**Date:** 2026-07-26
**Status:** Superseded by ADR-187 (GET_CABLE_ID's rejected `PduError`); still supersedes ADR-079 item 15's original error-code choice for the historical record
**Affects:**
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs`
- `j2534-0404-service/docs/iso22900-2-conformance-audit.md` (B23)

## Context

ADR-079 rejected `PDU_IOCTL_GET_CABLE_ID` with `PDU_ERR_CABLE_UNKNOWN`, on the
rationale "no underlying hardware capability" (item 15, and the Consequences
section's "by explicit product decision" framing). Conformance-audit item
B23 flagged this as a probable misreading of ISO 22900-2:2009 Table 53, and
recommended fixing the code and the ADR together if confirmed.

Verification against the actual spec text confirmed the misread. Table 53
(`ISO_22900-2_2009(E)-Character_PDF_document.md:3487-3497`) gives
`PDU_ERR_FCT_FAILED` a command-specific description unique to this table —
(the module does not offer cable detection) — where every
other IOCTL table in the spec renders `PDU_ERR_FCT_FAILED` with a generic
command-failed description. Annex D.3's own enum comments settle the trichotomy:
`PDU_ERR_CABLE_UNKNOWN` (0x82, line 6602-6603) means a cable **is attached** to
the module but its type is **not recognised** — a state that
presupposes cable detection ran and found something; `PDU_ERR_NO_CABLE_DETECTED`
(0x83, line 6605) means detection ran and nothing was attached. This adapter has no
cable-detection capability at all under J2534 v04.04 — it never reaches
either of those two "detection ran" states — so Table 53's `PDU_ERR_FCT_FAILED`
row is the one that actually describes this adapter's condition, not
`PDU_ERR_CABLE_UNKNOWN`.

`PDU_ERR_ID_NOT_SUPPORTED` — the code ADR-079's three sibling capability-gap
rejections use (`GENERIC`, `SEND_BREAK`, `READ_IGNITION_SENSE_STATE`) — was
also considered and rejected for this case: Annex D.3 (line 6581) defines it
as the IOCTL command id not being supported by the implementation, but this service
does recognize the `GET_CABLE_ID` command (it resolves via `GetObjectId` per
ADR-079's own name table); the command is recognized and rejected on
capability grounds, not unrecognized.

Caveat: this hinges on the 2009(E) edition's Table 53 row semantics, the only
edition available for this audit (see CLAUDE.md's spec-references note). A
2022 revision seems unlikely to change this specific trichotomy, but is
flagged as an accepted residual below rather than assumed away.

## Decision

`PDU_IOCTL_GET_CABLE_ID` now rejects with `PduError::PduErrFctFailed` and
message `"PDU_ERR_FCT_FAILED: cable detection is not supported by this
adapter"`, replacing `PduError::PduErrCableUnknown`. The gRPC status code
stays `Code::Unimplemented`, matching this adapter's existing convention for
the other three capability-gap IOCTL rejections (`GENERIC`, `SEND_BREAK`,
`READ_IGNITION_SENSE_STATE`) and `docs/rpc-api-guide.md`'s documented
"reject with `Status::unimplemented`" contract — only the `PduError` detail
was spec-nonconformant, not the transport-level code.

ADR-079's underlying decision — reject `GET_CABLE_ID` outright because this
adapter has no cable-detection hardware capability under J2534 v04.04 — is
**unchanged**. Only the `PduError` communicating that rejection is corrected.

## Consequences

- A client matching on `PduErrCableUnknown` for this specific rejection must
  switch to matching on `PduErrFctFailed`.
- `PduErrFctFailed` is Table 53's own catch-all in this context — a client
  cannot distinguish "this adapter has no cable-detection capability" from a
  more generic internal failure by `PduError` alone for this one IOCTL. That
  overload is imposed by the spec's own table, not introduced by this
  decision.
- Verified against the ISO 22900-2:2009(E) edition only. If ISO 22900-2:2022
  revises Table 53's row semantics for `GET_CABLE_ID`, revisit this decision
  (accepted residual, per this repo's established "verify against 2022"
  annotation convention for 2009-only findings).

**Re-checked against the 2022 edition (2026-08-11): the accepted residual above
has been triggered — this decision's premise no longer holds.** The 2022
text's equivalent table (renumbered Table 54,
`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:3797-3808`) gives
`PDU_ERR_FCT_FAILED` the same non-specific description every other IOCTL
table in the spec uses for this error code — the command-specific wording
this ADR's Context section found unique to the 2009(E) table for this one
IOCTL (describing an adapter that lacks cable-detection capability, and used
as the entire basis for preferring `PDU_ERR_FCT_FAILED` over
`PDU_ERR_CABLE_UNKNOWN`) is absent from the 2022 table. This was a factual
verification note, not itself a new design decision — the follow-up decision
it called for was made and resolved by
[ADR-187](ADR-187-get-cable-id-rejects-with-id-not-supported.md), which
supersedes this ADR's Decision (see this file's Status line).
- Tests: `tests/grpc_mock/pdu_ioctl.rs`'s unsupported-commands coverage for
  `GET_CABLE_ID` now pins `PDU_ERR_FCT_FAILED`/`PduError::PduErrFctFailed`
  instead of `PDU_ERR_CABLE_UNKNOWN`/`PduError::PduErrCableUnknown`.
