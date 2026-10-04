# ADR-188: SAE J2534-2 TP2.0 (Phase 7, Stage 7a — Active Connections)

**Date:** 2026-08-23
**Status:** Accepted (Consequences §7's RxFlag-forwarding deferral resolved, by decision, by
             ADR-191; §5's Stage 7c staged-delivery deferral fulfilled by ADR-192; §7's
             `TP2_0_CHx` Additional Channels deferral fulfilled by ADR-210)
**Affects:**
- `j2534-0404-service/src/service/protocol.rs`
- `j2534-0404-service/src/service/resources.rs`
- `j2534-0404-service/src/service/service_params.rs`
- `j2534-0404-service/src/service/comparam_support.rs`
- `j2534-0404-service/src/service/comparam_defaults.rs`
- `j2534-0404-service/src/service/comparam_id.rs`
- `j2534-0404-service/src/service/names.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service/rpc_primitive.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/src/service/tx_header.rs`
- `j2534-0404-service/src/service/events.rs`
- `j2534-0404-service/src/service/events_rx_routing.rs`
- `j2534-0404-service/src/service/events_tp20_connection.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-mock/src/lib.rs`
- `j2534-0404-service/docs/implementation-notes.md`
- `docs/j2534-2-support-plan.md`

## Context

SAE J2534-2 (DEC2020) clause 19 defines TP2.0 (VW/Audi transport protocol,
SAE J2819) — the next unshipped phase in `docs/j2534-2-support-plan.md`
whose full dependency chain (Phases 0-2) has already shipped, and the one
phase the plan's own up-front assessment flagged as needing a
`design-advisor` session before implementation ("connection-oriented
lifecycle, concurrency"). Two corrections to the plan surfaced during that
consult (folded into `docs/j2534-2-support-plan.md` in this same PR):

1. Clause 19.3.1 requires **four simultaneous connections total**, of which
   at most one may be the passive (inbound-accepting) one — that passive
   connection is included in the four rather than being a fifth on top.
2. Clause 19.3.3.1's own prose says "ten new configuration parameters," but
   its Table 77 lists twelve — the ten timing/count parameters plus
   `TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE`, the two passive-connection
   enablers (both default to 0 = disabled).

ISO 22900-2 (neither the 2009(E) nor the 2022 edition) defines any TP2.0/
J2819/VWTP equivalent — verified by an exhaustive grep of both spec files.
TP2.0 therefore needs a standalone D-PDU identity, the same category as
UART Echo Byte (ADR-170), Honda DIAG-H (ADR-174), SAE J1708 (ADR-175), and
SAE J1939 (ADR-179) already occupy — not a CAN-family bus-type variant
(SWCAN/FT-CAN's ADR-164/168 shape) and not a ComParam-inferred substitution
(CAN FD's ADR-158 shape): clause 19 frames TP2.0 as its own transport state
machine, not "CAN with listed exceptions" the way clause 20.1
frames FT-CAN.

Phase 0 (ADR-152) already added every constant this stage needs to
`j2534_v0404.h` — `PROTOCOL_TP2_0_PS` (0x800E), `IOCTL_REQUEST_CONNECTION`/
`_TEARDOWN_CONNECTION` (0x800A/0x800B), `CONFIG_TP2_0_*` (0x8044-0x804F),
`TX_FLAG_TP2_0_BROADCAST_MSG` (bit 16), `RX_FLAG_CONNECTION_ESTABLISHED`/
`_LOST` (bits 16/17), `DEVICE_INFO_TP2_0_SUPPORTED`/`_PS_J1962`/
`_SIMULTANEOUS`, and `ERR_NO_CONNECTION_ESTABLISHED` — so this stage needs
no header edit and no bindgen regeneration.

The central design question is how clause 19's connection-oriented model
(up to four independent logical connections, each individually
establish/teardown'd via `IOCTL_REQUEST_CONNECTION`/`_TEARDOWN_CONNECTION`,
multiplexed over one native `PassThruConnect(TP2_0_PS)` channel and
addressed purely by a 4-byte CAN-ID prefix on the wire, Table 80/clause
19.4.4.1) maps onto the D-PDU ComLogicalLink object model, and specifically
whether this needs the "genuinely new RPC-level semantics" ADR-178's
Consequences section flagged as a possible exception to its own
proto-freeze rule.

## Decision

**One D-PDU CLL models one TP2.0 connection.** Multiple CLLs join the same
physical `TP2_0_PS` channel through the existing `ChannelKey`/
`SharedChannel` ref-count machinery (the same sharing mechanism every other
multi-CLL-per-channel protocol already uses), each tracking its own
connection state in `LogicalLinkState`. `REQUEST_CONNECTION`/
`TEARDOWN_CONNECTION` are invoked **internally** from `PDU_COPT_STARTCOMM`/
`PDU_COPT_STOPCOMM` and CLL teardown — never exposed as client-visible
`PDU_IOCTL_*` commands. This is the direct application of
`docs/j2534-2-support-plan.md` §5 step 6's own guidance: a new raw IOCTL
"may instead need to be invoked internally ... behind an implicit protocol
operation" rather than becoming a new client-visible D-PDU command — the
D-PDU object model already contains the verb this needs (`CoptStartcomm`
establishes communication with an ECU; `CoptStopcomm` ends it). This
answers the ADR-178 question directly: TP2.0's lifecycle **does** fit
within the frozen interface. No new IOCTL id, no new `bytearray_data`
layout, no new proto surface.

This model was chosen over the alternative (one CLL = the whole TP2.0
channel, with `REQUEST_CONNECTION`/`TEARDOWN_CONNECTION` exposed as new
`L`-scoped `IoCtl` commands, mirroring Repeat Messaging's shape) because
the D-PDU COP model has no concept of addressing one of several
multiplexed connections within a single CLL — every `CoptSendrecv`/
`CoptStartcomm` targets a CLL, not a sub-connection within it — while
clause 19's own frame-routing rule (an inbound frame is attributed to a
connection by matching its 4-byte CAN-ID prefix against that connection's
established RX-ID, Table 80/clause 19.4.4) is structurally identical to
how this service already routes frames to one of several CLLs sharing a
physical CAN channel (the SAE J1939 source-address routing precedent,
ADR-184's `UniqueRespIdKey::matched`).

### 1. `ChannelProtocol` and resource mapping

New standalone identity `ChannelProtocol::TP2_0_PS = Self(0x0000_800E)`
(`protocol.rs`, mirroring `J1939_PS`/`J1708_PS`'s self-mapping,
`hw_protocol_override: None` shape) — `is_can_family()` stays `false`
automatically, correctly excluding TP2.0 from ADR-046's dual-channel/
software-ISO-TP machinery and the CAN-mode probe. One new resource-table
row, `resource_id: 0x025F` (the next free id after SAE J1939's `0x025E`),
minted protocol name `SAE_J2819_TP2_0` on a minted standalone bus type,
`TP2_0_DWCAN` (the `ANALOG_IN`/`BUSTYPE_ANALOG_IN` minting precedent,
ADR-177 — not `ISO_11898_2_DWCAN`, which would falsely imply an ISO 22900-2
sanction that doesn't exist). `tx_message_size_range`: `4..=4096` per Table
80 (4-byte CAN-ID prefix + up to 4092 data bytes at the native-frame
level). Pins: clause 19.2.2 documents exactly pins 6 and 14, no default —
closed-set `resolve_pin_selection` arm accepting only that pair
(`0x0000_060E`, the FT-CAN two-pin-closed-set shape), always issuing an
explicit `SET_CONFIG(CONFIG_J1962_PINS)`. `install_pass_all_filter` is
skipped for this protocol (ADR-177's Analog-Inputs precedent) — clause
19's per-connection addressing is the RX model, not a pass-all baseline.

### 2. Connection lifecycle

`handle_start_comm` gains a TP2.0 arm:

1. Rejects a non-empty `cop_data` (the inverse of UART Echo Byte's
   mandatory-one-byte contract, ADR-170 Decision — TP2.0 has no COP-borne
   initialization payload at all).
2. Snapshots the five newly-minted Working ComParams (below) and packs the
   11-byte `SBYTE_ARRAY` request per Table 78 (bytes 0-3 setup CAN ID, byte
   4 destination address, byte 5 the fixed opcode `0xC0`, bytes 6-7
   proposed TX-ID, bytes 8-9 proposed RX-ID, byte 10 application type).
3. Issues native `IOCTL_REQUEST_CONNECTION` and registers a per-physical-
   channel routing-map entry keyed on the proposed RX-ID, tagged with the
   CLL's `connect_generation` (the structural sibling of SAE J1939's claim-
   indication map, `events_j1939_claim.rs`, ADR-179/ADR-180's staleness
   discipline: a stale generation's indication is dropped, not
   misattributed).
4. Waits — without holding `logical_links`, reusing the SAE J1939 claim-
   loop's polling shape (ADR-179/ADR-180) rather than re-deriving it — for
   the matching `RX_FLAG_CONNECTION_ESTABLISHED`/`_LOST` indication
   (RxStatus bits 16/17), consumed from the ordinary RX poll path. The
   deadline is derived from the connection's own `CP_TP20T_E` ×
   (`CP_TP20MNTC` + 1) Working values (Table 77 defaults: 100 ms × 11 ⇒ a
   ~2 s floor).
5. `CONNECTION_ESTABLISHED`: records the established TX-ID from the
   indication's `Data[4..7]`, `CoptStartcomm` succeeds. `CONNECTION_LOST`:
   maps `Data[4]`'s reason byte — `1` (timeout) to the existing COP-timeout
   error shape; `0xD6`/`0xD7` to a not-supported rejection; `0xD8` to the
   existing `PduErrResourceBusy` shape (the Phase 13 `ERR_*_IN_USE`
   precedent).

`handle_stop_comm`, plus every existing `DestroyComLogicalLink`/
`DisconnectComLogicalLink` cleanup site (the same call sites Repeat
Messaging's Decision 4 already touches, `rpc_link.rs`), issue a best-effort
`IOCTL_TEARDOWN_CONNECTION` keyed on the connection's own established
RX-ID (clause 19.3.3.3: teardown is keyed on the original request's RX-ID).
`PassThruDisconnect` at `ref_count == 0` remains the backstop.

TX: `tx_header.rs` gains a TP2.0 arm prepending the established 4-byte
TX-ID to the client's payload (0..=4092 bytes); a send on a CLL whose
connection phase is not `Established` rejects via the existing no-
connection error mapping. RX: `events.rs`'s header/footer length table
gains a 4-byte-header/no-footer arm; per-CLL routing matches an inbound
frame's `Data[0..3]` against that CLL's own established RX-ID (the ADR-184
routing shape) — and, since round 9 (Codex review fix, PR #97, Fix M),
against that CLL's own established TX-ID too, but ONLY for a frame the
poll loop already flags `is_tx_side` (round 10, Codex review fix, PR #97,
Fix N — round 9's Fix M first added the TX-ID tier unconditionally, which
was itself a fresh cross-CLL leak: a genuine inbound frame addressed to a
sibling's own RX-ID could match this entry too whenever that RX-ID happened
to equal this connection's TX-ID): a `CP_Loopback`-enabled write's own
device-generated echo carries the written frame's own prefix (our TX-ID),
not an RX-ID any peer ever addresses us with, so RX-ID-only matching
dropped it (or misdelivered it to whichever sibling's RX-ID happened to
equal this TX-ID) until Fix M added the second field — gated, since Fix N,
on the frame actually being our own echo. Fix N's own gate was one-sided
until round 12 (Codex review fix, PR #97, Fix P): `tp20_rx_id` still
matched a TX-side frame unconditionally, the same class of leak mirrored
onto the other tier — a TX-side echo of a DIFFERENT connection's own write
could match a sibling's entry via `tp20_rx_id` whenever that other
connection's TX-ID happened to equal the sibling's own RX-ID. The two tiers
are now strictly mutually exclusive by frame kind: `tp20_tx_id` only
eligible when the frame is our own echo, `tp20_rx_id` only eligible
otherwise.

Per-CLL connection state — `requested_rx_id`, `established_tx_id`, and a
`Requested`/`Established`/`Lost` phase — lives in a new `LogicalLinkState`
field under the existing `logical_links` lock (the same placement Repeat
Messaging's `repeat_message_ids` already uses, ADR-165 Decision 4). The
routing map lives per physical channel, beside the SAE J1939 claim map,
under the same lock discipline.

### 3. No new client-visible IOCTLs

Per the Decision's opening paragraph: none. `PDU_IOCTL_BASE`'s next-free
offsets stay free.

### 4. ComParam surface

Existing ComParams allowlisted for TP2.0 with their existing native
translations: `CP_Baudrate` → `DATA_RATE` (bustype default seeded to
500000, clause 19.3.1's fixed 500 kbps requirement; unlike Honda DIAG-H,
this stays client-settable since Table 77 lists `DATA_RATE` as a supported
override), `CP_Loopback`, `CP_BitSamplePoint` (default 80), and
`CP_SyncJumpWidth` (default 15). `CP_TesterPresentSendType` is excluded by
allowlist construction (the UART Echo Byte Decision 3 mechanism, ADR-170)
— clause 19.3.1 requires the device to autonomously maintain the
connection; no client-driven periodic keep-alive concept exists.

Five **minted** ComParams (no ISO 22900-2 source; the `CP_AnalogSampleRate`
naming/id-range convention, ADR-178), ids **`0x80C9`-`0x80CD`** (the actual
next-free block in `service_params.rs` — `0x80C5`-`0x80C8` are already
taken by `PARAM_W1_MIN`/`_W2_MIN`/`_W3_MIN`/`_W4_MAX`, so the design
consult's originally-proposed `0x80C5`-`0x80C9` range was corrected during
implementation to avoid a collision):

- `CP_TP20ChannelSetupCanId` (`0x80C9`)
- `CP_TP20DestinationAddress` (`0x80CA`)
- `CP_TP20TxIdProposal` (`0x80CB`)
- `CP_TP20RxIdProposal` (`0x80CC`)
- `CP_TP20ApplicationType` (`0x80CD`)

`to_j2534_config_id` returns `None` for all five (service-level-only,
consumed directly by `handle_start_comm`'s `SBYTE_ARRAY` packing rather
than forwarded to a native `SET_CONFIG`, the `CP_AnalogSampleRate` shape).
Validated present and well-formed at `CoptStartcomm` time, not
`CreateComLogicalLink` time (the CAN-FD/ISO15765-on-CAN-FD deferral
shape, ADR-158/159).

Table 77's ten timing/count ComParams are **deferred at native defaults**
this stage (the UART Echo Byte `UEB_T*`/Analog-Inputs acquisition-params
deferral precedent, ADR-170/177 — the spec-mandated defaults are
functional). `TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE` are deferred to Stage
7b (native default `0` = passive feature disabled, the safe default).

### 5. Staged delivery

Three sub-stage PRs, mirroring Phase 2's 2a/2b and Phase 3's 3a/3b/3c
splits (ADR-155):

- **7a — active connections** (this ADR): everything above.
- **7b — passive connection**: `TP2_0_IDENTIFER`/`TP2_0_RXIDPASSIVE`,
  device-side inbound accept, and how to surface an unsolicited inbound
  `CONNECTION_ESTABLISHED` to a client — genuinely open, and explicitly
  **not** resolved by this ADR; needs its own `design-advisor` round before
  implementation starts.
- **7c — broadcast + re-triggered broadcast**: `TX_FLAG_TP2_0_BROADCAST_MSG`
  client surface and the `PassThruStartPeriodicMsg` re-trigger path
  (clause 19.4.5.3) — needs a precondition check (whether the frozen
  proto's `TxFlagBit` enum already carries a value for this bit) before its
  own mechanism is chosen.

### 6. Mock support

`j2534-0404-mock` gains a per-`TP2_0_PS`-channel connection table (4
slots): `IOCTL_REQUEST_CONNECTION` validates the request's `NumOfBytes ==
11`, allocates a slot and queues a `CONNECTION_ESTABLISHED` indication
(RxStatus bit 16, `Data[0..3]` echoing the requested RX-ID, `Data[4..7]` a
mock-assigned TX-ID) or, when all 4 slots are taken, a `CONNECTION_LOST`
indication with reason `0xD8`; `IOCTL_TEARDOWN_CONNECTION` validates
`NumOfBytes == 4` and a matching slot (else `ERR_INVALID_IOCTL_VALUE`) and
queues a `CONNECTION_LOST` indication with reason `0`.
`PassThruWriteMsgs` implements clause 19.4.4's three-way frame-routing
rule and Table 80's size checks, rejecting an oversized non-connection
write with `ERR_NO_CONNECTION_ESTABLISHED` (already mapped in Phase 0).
`DEVICE_INFO_TP2_0_SUPPORTED` is advertised per the existing ADR-185 mock
pattern; `SET_CONFIG` accepts the `CONFIG_TP2_0_*` ids at Table 77's
default values. The mock owns the real connection state machine (the same
"the mock stands in for the device this design forwards to" rationale as
Repeat Messaging, ADR-165 Decision 6).

### 7. Explicitly out of scope this stage

- `TP2_0_CHx` Additional Channels (every prior phase's own deferral
  precedent).
- Passive connections (Stage 7b).
- Broadcast frames and periodic re-trigger (Stage 7c) — later resolved by ADR-192: a minted
  per-send-scoped ComParam (`CP_TP20BroadcastAddress`) for the broadcast-send client surface,
  and a reintroduced native `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg` pairing (the
  one exception to this codebase's general move away from that native call, per ADR-093) for
  the periodic re-trigger path. See ADR-192 for the full reasoning.
- Table 77's ten timing/count ComParams (deferred at native defaults).
- A raw single-frame send to a non-connection address (clause 19.4.4's
  third routing case) — this service's payload-only TX contract for a
  connection-bound CLL carries no client-supplied CAN ID to address such a
  send with; rejected outright, not silently mishandled.
- Forwarding RxStatus bits 16/17 into a client-visible `RxFlag` — later
  resolved, by explicit decision rather than further deferral, by ADR-191:
  these indication frames are state-machine input, not vehicle content, so
  tagging them into the content-delivery stream would be a category error,
  and no ISO 22900-2 `RxFlag` bit exists for them either. See ADR-191 for
  the full reasoning.
- `DEVICE_INFO_TP2_0_SIMULTANEOUS` and `_PS_J1962` discovery wiring
  (consistent with every ADR-185 Stage-1 family's own `_SIMULTANEOUS`
  residual — only the flat `DEVICE_INFO_TP2_0_SUPPORTED` bit is wired into
  `resources::connect_discovery_check`).

## Consequences

- A leaked native connection slot, if a best-effort teardown fails while a
  sibling CLL keeps the physical channel open, is an **accepted residual**
  this stage — unlike a leaked Repeat Messaging slot (ADR-165 round 2), a
  stranded TP2.0 connection self-heals device-side via the connection's own
  mandatory maintenance-timeout (clause 19.3.1), and the `ref_count == 0`
  `PassThruDisconnect` backstop remains the eventual cleanup path. Revisit
  if a Codex review or later audit finds this insufficient in practice —
  the `SharedChannel::leaked_repeat_message_ids` shape is the ready-made
  precedent to copy if so. This wording already covers the residual's own
  narrowing across two later Codex review rounds against PR #97 (see
  `j2534-0404-service/docs/implementation-notes.md`'s TP2.0 narrative section
  for the mechanism): the residual used to mean "no best-effort teardown was
  ever attempted on abandonment" for both the connection-request wait's own
  local-deadline timeout (round 3, Fix E) and a requesting CLL going stale
  mid-wait after its native call already succeeded (round 4, Fix G) — both
  now attempt the same best-effort `IOCTL_TEARDOWN_CONNECTION`, narrowing
  the accepted residual to "only a teardown call that itself fails" in both
  cases, exactly as this bullet already anticipated. A third round (round
  7, Fix J) closed a related but distinct gap the teardown attempt alone
  didn't: a best-effort teardown call returning does not itself guarantee
  the abandoned request's own delayed native indication has already
  arrived, so an immediate same-`cll_handle` retry proposing the identical
  RX-ID could previously register a new routing entry indistinguishable
  from the abandoned one (both share the same `cll_handle`/
  `connect_generation`, since no disconnect occurred), letting the stale
  indication misattribute onto the retry. `SharedChannel::tp20_connections`
  entries now carry their own `abandoned` flag: an abandonment marks the
  entry in place instead of removing it, the pre-insert conflict check
  rejects any new request (sibling or self-retry) against a still-
  `abandoned` entry, and the entry is only actually removed once its
  terminal indication finally arrives (or, as an accepted residual, never
  self-heals from an indication that never comes at all, distinct from and
  layered on top of the native-slot-leak residual above). Two more rounds
  (round 8, Fixes K and L) closed gaps in this same quarantine mechanism
  that the wording above did not yet anticipate. First, the two round-3/4
  local-abandonment paths were not the only way a request could be
  abandoned locally without a device-confirmed outcome: a connection
  reaching `Established` right as its own owning CLL disappeared (a race
  between `run_tp20_connection_request` returning and the write-back that
  records the result) left a THIRD, previously-unhandled abandonment shape
  with no local record at all -- the write-back's own `events.rs` call site
  now best-effort tears the connection down and quarantines its RX-ID the
  same way the other two paths do (Fix K). Second, the release side of the
  mechanism itself had a latent defect the wording above did not surface:
  releasing an `abandoned` entry was reachable only by first resolving the
  indication to a live owner CLL, which made release IMPOSSIBLE once that
  owner disconnected, reconnected, or was destroyed (i.e. almost always,
  for an entry that reached `abandoned` via disconnect/destroy in the first
  place) -- a self-inflicted PERMANENT quarantine, worse than the pre-round-
  7 bug this mechanism itself was built to close. Release is now checked
  independently of the entry's own owner liveness (Fix L), so "the entry is
  only actually removed once its terminal indication finally arrives" above
  is now unconditionally true, not merely true while the owner happens to
  still be live. A fourth round (round 11, Fix O) closed one more gap this
  wording did not anticipate: releasing a quarantined entry on its delayed
  terminal indication (Fix L, above) took no further action even when that
  indication said `Established` -- if the abandonment-time best-effort
  teardown had raced ahead of the device's own internal state and been
  rejected, the connection was by then genuinely active with no owning CLL
  and no further teardown attempt. Release now re-issues
  `IOCTL_TEARDOWN_CONNECTION` when (and only when) the delayed outcome is
  `Established`; a `Lost` outcome needs no follow-up, since the device
  already confirms no slot is occupied. Corrected round 15 (Fix S, Codex
  review, PR #97): Fix O's own re-issue released the quarantine BEFORE
  issuing that follow-up call, but the follow-up call is itself just as
  non-blocking (Fix R, below) — a promptly-issued new `CoptStartcomm` for
  the same `rx_id` could register before its own delayed confirmation
  drained. The entry now stays `abandoned` through an `Established`
  outcome's own processing, releasing only once the follow-up teardown's
  own terminal indication arrives.
- Two concurrent `CoptStartcomm` RPCs on the same CLL could race each
  other (round 13, Fix Q, Codex review, PR #97): `comm_started` is only
  set once `handle_start_comm` finishes ALL of its processing, well after
  a TP2.0 connection actually establishes, so both RPCs could pass
  `rpc_primitive.rs`'s own precondition and queue as two separate
  `TxItem::StartComm` entries; the second dispatch then unconditionally
  overwrote the first attempt's own just-established connection state
  before failing natively as a duplicate RX-ID. Fixed by rechecking
  `comm_started` at dispatch time, under the same lock that writes the
  fresh `Requested` phase, and rejecting the second attempt outright
  instead of touching the first's own state. The same design shape
  (`comm_started` checked only at RPC-accept time, never rechecked at
  dispatch) plausibly affects the SAE J1939 arm's own analogous "fresh
  attempt" reset the identical way — not investigated here, flagged as a
  P3 backlog entry in `j2534-0404-service/docs/implementation-notes.md`
  for a future look rather than assumed safe by omission. Corrected round
  16 (Fix T, Codex review, PR #97): the rejection branch skipped the
  `Temp` hardware revert every other async-COP failure path in this
  function already performs, leaving a racing `temp_param_update = 1`
  request's own already-applied hardware change (e.g. `CP_Loopback`)
  active on the winner's connection despite the rejection. Now reverts
  identically to those other paths.
- Every NORMAL, successful TP2.0 teardown left the torn-down `rx_id`
  unquarantined (round 14, Fix R, Codex review, PR #97): `IOCTL_
  TEARDOWN_CONNECTION` is non-blocking, and `tp20_connections` already has
  no entry left for a connection once it establishes (this ADR's own
  "resolves either way" cleanup), so `handle_stop_comm`/
  `DisconnectComLogicalLink`/`DestroyComLogicalLink` had nothing to guard
  the `rx_id` with against a promptly-issued new `CoptStartcomm` racing the
  device's own delayed teardown confirmation -- the same misattribution
  risk Fix J/K's quarantine mechanism already closes for the LOCAL-
  abandonment paths, reached here via a normal teardown instead. Fixed by
  reusing that same quarantine primitive
  (`quarantine_tp20_connection_for_orphaned_write_back`) at all three call
  sites; the helper's own visibility widened from `pub(super)` to `pub(in
  crate::service)` so `rpc_link.rs` can reach it, mirroring `cancel_
  j1939_claims_for_cll`'s identical cross-module shape. Corrected round 17
  (Fix V, Codex review, PR #97): Fix R's own quarantine call ran
  unconditionally, even when the native `IOCTL_TEARDOWN_CONNECTION` call
  itself failed synchronously — unlike the delayed-indication race Fix R
  closes, a synchronous failure means no async indication will ever arrive
  to release the quarantine later, so every future `CoptStartcomm` for that
  `rx_id` would be rejected as `RxIdInUse` until the physical channel
  closes: a permanent lock-out, the same pathology class Fix L (round 8)
  already closed for a different gap. Now tracks whether the native call
  actually succeeded and gates the quarantine-insert on that, at all three
  call sites. Investigated further, rounds 19-20 (Fix Y, Codex review, PR
  #97), with NO code fix surviving: Codex's round-19 finding reported the
  identical shape at `deliver_tp20_connection_indication`'s late-
  `Established` re-teardown branch (Fix O/S); releasing the quarantine
  there on a synchronous failure was implemented, then reverted after
  `cargo-runner` caught it regressing an existing regression test
  (`abandoned_entrys_established_outcome_stays_quarantined_until_the_
  followup_teardown_confirms`) — by the time that branch runs, `rx_id`
  already has ONE best-effort teardown call issued for it (from whichever
  of `run_tp20_connection_request`'s two internal abandonment paths
  originally quarantined the entry), so a failure there is routine
  evidence the ORIGINAL call already tore the connection down, not
  evidence no indication is coming. Separately, `handle_start_comm`'s TP2.0
  write-back arm (Fix K, round 8 — never one of the three call sites Fix R
  touched, so outside Fix V's own stated scope) turned out to share the
  identical unconditional-quarantine-insert shape Fix V fixed elsewhere;
  gating it on teardown success was implemented and briefly believed safe,
  until a round-20 Codex re-review flagged a DIFFERENT race: even though
  that call IS the first teardown attempt for `rx_id`, the device may have
  independently and spontaneously lost the connection before it ran (this
  ADR's own "no ongoing monitoring for a later spontaneous loss" residual),
  so a failure there can also mean an indication is already queued, not
  that none is coming. Reverted too. Conclusion: none of `best_effort_
  teardown_on_abandon`'s four call sites can use a synchronous teardown
  failure to decide whether quarantining is safe to skip; all four
  quarantine unconditionally (restoring pre-round-19 behavior), and the
  helper's return type reverted from `#[must_use] bool` back to `()`.
  Round 21 (Codex review finding, P2, PR #97) generalized this same
  conclusion back to Fix V's own three sites: a spontaneous, device-
  independent loss can race a deliberate `CoptStopcomm`/`Disconnect`/
  `Destroy` teardown exactly as it can race `best_effort_teardown_on_
  abandon`'s callers, so Fix V's own `teardown_issued` gate is unsafe for
  the identical reason. Reverted at all three of Fix V's sites back to Fix
  R's original unconditional insert -- every quarantine-insert call site in
  this whole mechanism (seven total, across `best_effort_teardown_on_
  abandon`'s four and these three) now uniformly quarantines
  unconditionally.
- Round 21's uniform-unconditional-quarantine conclusion exposed the
  opposite failure mode (round 22, Fix Z, Codex review finding, P1, PR
  #97; design-advisor consult): once a connection is `Established`,
  `run_tp20_connection_request`'s own post-loop cleanup already removes
  its `tp20_connections` routing entry, so a LATER, spontaneous device-
  side loss (unrelated to any local teardown) has no entry to resolve
  against and was silently dropped -- exactly the "no ongoing monitoring
  for a later spontaneous connection loss once established" residual this
  ADR already accepted. The owning CLL's own `LogicalLinkState::
  tp20_connection.phase` then stayed `Established` forever: a later
  `CoptStopcomm` believed the connection still live, issued a doomed
  native teardown, and (per round 21's own unconditional quarantine)
  inserted a quarantine entry with no confirmation left to ever release
  it -- a permanent lock-out, since the real `Lost` confirmation had
  already arrived and been discarded before that quarantine entry ever
  existed. Unlike rounds 19-21, this could not be resolved by gating a
  quarantine decision on a teardown call's own result; it needed the
  missing reconciliation itself. Fixed by giving `deliver_tp20_connection_
  indication` a last-resort arm: an unmatched `Lost` indication now scans
  every live CLL on the physical channel for one whose own
  `tp20_connection` still claims `Established` for this rx_id, and flips
  it to `Lost` directly (also best-effort stopping that CLL's own running
  repeat slots, the one consumer not already gated on `phase ==
  Established`) -- so a later `CoptStopcomm` sees a connection that's
  already gone and skips the native teardown, and the quarantine-insert,
  entirely. This narrows the "no ongoing monitoring" residual to "no
  *proactive* monitoring and no client-visible loss event" — the CLL's
  own state now passively reconciles whenever the device's own unsolicited
  indication happens to arrive, rather than never reconciling at all.
  Corrected round 23 (Codex review finding, P1, PR #97): the round-22
  scan-then-flip ran under a `logical_links`-only critical section with no
  `shared_channels` hold spanning it, leaving a window for a concurrent
  `CoptStopcomm`/`Disconnect`/`Destroy` to race in, "win" by taking the
  SAME CLL's `tp20_connection` first, and insert a fresh `abandoned` entry
  only THIS `Lost` indication could ever have released. Fixed by acquiring
  `shared_channels` first and holding it for the whole function (mirroring
  the three NORMAL-teardown sites' own outermost-lock discipline), making
  the scan-then-flip one unbroken step, and — since serialization alone
  doesn't tell either side what the other already did — releasing any
  `abandoned` entry found for `rx_id` when the scan finds no live match at
  all, the direct sign the racing teardown won and needs this indication
  to release what it inserted.
- A stale spontaneous-loss `CONNECTION_LOST` indication for an
  already-cleanly-resolved connection could misattribute onto an
  unrelated, brand-new `CoptStartcomm` reusing the same `rx_id` (found by
  a mandatory pre-merge `edge-case-hunter` pass after round 23, fixed per
  `design-advisor` consult, round 24, Fix AA, PR #97): `tp20_connections`'
  routing is keyed only by whichever entry CURRENTLY occupies `rx_id`,
  with nothing distinguishing "the current occupant" from "the specific
  request attempt an old, delayed indication actually belongs to" once an
  old occupant's entry was already cleanly removed (the ordinary
  "resolves either way" cleanup, not abandonment) and a new, unrelated
  occupant registered before the old occupant's own stale indication
  drained. Confirmed reachable: clause 19.3.3.2 means the new occupant's
  own `IOCTL_REQUEST_CONNECTION` can only succeed if the device no longer
  considers `rx_id` occupied, which is itself proof the old occupant's own
  `CONNECTION_LOST` is genuinely queued (Table 81) and will eventually
  arrive against the wrong, unrelated entry. Fixed with a registration-time
  reconcile plus a one-shot swallow flag rather than a wire-correlation
  token (Table 81 carries no per-attempt correlation ID to compare
  against) or a timed quarantine (no window anchored at the old occupant's
  own resolution time can help, since the stale frame originates at
  spontaneous-loss time, arbitrarily long afterward): `Tp20ConnEntry`
  gains `expect_stale_lost: bool`, set the instant a new registration's own
  successful native request reconciles a sibling's stale `Established`
  belief (reusing Fix Z's own scan-flip-and-stop-repeat-slots core, now
  factored into a shared `reconcile_live_established_cll` helper), and
  checked by `deliver_tp20_connection_indication` before routing a `Lost`
  outcome -- swallowing it (clearing the flag, not removing the entry)
  instead of misattributing it. Doing this at REGISTRATION time (not only
  at delivery time) also closes a sibling variant a delivery-time-only fix
  would miss: the old occupant's own `CoptStopcomm` clearing its belief
  before the stale frame ever drains, which a registration-time reconcile
  preempts by running the instant the new occupant registers, strictly
  before that `CoptStopcomm` could run. Accepted residual: this rests on
  device conformance to clause 19.3.3.2/Table 81; a nonconformant device
  degrades the new occupant to its own local 2-second connection-request
  timeout plus the existing abandon-quarantine mechanism, not a new
  failure mode.

  A follow-up Codex round asked whether the one-shot swallow can misfire
  when the new occupant's own genuine `Lost` races the stale one, since
  Table 81's payload carries no per-attempt token (round 25, PR #97).
  Accepted as irreducible at the wire's granularity but provably safe on a
  conformant device (design-advisor consult, round 25): J2534-1 clause
  7.2.5 requires a channel's messages and indications to be read back in
  the order their underlying events occurred, and clauses 19.3.3.2/19.3.3.3
  place TP2.0 connection indications in that same RX queue -- since the new
  registration can only succeed after the device has already recorded the
  prior occupant's loss (the same clause-19.3.3.2 proof above), the stale
  `Lost` always occupies an earlier queue position than any outcome the new
  request can produce, so the first `Lost` the flag swallows is the stale
  one by construction, and at most one stale `Lost` can be outstanding per
  registration (the bool needs no counter). Only a device violating
  clause 7.2.5's ordering reopens the ambiguity, and its degradation is
  bounded: a swallowed genuine `Lost` degrades the new occupant to its own
  local connection-request timeout (possibly surfacing the stale frame's
  reason byte instead of its own), and a stale `Lost` arriving after the
  new occupant establishes would flip that healthy connection to `Lost` via
  the round-22 reconcile arm (stranding the native slot inside this
  bullet's own leaked-slot residual class above) -- the same
  conformance-dependence as this bullet's existing residual, and not
  addressable by any flag redesign, since the reconcile arm never consults
  the flag. The one-shot swallow property itself (a second, later `Lost`
  for the same entry is never swallowed twice) is proven deterministically
  by `tp20_connection_indication_is_a_swallowed_stale_lost`'s own unit
  tests (`events_tp20_connection.rs`'s `#[cfg(test)]` module) -- a direct,
  flag-state-controlled test, not a timing-dependent one: an end-to-end
  `grpc_mock` attempt to race this branch against the round-22 no-match
  fallback (`reconcile_established_tp20_loss`) found the fallback wins
  systematically (client-driven gRPC registration dispatch reliably takes
  longer than the mock's own RX-poll interval), so an integration test can
  only prove the OBSERVABLE end-to-end invariant, not that this specific
  branch executed. `stale_lost_swallow_is_one_shot_and_never_eats_the_new_
  occupants_own_genuine_loss` (`tests/grpc_mock/tp20.rs`) is that
  end-to-end regression test: fills the physical channel to its four-slot
  capacity around CLL A, spontaneously loses A (queuing A's stale `Lost`
  reason `0` first), immediately re-fills the freed slot with a fourth
  filler so the channel is genuinely full again, then registers CLL B on
  A's vacated `rx_id` -- B's own native request itself now hits the
  resource-exhaustion path and queues B's own `Lost` reason `0xD8` second.
  Confirms B surfaces its own `PduErrEvtRscLocked`, never A's stale
  `PduErrEvtInitError` shape, regardless of which internal path reconciled
  A.
- A TP2.0 send never carried `TX_EXTENDED_ID` even when its established
  TX-ID needs a 29-bit CAN identifier (round 18, Fix W, Codex review, PR
  #97): `rpc_primitive.rs::apply_resolved_tx_flags` -- the shared helper
  every TX-flags call site funnels through -- derives this bit from CAN
  addressing resolution, which is always empty for TP2.0 (no
  `CP_CanPhysReqId`/`CP_CanFuncReqId` concept), and unlike SAE J1939
  (always 29-bit, forced unconditionally) TP2.0 had no dedicated branch at
  all before this fix. Round 17's Fix U (moving the mock's own TX-ID
  formula into the valid 29-bit range) made this reachable/visible for the
  first time; the gap itself predates that fix. Fixed by deriving the flag
  from the established TX-ID's own magnitude (`> 0x7FF`, the 11-bit CAN ID
  maximum) instead of forcing it unconditionally, since clause 19 permits
  either width. `rpc_misc.rs::ioctl_start_repeat_message`'s own independent
  TxFlags composition for the transmitted `RepeatMsgData[0]` message (a
  documented duplicate of `apply_resolved_tx_flags`'s logic, per its own
  PR #72 round 11 J1939 precedent) needed the identical fix. A companion
  gap in `tx_header.rs::response_header_bytes`'s TP2.0 arm (round 18, Fix
  X, Codex review, PR #97) returned `tx_flags = 0` unconditionally for the
  repeat-slot stop-condition template too, regardless of the established
  RX-ID's own width -- fixed with the same magnitude-based rule.
- Stage 7b's passive-connection client-surface design is explicitly
  deferred and unresolved by this ADR — do not treat its two ComParams'
  mere existence in the header as authorization to wire them without a
  fresh `design-advisor` round.
- Real hardware is OEM/VW-Audi-specific and unavailable to this workspace;
  correctness rests on mock fidelity and careful spec re-reading, per
  `docs/j2534-2-support-plan.md` §8's standing caveat for every
  vendor-specific phase. This stage's mock support was scoped, at initial
  implementation, to `NumOfBytes`/four-slot-capacity validation only; round
  11 (Fix N-1, Codex review, PR #97) closed the resulting gap where a
  duplicate already-established RX-ID proposal silently overwrote the
  existing slot instead of being rejected with clause 19.3.3.2's native
  `ERR_NOT_UNIQUE`, the way a real adapter rejects it. Round 17 (Fix U,
  Codex review, PR #97) closed a further mock-fidelity gap present since
  this stage's initial implementation: the mock's simulated TX-ID formula
  (`rx_id_proposal | 0x8000_0000`, bit 31 set) produced CAN IDs outside the
  valid 29-bit extended range (`0x1FFF_FFFF` max), so every established
  TX-ID this mock ever produced was structurally unrepresentable on the
  wire. Moved the marker bit to bit 28 (`rx_id_proposal | 0x1000_0000`),
  which stays within range for every possible 16-bit `rx_id_proposal`
  while preserving the same TX-ID/RX-ID structural-disjointness property
  Fix N and Fix P (below) already rely on.
- `protocol.rs`'s `ModifyTiming`-applicability table is missing rows for
  `J1708_PS`, `J1939_PS`, and `ANALOG_IN` despite its own "add a row when
  adding a constant" contract — a pre-existing gap found while adding this
  stage's own `TP2_0_PS` row, unrelated to TP2.0 itself. Backfilled in the
  same PR alongside the new `TP2_0_PS` row, since all four rows are a
  single mechanical addition to one table.
- Tests: mock-backed integration coverage for connection establish/
  teardown/reject-when-full, TX/RX framing and size-range boundaries, and
  the CLL-per-connection sharing model (two CLLs on one physical channel
  each independently establishing and tearing down).
