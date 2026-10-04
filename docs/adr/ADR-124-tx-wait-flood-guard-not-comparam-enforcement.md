# ADR-124: TX WAIT-Flood Guard Is an Internal Safety Bound, Not `CP_CanMaxNumWaitFrames` Enforcement (Supersedes ADR-121)

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service` service, service/events.rs, service/rpc_primitive.rs, docs/adr/ADR-121

## Context

ADR-121 (PR #130, conformance-audit finding A2-11) enforced `CP_CanMaxNumWaitFrames`
(native constant `ISO15765_WFT_MAX`) in the software ISO-TP TX driver's FlowControl-wait
loop (`isotp_send`, `events.rs`): consecutive `FS_WAIT` frames received while waiting for a
peer's FlowControl were counted against the comparam's configured value, aborting the COP
with `PduErrEvtProtErr` once exceeded.

A post-merge Codex review round on PR #130 challenged this direction: `CP_CanMaxNumWaitFrames`
bounds how many `FS_WAIT` frames THIS service is permitted to *transmit* when it is the
*receiver* of an incoming segmented message (generating its own FlowControl replies), not
how many `FS_WAIT` frames it *tolerates from a peer* while it is the *sender* — which is
where ADR-121's entire enforcement lived. Re-investigation confirmed this, at high (not
absolute) confidence, from evidence available in this workspace:

- **`names.rs`'s own grouping.** `CP_StMin`/`ISO15765_STMIN`, `CP_BlockSize`/`ISO15765_BS`,
  and `CP_CanMaxNumWaitFrames`/`ISO15765_WFT_MAX` are grouped under a `// ISO15765
  flow-control` comment (`names.rs:698-708`, `:1277-1288`), immediately followed by a
  *separate* group under `// ISO15765 Tx-side flow-control overrides`:
  `CP_BlockSizeOverride`/`BS_TX`, `CP_StMinOverride`/`STMIN_TX`. `CP_BlockSize`/`CP_StMin`
  (the non-override pair) configure the FlowControl frames *this service generates* when it
  is the receiver reassembling an incoming message (`events.rs`'s RX-side FC construction,
  `isotp.rs:397`). `WFT_MAX` slots naturally into that same generated-FC trio. There is no
  `WFT_MAX_TX`-style sibling constant anywhere in this codebase — J2534 defines no distinct
  TX-side WAIT-tolerance knob at all.
- **The J2534-1 v04.04 native default is `0`** (Table B.20,
  `vehicle-comm-specs/j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru
  Vehicle Programming.md:1264`). Under the sender-tolerance reading ADR-121 used, an
  unconfigured device would abort a TX transfer on the very first WAIT any ECU sends — an
  awkward default for a specification titled *Pass-Thru Vehicle Programming*. Under the
  receiver-self-limit reading, default `0` is coherent: an unconfigured pass-thru device
  simply never emits WAIT itself.
- **ISO 22900-2:2009's one-liner is directionally silent but not contradictory**
  (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5992`,
  which describes the ComParam as a cap on the WAIT flow-control frames tolerated over a
  multi-segment transfer, with SAE J1939 reading it as a cap on CTS frames instead) — the J1939 variant (CTS frames) also points at the
  *transmitting* role of a receiver, since CTS/hold frames are what a receiver sends.
- The genuinely normative ISO 15765-2 network-layer text, which would settle this
  unambiguously, is confirmed **not present anywhere in this workspace** (same caveat
  ADR-121 already carried). This verdict is therefore a documented judgment call from
  available in-workspace evidence, not a citation of the controlling spec — re-verify
  against ISO 15765-2 directly, or against ISO 22900-2:2022 if either becomes available in
  this workspace.

Separately: this codebase's software-ISO-TP RX/reassembly path only ever emits
`FS_CONTINUE_TO_SEND` (`isotp.rs:397`, `events.rs:1055`) — it never sends `FS_WAIT` under
any condition today. So even under the receiver-self-limit reading, `CP_CanMaxNumWaitFrames`
is currently satisfied *trivially* by this implementation (sending zero WAITs conforms to
any configured allowance, including `0`) — there is no live site to move ADR-121's
enforcement *to*, and building one (deciding when this service should itself emit WAIT under
backpressure) is a new feature, well outside A2-11's original scope.

The underlying problem A2-11 identified is still real, independent of which direction the
comparam governs: without *some* bound, an ECU (or fault) emitting `FS_WAIT` faster than
`N_Bs` re-arms this driver's FlowControl wait indefinitely, and the COP never completes.

## Decision

1. **`CP_CanMaxNumWaitFrames` enforcement in `isotp_send` is removed.** The software RX
   path already conforms to it trivially (see above); native `SET_CONFIG` forwarding
   (`comparam_id.rs`) is untouched and remains correct under the receiver-self-limit
   reading.
2. **The liveness guard against a WAIT flood is retained, but decoupled from the comparam.**
   `isotp_send`'s FC-wait loop keeps the `wait_frames` counter, the `FcCapture` batch-drain
   mechanism (ADR-121's Consequences bullet on `MAX_POLL_MESSAGES`-sized batches — still
   correct and still needed for whatever bound is enforced), the consecutive-per-FC-cycle
   reset semantics (ADR-121 Decision point 1, unchanged), and the `PduErrEvtProtErr` overrun
   mapping (ADR-121 Decision point 2, unchanged) — but the threshold is now a fixed internal
   constant, `ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES = 1027`, not `opts.wft_max` /
   `CP_CanMaxNumWaitFrames`. `1027` is ISO 22900-2's own documented upper bound for this
   comparam's `[0, 1027]` range (`...md:5992`) — deliberately the most permissive value any
   conforming D-PDU configuration could ever grant a peer, so this guard cannot reject any
   value a real peer/protocol preset would produce; it exists purely to bound a pathological,
   effectively-infinite flood, not to enforce a client-configurable policy.
3. **ADR-121's Decision point 3** (`CP_CanMaxNumWaitFrames = 0` meaning zero WAITs
   tolerated, and its supporting `SoftIsoTpTx.wft_max`/`ComParamSet::isotp_wft_max()`
   implementation) **is void** — those were the comparam-linkage this ADR removes.
   ADR-121's Decision points 1 and 2 describe the guard's *shape* (scope, error mapping) and
   remain accurate for the internal-constant guard this ADR keeps.
4. **A2-11 is reclassified, not reopened.** The comparam side is trivially conformant (RX
   never sends WAIT); the liveness hole is closed by an internal guard, not by comparam
   enforcement — `iso22900-2-conformance-audit.md`'s A2-11 entry is updated to describe
   this rather than claiming `CP_CanMaxNumWaitFrames` is "enforced."

## Consequences

- ISO15765-4 (OBD) sessions no longer abort on the first `FS_WAIT` they receive (a
  behavior ADR-121 introduced based on the now-superseded sender-tolerance reading) — an
  OBD ECU sending `FS_WAIT` is still a spec violation (ISO 15765-4 forbids `FC.WAIT`), but
  this implementation no longer treats a single instance as fatal; it is now bounded by the
  same generous internal guard as every other profile. Accepted residual: a genuinely
  non-conformant OBD ECU could send up to 1027 consecutive WAITs before this driver aborts,
  rather than aborting on the first one.
- `CP_CanMaxNumWaitFrames` is no longer read anywhere in `j2534-0404-service`'s software
  path (`SoftIsoTpTx.wft_max`, `ComParamSet::isotp_wft_max()`, and the `rpc_primitive.rs`
  population site are all removed); it remains fully forwarded to native hardware via
  `SET_CONFIG`, unaffected by this change.
- If ISO 15765-2's normative text, or ISO 22900-2:2022, becomes available in this
  workspace, re-verify the direction verdict above before assuming it still holds — this
  ADR's Context section is explicit that it is a judgment call from indirect evidence, not
  a spec citation.
- The `FcCapture` batch-drain mechanism this ADR retains from ADR-121 (queuing every
  matching FlowControl frame in a `PassThruReadMsgs` batch, not just the first) remains
  necessary and correct regardless of which threshold gates on it.
