# ADR-165: SAE J2534-2 Repeat Messaging — Phase 12

**Date:** 2026-08-07
**Status:** Accepted (Context's `Condition`-semantics paraphrase, Decision 6, and the rounds-7/8/17 condition-0 carve-outs superseded by ADR-173; Decision 2's `DataItem.IORepeatMessageSetup` proto variant superseded by ADR-178 — re-expressed as a hand-packed byte payload inside the pre-existing `bytearray_data` carrier; Decision 3's per-protocol response-header convention gains a J1939 arm per ADR-179 Decision 9, and its repeat-slot lifecycle gains a J1939-specific stop-on-address-relinquishment amendment per ADR-180 Decision 10; round-4/5 `FD_CAN_PS` repeat-message padding and round-6 TX-size-range mechanism superseded by ADR-186; Decision 3's response-header/mask composition mechanism is scoped to RawMode=OFF CLLs by ADR-199 — a RawMode=ON CLL composes no service-derived header at all; every other Decision item remains in force)
**Affects:** `j2534-0404-service/src/service.rs`, `service/names.rs`, `service/service_params.rs`, `service/rpc_misc.rs`, `service/rpc_link.rs`, `j2534-0404-mock`, `vci-service-interface/src/proto/service.proto`, `docs/j2534-2-support-plan.md`

## Context

`docs/j2534-2-support-plan.md`'s Phase 12 targets SAE J2534-2 clause 14
(Repeat Messaging): a client hands the interface a `REPEAT_MSG_SETUP`
(interval, stop `Condition`, up to 3 `PASSTHRU_MSG` slots) via
`IOCTL_START_REPEAT_MESSAGE`, and the interface autonomously retransmits
`RepeatMsgData[0]` at that interval until a stop condition is met, per
`Condition`: 0 keeps transmitting indefinitely regardless of RX; 1 stops on
a matching response, and *also* stops if the interval elapses with no
matching response seen. `IOCTL_QUERY_REPEAT_MESSAGE`/`_STOP_REPEAT_MESSAGE`
let a client poll or cancel a running slot by the `MsgId` the interface
assigned at `START` time. The device supports at minimum 10 concurrent
slots per channel, and evaluates each slot's mask/pattern against incoming
frames *before* the channel's Pass/Block message filters apply — a byte
sequence shorter than the pattern never matches; bytes beyond the pattern's
own length are don't-care.

The plan's up-front assessment flagged this phase for a `design-advisor`
consult given the codebase's only existing "autonomous periodic TX"
mechanism (`TesterPresentState`/`dispatch_due_tester_present`, ADR-083) is
itself documented as fragile and per-CLL, not per-channel — the initial
working assumption was that Repeat Messaging would need to extend or
parallel that mechanism, plus add a mask/pattern evaluation step to the RX
poll path (`poll_rx_inner`) ahead of Pass/Block filtering.

That assumption does not survive a reading of clause 14 itself. The
autonomous retransmission, its interval timing, and the mask/pattern
evaluation are explicitly assigned to the *interface* (the vendor's
pass-thru DLL/hardware) — not the API caller. This service is a J2534 API
*client* of that DLL, in exactly the position `ioctl_sw_can_hs` already
occupies for ADR-164's Single Wire CAN commands: a thin IOCTL forwarder,
not a reimplementer of device-internal behavior. A software reimplementation
is not merely more work than necessary here, it is provably wrong against
clause 14's own ordering rule: this service installs Pass/Block filters
*natively* (`ioctl_start_msg_filter` calls into the vendor DLL directly),
so by the time a frame reaches this service's poll loop, native filtering
has already discarded anything not covered by an installed PASS filter.
J2534's default-deny model means a repeat slot's stop-response frequently
has no covering client filter (the client only asked to watch for the
repeat's own stop condition, not to pass that traffic through). A
service-side mask/pattern check in `poll_rx_inner` would simply never see
those frames, and the repeat would never stop. A tight `TimeInterval` (the
spec allows millisecond-scale intervals) is a second, independent failure
mode against this service's ~10ms poll cadence. Only the device, sitting
below native filtering and running its own timer, can satisfy clause 14's
ordering and timing requirements — so this service's job is to forward the
setup faithfully, not execute it.

## Decision

**1. Native forwarding via three new `L`-scoped ADR-079 IOCTL commands.**
`IOCTL_START_REPEAT_MESSAGE`/`_QUERY_REPEAT_MESSAGE`/`_STOP_REPEAT_MESSAGE`
are `ChannelID`-scoped in the native API, matching the existing `L`
(ComLogicalLink-level) convention `SW_CAN_HS`/`SW_CAN_NS` (ADR-164) and 6
other commands already use via `require_cll_handle_for_ioctl`. They become
`PDU_IOCTL_BASE + 0x14`/`+ 0x15`/`+ 0x16` (the next unused offsets after
ADR-164's `+0x12`/`+0x13`), with `map_ioctl_name` entries and handling in
`resolve_object_id`'s `ObjtIoCtrl` arm. Gated on the module's J2534-2
opt-in (`"J2534-2:"` `pname` prefix, ADR-152 Decision 1) — no protocol-family
gating is needed since clause 14 applies channel-wide, not per-protocol.
`START`/`QUERY`/`STOP` resolve `cll_handle → channel_id →
PassThruIoctl(IOCTL_*_REPEAT_MESSAGE)`, the same one-hop resolution as
every other CLL-scoped IOCTL. No new RPC — ADR-152 Decision 2's mechanism
(ride the existing `IoCtl` RPC) extended three more scope-letter entries.

**2. `DataItem.IORepeatMessageSetup` proto variant carries the structured
input/output; `unum32_value` (already tag 1) covers `START`'s returned
`MsgId` and `QUERY`'s returned status.** New oneof variant at `DataItem`
tag 11 in `vci-service-interface/src/proto/service.proto`, mirroring the
existing `IOProgVoltage`/`IOEntityAddress` structured-IOCTL-input shape:
carries `TimeInterval`, `Condition`, and the repeat message's D-PDU-style
payload bytes plus target-address fields (mirroring `transmit_request`'s
own D-PDU-to-native TX composition inputs — see Decision 3). This is a
routine `vendored-protoc` regeneration (`vci-service-interface/docs/implementation-notes.md`),
not the CLAUDE.md-gated `*-sys` FFI bindgen path — `j2534_v0404.h` already
carries every native constant and struct this phase needs
(`IOCTL_START/QUERY/STOP_REPEAT_MESSAGE` at `j2534_v0404.h:2768-2770`,
`REPEAT_MSG_SETUP`/`PASSTHRU_MSG` at `j2534_v0404.h:41-45,17-25`) — no
`-sys` header edit, so no bindgen decision arises this phase.

**3. The service composes the native `RepeatMsgData[0]` frame from the
CLL's own TX addressing, and the mask/pattern stays payload-scoped at the
D-PDU surface.** D-PDU clients speak payload-only bytes (ADR-051's header
stripping); the device's mask/pattern evaluates raw wire frames. The
service builds `RepeatMsgData[0]`'s header/ID bytes the same way
`transmit_request` already does for an ordinary `CopSendrecv`/COP TX on
this CLL, and — for the mask/pattern the client supplies — prepends the
CLL's configured expected-response ID bytes to the pattern, with an
exact-match (`0xFF`) mask over most header-byte positions and a don't-care
(`0x00`) mask over any position whose real value this service cannot
predict (Codex review PR #42 round 2 Finding B: a KWP/ISO14230 response's
wire-level length byte is payload-length-dependent, not a static ECU
identity byte, so it cannot be masked exact-match like the rest of the
header). This scopes a repeat slot to the addressed ECU's own responses
(mirroring ADR-006/ADR-051's existing payload-scoped expected-response
convention) rather than exposing a raw full-frame mask/pattern surface that
would break the payload-only abstraction every other D-PDU surface in this
service maintains.
`ExpectedResponse::matches()` itself is not reused code (it runs
service-side against COP completions; Repeat Messaging's matching runs
device-side), but its payload-only convention is the contract this
composition step preserves.

**4. MsgId ownership tracked per-CLL, not per-channel; teardown mirrors the
`client_filters` pattern.** The device assigns `MsgId` at `START` time (no
service-side allocation); `LogicalLinkState` gains a `Vec<u32>` of live
MsgIds for the owning CLL, populated on a successful `START` and drained on
`STOP`/CLL teardown — placed alongside `client_filters` (`service.rs:3156`)
since both are "hardware state this CLL owns, cleaned up the same way."
`QUERY`/`STOP` validate the caller-supplied `MsgId` against this CLL's own
tracked set before forwarding to the device — the device has no notion of
CLL identity on a shared physical channel, so a client attempting to
`STOP` a sibling CLL's repeat slot is rejected service-side (mapped to the
baseline v04.04 `ERR_INVALID_MSG_ID`; `ERR_EXCEEDED_LIMIT` on `START`
similarly needs no new error-code mapping — both are pre-existing v04.04
codes). Teardown: best-effort `STOP` for every tracked slot in
`DestroyComLogicalLink`'s existing client-filter-cleanup loop
(`rpc_link.rs:1894-2033` precedent) and on `DisconnectComLogicalLink` before
`release_shared_channel_ref` — J2534-1 §7.2.4's disconnect-stops-all-periodic-
messages requirement (which clause 14 extends to repeat messages) is the
backstop once the physical channel's `ref_count` reaches 0, so no dedicated
new physical-channel-teardown hook is needed beyond the existing
CLL-scoped cleanup loop.

**5. `poll_rx_inner`/`dispatch_due_tester_present` are untouched.** No
software timer, no RX-path mask/pattern evaluation step, no new shared
per-channel autonomous-TX abstraction. Repeat responses flow through the
existing poll/filter/COP-attribution path as ordinary unsolicited RX,
exactly as any other device-originated frame would.

**6. The mock (`j2534-0404-mock`) implements the real clause-14 state
machine**, since it stands in for the device this design forwards to:
per-channel slot table (≥10 capacity), interval timer, `Condition=0`
(unconditional continued transmission) vs `Condition=1` (stop on matching
RX, *or* stop on interval expiry with no matching RX — the two are
independent stop triggers under `Condition=1`, not "continue on no
match" as an earlier draft of this phase's brief assumed), mask/pattern
evaluation with the short-frame-never-matches and beyond-pattern-length
don't-care rules, and `MsgId` assignment/lookup for `QUERY`/`STOP`.

## Consequences

- **Slot budget is shared across sibling CLLs on one physical channel** —
  the device's ≥10-slot minimum is per-channel, not per-CLL, so
  `ERR_EXCEEDED_LIMIT` can surface to whichever client's `START` call
  crosses the shared limit first. Accepted residual; document in
  `j2534-0404-service/docs/implementation-notes.md`'s backlog if this proves
  surprising in practice.
- **Repeat slots do not survive their owning CLL's disconnect/destroy** —
  a client that expects a repeat slot to persist across a `Disconnect`/
  reconnect cycle on the same physical channel will find it stopped. This
  matches J2534-1 §7.2.4's own disconnect semantics (not a design choice
  unique to this phase) and is the correct behavior, but is called out
  explicitly since it diverges from how a channel-lifetime resource might
  naively be expected to behave.
- **Device-autonomous repeat TX does not stamp `last_bus_activity`**
  (`events.rs:2843`) — only the resulting RX response does. A mode-1
  idle-triggered tester-present could in principle fire mid-repeat-sequence
  on a quiet bus; bounded by the fact that any matching response does stamp
  activity. Accepted residual, not fixed this phase — no existing mechanism
  reaches into device-internal TX timing to stamp this, and doing so would
  require the very software-timer machinery this ADR's Decision explicitly
  avoids.
- **Real-DLL support is optional** — a v04.04-era DLL predating clause 14
  returns an error for these three IOCTLs; this service forwards and maps
  that failure rather than emulating the feature in software, per this
  ADR's core Decision. The Phase 1 discovery cache
  (`discovery_protocol_info`) is not consulted as a pre-check this phase
  (still uncalled from any enforcement point, same as every phase since
  Phase 1 shipped) — deferred alongside that existing gap, not a new one.
- **Software-ISO-TP links cannot support Repeat Messaging at all** (Codex
  review, PR #42 round 4) — `START_REPEAT_MESSAGE` is rejected outright on a
  CLL connected in `can_channel_mode = "software-isotp"` (ADR-046). This is
  not a deferred feature gap: clause 14's device-autonomous retransmission
  has no way to drive the per-retransmission ISO-TP segmentation/flow-control
  that mode requires (this service, not the device, does that driving for
  every ordinary COP in that mode), the same fundamental incompatibility
  already established for FD-connected links + software-ISO-TP at connect
  time. Unlike that case, the rejection lives at `START_REPEAT_MESSAGE` time,
  not connect time, since software-ISO-TP itself remains fully supported for
  every other feature on such a link.
- **UART protocol exclusion** (native Echo Byte handling excludes Repeat
  Messaging) is irrelevant until a UART-family phase lands; no enforcement
  needed yet, but a future UART phase must add the exclusion check this
  phase does not.
- **Interplay with loopback/ADR-099's TX-discard machinery and pending-COP
  expected-response matching** on a repeat response is a verification
  concern for implementation, not a design question this ADR resolves
  differently from the existing poll-path behavior — flagged for the
  mandatory `edge-case-hunter` pass rather than pre-emptively redesigned
  here, since the existing poll/filter/attribution path is intentionally
  left untouched (Decision 5).
- **Post-review corrections (Codex review, PR #42 round 2):** a failed
  best-effort `STOP` during CLL teardown was found unsafe to just-log
  whenever the physical channel stays open for a sibling CLL — unlike
  `client_filters` (ADR-082's sole-ownership guarantee), a repeat slot's
  shared-across-sibling-CLLs budget (above) means no imminent
  `PassThruDisconnect` is guaranteed to clean it up. Fixed by adding
  `SharedChannel::leaked_repeat_message_ids`, retried opportunistically the
  next time any CLL touches Repeat Messaging on that channel, with a
  backstop drop (not retry) right before the channel's own `SharedChannel`
  entry is removed on `ref_count` reaching 0. Separately, a `Condition == 1`
  slot that self-completes device-side (no notification back to this
  service) is now pruned from `LogicalLinkState::repeat_message_ids` the
  moment a `QUERY`/`STOP` on it reports `ERR_INVALID_MSG_ID`, not only on a
  successful `STOP` — closing an unbounded-growth gap on a long-lived CLL.
- **Post-review corrections (Codex review, PR #42 round 6):** three
  independent review rounds (2, 3, 6) each found a consumer trusting
  `repeat_message_ids` membership after a slot self-completed device-side
  with no notification — a deliberate consequence of the thin-forwarder
  design (Decision 1/5). Membership in `repeat_message_ids` is a claim,
  not a fact — the device is the sole source of truth. A consumer that
  already receives an in-band device verdict on the exact MsgId it is
  acting on (`QUERY`/`STOP`'s own prune above, `START`'s stale-claim
  revocation on MsgId reuse) reconciles from that verdict directly, no
  extra call needed. A consumer with no such in-band verdict must call the
  new `prune_stale_repeat_message_ids` helper (`rpc_misc.rs`) first before
  trusting the Vec — currently only `rpc_lock_resource`'s
  `LOCK_PHYSICAL_TX_QUEUE` check, which also stopped exempting the
  requester's own claim from this scan (Fix J's `same_physical_resource`
  reading of the governing active-transmissions clause applies identically
  here: it carries no "other" qualifier). Consequences: bounded
  (≤ `MAX_REPEAT_SLOTS_PER_CHANNEL` = 10) synchronous native probes inside
  the lock-grant critical section per sibling; a probe error other than
  `ERR_INVALID_MSG_ID` is treated as inconclusive and fails closed (blocks
  the grant) rather than risking a wrong grant during autonomous device TX;
  if a real device reports a completed slot via a QUERY status code rather
  than `ERR_INVALID_MSG_ID`, this would over-block until verified against
  the SAE J2534-2 clause 14 QUERY output semantics in the sibling
  `vehicle-comm-specs` repo — accepted residual, not yet checked against
  the spec text.
- **Post-review corrections (Codex review, PR #42 round 7):** two
  independent findings. First, `START`'s response-header resolution
  (`tx_header::response_header_bytes`, Decision 3) was unconditional even
  though clause 14's `Condition == 0` (unconditional/free-running
  retransmission) never has the device evaluate the mask/pattern at all —
  only `Condition == 1` (stop-on-match-or-timeout) does. An otherwise valid
  `Condition == 0` link (e.g. raw CAN with only `CP_CanPhysReqId` set and no
  paired response-id ComParam, or functional addressing) was rejected for a
  header the device would never consult. Fixed by resolving the header only
  when `condition != 0`, falling back to the same empty-header/zero-flags
  shape the helper itself already returns for protocols with no header
  concept when `condition == 0`; `Condition == 1` is unaffected and still
  requires a resolvable header. Second, `IORepeatMessageSetup` had no way
  for a client to request pass-through TX flags (e.g. ISO-TP frame padding)
  on the transmitted `RepeatMsgData[0]` message, unlike an ordinary
  `CoptSendrecv` TX's `ComPrimitiveCtrlData.tx_flag`. Fixed by adding a
  `tx_flag_bits` field (reusing the existing `TxFlagBit` enum) that is
  folded into `RepeatMsgData[0]`'s composed `tx_flags` only — never applied
  to the mask/pattern messages, consistent with round 2's Finding 2
  precedent that those are comparison templates, not transmitted frames.
- **Post-review addendum (Codex review, PR #42 round 11):** round 6's
  `prune_stale_repeat_message_ids`-before-rejecting fix covered
  `LogicalLinkState::repeat_message_ids` (a live CLL's own claim) but missed
  the sibling data source `SharedChannel::leaked_repeat_message_ids` (round
  2's own fix, above) entirely — a `MsgId` moved there when a disconnecting
  CLL's best-effort `STOP` fails has no owning `LogicalLinkState` left for
  `rpc_lock_resource`'s `links`-only scan to see at all. Fixed by adding an
  identical probe-and-prune scan over `shared_channels` (matched by physical
  resource, alongside the pre-existing `executing_cop` check) before
  rejecting a `LOCK_PHYSICAL_TX_QUEUE` grant on a non-empty leaked-id set.
  Not independently black-box testable end-to-end for the "genuinely still
  live" rejection case, for the same reason round 2/3's own leaked-id unit
  tests (`rpc_misc.rs`) already document: the mock's only native `STOP`
  failure mode is `ERR_INVALID_MSG_ID`, which always means the slot is
  already gone — covered instead by two white-box unit tests alongside
  those existing tests.
- **Post-review correction (Codex review, PR #42 round 14):** the mock's
  `Condition == 1` response-format check (added round 9, refined round 10)
  compared the wrong `PASSTHRU_MSG` field — an incoming frame's `TxFlags`,
  which the mock always zeroes for RX-direction messages, so the check never
  actually fired as intended. The governing rule going forward: a
  received frame's wire-format facts (CAN ID width, ISO15765 addressing
  type) live in `RxStatus`, not `TxFlags` — `TxFlags` is TX-direction only,
  application-filled for a write, and the two fields' bit values merely
  happen to coincide (`0x100`/`0x80`) rather than being interchangeable.
  A REPEAT_MSG_SETUP's `TxFlags`-sourced expectation must be translated into
  `RxStatus` space at the boundary (once, at slot-creation time) rather than
  compared against an incoming frame's `TxFlags` directly. Separately,
  nothing previously stopped a loopback echo of the CLL's own transmission,
  or another indication frame (TX-done, start-of-message, break), from
  satisfying a slot's stop condition if its `RxStatus`/`ProtocolID`/`Data`
  happened to agree with the slot's criteria — not a "received message" per
  clause 14's own scope, so an eligibility check now excludes these
  unconditionally before any mask/pattern comparison runs. Also removes the
  round-9/13 `TxFlags`-threading mechanism (the mock's `__mock_inject_rx_
  msg_with_flags` export, its harness test-helper counterparts) as dead
  weight now that the comparison reads the field the mock already
  populates correctly.
- **Post-review correction (Codex review, PR #42 round 15, design-advisor
  consult):** round 15's finding claimed an FD-connected slot's mask/pattern
  couldn't distinguish classic-vs-FD responses and asked to compare FD
  format bits in the matcher — verified against SAE J2534-2 and found partly
  wrong. Clauses 21.2.2(g)/22.2.2(d) are normative rules that FD-capable-
  channel filtering/matching ignore CAN message format entirely (matching is
  on address+data only, regardless of classic-vs-FD wire encoding), so
  comparing FD format bits in the matcher would itself violate the spec. The
  closed-form completeness criterion this pass derives for the repeat-slot
  response comparison: a wire-format bit belongs in the comparison IFF it
  changes the identity of the address or data, per 21.2.2(g)/22.2.2(d)'s
  "filtered on the address" language. CAN-ID width and ISO15765 addressing
  type qualify (two frames with identical Data bytes but different ID width
  or addressing are genuinely different messages, round 9/14's own basis).
  FD format, BRS, and ESI do NOT qualify — they only change the wire
  encoding of the SAME address+data, and 21.2.2(g)/22.2.2(d) explicitly
  require ignoring them for FD-capable-channel filtering. The actual, narrower
  gap: 21.4.4 lets a device cap a `PASSTHRU_MSG`'s `DataSize` at 12 bytes
  when `TX_FD_CAN_FORMAT` is unset, so `TX_FD_CAN_FORMAT` is nonetheless SET
  on repeat-slot mask/pattern templates on an FD-connected link (`rpc_misc.
  rs`) — not for comparison, but for template DataSize validity.
  `TX_FD_CAN_BRS` stays unset on those templates: Table 93 defines BRS as a
  bit-timing property within an FD frame, not a format discriminator, with
  no equivalent validity coupling. Separately, `j2534-0404-mock`'s TX→RX
  translation helper (`tx_format_flags_to_rx_status_bits`) now also
  translates the FD format/BRS bits, for RX-echo fidelity (e.g. a loopback
  echo of an FD transmission honestly reporting FD `RxStatus` bits) — but
  the mock masks its `RepeatSlot::response_format_rx_bits` at slot-creation
  time to `REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` (which excludes the FD
  bits), specifically so that translation-helper growth (this fix, or any
  future addition) can never leak an FD bit into the comparison and silently
  break matching on an FD-connected link.
- **SAE J1939 (ADR-179/180, developed independently, many rounds later) needed
  two amendments to this Decision's own mechanisms, found by Codex review PR
  #72 round 9.** Decision 3's per-protocol response-header convention had no
  J1939 arm at all (the generic empty-header fallback silently misaligned
  every J1939 repeat slot's stop-condition template against the wire frame's
  5-byte prefix) — resolved by ADR-179 Decision 9, which exact-matches only
  the responding ECU's own source address and wildcards every other header
  byte (a J1939-specific instance of this Decision's own "wildcard what's
  unknowable" convention, not a departure from it). Separately, this
  Decision's repeat-slot lifecycle had no mechanism for a live slot to be
  stopped when the CLL that started it under a since-relinquished address —
  a J1939-specific concern with no analogue in any protocol this Decision
  was originally designed against, since no other protocol this codebase
  supports has a device-negotiated, revocable source address — loses that
  address; resolved by ADR-180 Decision 10.
