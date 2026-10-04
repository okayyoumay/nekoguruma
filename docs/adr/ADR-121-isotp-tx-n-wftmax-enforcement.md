# ADR-121: Software ISO-TP TX Driver Enforces N_WFTmax (`CP_CanMaxNumWaitFrames`)

**Date:** 2026-07-23
**Status:** Superseded by ADR-124
**Affects:** `j2534-0404-service` service, service/events.rs, service/rpc_primitive.rs

## Context

Conformance-audit finding A2-11 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`):
the software ISO-TP TX driver's FlowControl-wait loop (`isotp_send`, `events.rs`, the
ADR-046 `"software-isotp"` engine) re-arms its N_Bs timeout on every `FS_WAIT` frame with
no counter, so an ECU (or a fault condition) emitting `FS_WAIT` faster than `N_Bs` re-arms
indefinitely and the COP never completes on this axis. `CP_CanMaxNumWaitFrames` (native
constant `ISO15765_WFT_MAX`) is the comparam that is supposed to bound this — and it is
already seeded and forwarded correctly to *native* J2534 hardware channels
(`comparam_id.rs`'s `is_can_param`/`to_j2534_config_value`) — but the software path never
read it.

Neither in-repo spec text elaborates on `N_WFTmax` beyond a one-line description: SAE
J2534-1 (v04.04) Table B.20
(`vehicle-comm-specs/j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru
Vehicle Programming.md:1264`) and ISO 22900-2:2009(E)
(`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5992`) both
describe it only in general terms, as a limit on how many WAIT flow-control frames
are tolerated over the course of a multi-segment transfer (`PDU_PT_UNUM32`, range `[0, 1027]`, defaults `ISO_15765_2=255`,
`ISO_15765_4=0`, `SAE_J1939_21=255` — matching this codebase's own existing
`comparam_defaults.rs` presets). The normative behavior this comparam actually bounds — ISO
15765-2's own network-layer `N_WFTmax` parameter — is defined in the ISO 15765-2 document
itself, which is not present anywhere in this workspace. Implementing this fix correctly
therefore requires committing to three interpretive choices a future reader could otherwise
re-litigate from the same two thin spec-table rows.

## Decision

Enforce `N_WFTmax` in the software ISO-TP TX driver per ISO 15765-2's network-layer
semantics:

1. **Scope: consecutive `FS_WAIT` frames within one FlowControl-wait cycle, not a
   cumulative count across the whole multi-segment transfer.** ISO 15765-2 defines
   `N_WFTmax` as a limit on *consecutively received* `FC.WAIT` frames; the counter resets
   the moment `FC.CTS` (Continue To Send) arrives and a new block of ConsecutiveFrames is
   sent. The "over a multi-segment transfer" wording in both in-repo spec tables is a
   loose paraphrase of this, not a mandate to sum WAIT frames across the entire transfer.
   Implementation: a `wait_frames: u32` counter local to each pass through `isotp_send`'s
   FC-wait loop, reset to `0` at the top of every `while idx < chunks.len()` iteration (i.e.
   every time a new block starts waiting for FlowControl) — entering a new block after a
   `CTS` *is* the reset.
2. **Overrun reports `PDU_ERR_EVT_PROT_ERR`, not `PDU_ERR_EVT_RX_TIMEOUT`.** Exceeding
   `N_WFTmax` is the ECU exhausting a negotiated allowance while still actively responding —
   the same failure family as `FS_OVERFLOW`, which this exact `match` arm already maps to
   `PduErrEvtProtErr`. `PduErrEvtRxTimeout` (used when `N_Bs` itself expires with no
   FlowControl frame at all) would misrepresent an active, responding ECU as an unresponsive
   one.
3. **`CP_CanMaxNumWaitFrames = 0` means zero WAIT frames are tolerated, not "unlimited."**
   The value `0` is ISO 15765-4 (OBD)'s own default in this codebase's existing
   `comparam_defaults.rs`, and ISO 15765-4 forbids `FC.WAIT` outright (CTS-only, `BS=0`) — a
   conforming OBD ECU never sends `FS_WAIT`, so this reading cannot spuriously break normal
   OBD sessions; an `FS_WAIT` received under this profile is a genuine protocol violation and
   should abort. The check is `wait_frames > wft_max` performed *after* incrementing on each
   `FS_WAIT`, so a limit of `255` permits `255` consecutive WAITs and a limit of `0` permits
   none.

Supporting implementation:

- `SoftIsoTpTx` (`service.rs`) gains a `wft_max: u32` field, populated at COP-start time
  (`rpc_primitive.rs`) from a new `ComParamSet::isotp_wft_max()` getter reading
  `ComParamId(j2534_0404::ISO15765_WFT_MAX)`. Unlike `isotp_n_bs_timeout_ms`'s
  `.filter(|&v| v > 0)` fallback pattern, `0` is a meaningful, deliberately-chosen value here
  and must not be filtered out; the getter defaults to `255` (the ISO 15765-2 default) only
  when the comparam is altogether absent from the set, which should not normally happen since
  every protocol preset seeds it.

## Consequences

- An ECU that emits `FS_WAIT` faster than `N_Bs` now aborts the COP with `PduErrEvtProtErr`
  once it exceeds `CP_CanMaxNumWaitFrames`, instead of holding the poll task in an unbounded
  re-arm loop — closes A2-11.
- ISO15765-4 (OBD) software-ISO-TP sessions now abort immediately on the very first
  `FS_WAIT` they receive (the spec-conforming behavior for a profile that forbids WAIT
  frames), where previously the driver would simply keep re-arming.
- The software path now honors ISO 22900-2's documented `[0, 1027]` range for this comparam,
  while the native `PassThruIoctl(SET_CONFIG)` path is still capped at the J2534 IOCTL's
  8-bit range (`0x0`-`0xFF`) — a pre-existing, out-of-scope-here divergence between the two
  paths, tracked as a backlog item in `j2534-0404-service/docs/implementation-notes.md`.
- If the ISO 22900-2:2022 edition or the ISO 15765-2 document itself becomes available in
  this workspace, re-verify the three interpretive choices above against the normative text
  directly rather than assuming this ADR's reading is still correct.
- `FcCapture` (the FlowControl-wait probe `isotp_send` passes into `poll_rx_inner`) queues every matching FlowControl frame in a poll batch, not just the first — a single `PassThruReadMsgs` call can return up to `MAX_POLL_MESSAGES` (8) frames, and a burst of consecutive `FS_WAIT` frames landing in one batch must all count toward `N_WFTmax`, not just the first one per poll (Codex review finding, PR #130). Frames still queued after a batch's terminal `FS_CONTINUE_TO_SEND` are discarded uncounted, per ISO 15765-2's rule that an unexpected FlowControl N_PDU is ignored.
