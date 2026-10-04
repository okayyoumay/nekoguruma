# ADR-150: UDS DiagnosticSessionControl (SID 0x10/0x50) Live Exchange for CP_ModifyTiming

**Date:** 2026-07-30
**Status:** Accepted
**Affects:** `j2534-0404-service` service, service/events, docs/GLOSSARY, docs/j2534-0404-architecture, j2534-0404-service/docs/COMPARAM_PROTOCOL_SUPPORT

## Context

ADR-146 implemented `CP_ModifyTiming`'s KWP half (ISO 14230-2 Access Timing
Parameter service, SID 0x83/0xC3, on `ISO14230` channels), explicitly
leaving `CP_ModifyTiming`'s other named mechanism — UDS DiagnosticSessionControl
(SID 0x10 request / 0x50 positive response) on CAN/`ISO15765` channels —
out of scope. `CP_SessionTiming_Ecu` (0x8015) and `CP_SessionTimingOverride`
(0x8016) were, like `CP_AccessTiming_Ecu` before ADR-146, inert
`SetComParam`/`GetComParam`-storable values with no behavior: nothing in
this service observed a DiagnosticSessionControl exchange, and nothing
derived `CP_P2Max`/`CP_P2Star` from it. ISO 22900-2's `CP_ModifyTiming`
description names both the KWP and the UDS mechanism as the same enable
gate's two protocol-specific behaviors, so bringing UDS into scope is an
extension of ADR-146's mechanism, not a new gate.

**Wire-format provenance.** ISO 22900-2 (2009 and 2022 editions, both
in-repo under `vehicle-comm-specs/`) defines `CP_SessionTiming_Ecu`'s own
D-PDU struct shape (§B.3.3.2.2, `PDU_PARAM_STRUCT_SESS_TIMING`: session,
`P2Max_high`/`P2Max_low` at 1 ms resolution, `P2Star_high`/`P2Star_low` at
10 ms resolution) but — unlike the KWP Access Timing case, where ISO 22900-2
§B.3.3.2.3 documents the full wire byte layout — does **not** specify how
those fields map onto a UDS SID 0x10/0x50 exchange's actual bytes. That
byte-level mapping is ISO 14229 (UDS) territory, and ISO 14229 is not
present anywhere in this workspace's `vehicle-comm-specs` spec repository
(confirmed by search before starting this work). Rather than guess or
implement against unverifiable general knowledge, the project owner was
asked directly and supplied the wire mapping in their own words:

> Rx: 50 [DiagSessionType] [P2 Server Max (High byte)] [P2 Server Max (Low
> byte)] [P2 Star Server Max (High byte)] [P2 Star Server Max (Low byte)]
> Resolution: P2 Server Max: 1 millisecond, P2 Star Server Max: 10
> milliseconds

This is **not** a citation of ISO 14229 text (which this workspace cannot
verify) — it is the byte *layout* (SID + subFunction echo + 4 timing
bytes) as directly supplied. The 1 ms/10 ms *resolutions* it states happen
to match ISO 22900-2 §B.3.3.2.2's own documented resolutions for the
equivalent D-PDU struct fields exactly, which is independently verifiable
in-repo and corroborates the mapping without relying on the unavailable
ISO 14229 text.

The corresponding SID 0x10 request is standard UDS DiagnosticSessionControl:
`[0x10, subFunction]`, where `subFunction`'s low 7 bits are the session
type (ISO 22900-2 §B.3.3.2.2: valid range `[1;127]`) and bit `0x80` is the
suppressPositiveResponseMessageIndicationBit — well-established, protocol-generic
UDS structure, not something requiring a clause citation.

## Decision

**Reuse ADR-146's mechanism via an enum, not a parallel pipeline.**
`TimingChangeConfig` becomes a two-variant enum:

```rust
pub(super) enum TimingChangeConfig {
    KwpAccess(AccessTimingConfig),   // ADR-146's original struct, renamed
    UdsSession(SessionTimingConfig), // new
}
```

`from_params` dispatches on the connected channel's LOGICAL/service-level
protocol (`LogicalLinkState::protocol`, not `LogicalLinkState::hw_protocol_id`
— see the Codex round-1 correction below): `protocol.kwp_access_timing_applies()`
+ `CP_ModifyTiming` enabled → `KwpAccess`; `protocol.uds_session_timing_applies()`
+ `CP_ModifyTiming` enabled → `UdsSession`; otherwise `None` (these two
`ChannelProtocol` predicates, in `protocol.rs`, are themselves the product
of the round-3 correction below — an earlier, less precise gating condition
is described in the rounds 1-2 history for context). No new enable ComParam
was introduced — `CP_ModifyTiming` itself is ISO 22900-2's own gate for
both mechanisms.

**Codex round-1 correction (protocol gating).** The original version of
this Decision gated on the exact `ChannelProtocol` identity of a raw
hardware channel id (`ChannelProtocol::from_raw(link.hw_protocol_id)`),
mirroring what was believed to be an existing `RcHandlingConfig::from_params`
precedent — that precedent turned out not to exist (`RcHandlingConfig`
gates on `is_kwp_family()`/`is_j1850_family()`, already family-based). Two
real bugs followed from the exact-identity/hardware-id approach, both
found by Codex's first review round and fixed together:

1. In `software-isotp` mode, an ISO15765-family CLL's `hw_protocol_id` is
   `CAN` (ADR-046 — the service performs its own ISO-TP segmentation over
   a raw CAN channel), never `ISO15765`, so gating on it left `timing_cfg`
   permanently `None` in that entire supported mode.
2. Even in ordinary hardware mode, `ChannelProtocol::from_raw` on a raw
   J2534 id always produces the BARE protocol value — an extended
   service-level resource-table protocol like `ISO_14229_3_ON_ISO_15765_2`
   is a distinct `ChannelProtocol` value from bare `ISO15765`, and only
   `j2534_protocol_id()` collapses them to the same underlying channel.

The fix switched every `TimingChangeConfig::from_params` call site to pass
the CLL's LOGICAL protocol instead (`link.protocol`, or — for the ordinary
`CoptSendrecv` path in `handle_send_recv`, which has no direct `link`
reference at the point it needs one — a new `TxItem::SendRecv::logical_protocol`
field threaded alongside the pre-existing `protocol_id` hardware field,
captured once at `StartComPrimitive` call time and carried unchanged
through every cyclic continuation), and widened `from_params`'s matching
to `j2534_protocol_id()`-based family matching for BOTH arms.

**A follow-up edge-case-hunter audit caught a regression the first pass of
this fix introduced**: switching the two `OneShotCommTx` call sites'
argument to `link.protocol` without ALSO widening the `ISO14230` arm broke
`KwpAccess` for any KWP CLL opened via an extended ISO14230-family resource
row (e.g. `ISO_14230_3_ON_ISO_14230_2`) — those extended values, like their
ISO15765-family counterparts, are distinct from bare `ChannelProtocol::ISO14230`
and only collapse to it via `j2534_protocol_id()`. The two arms must stay
symmetric: whichever one is left on exact-identity matching while the call
sites pass the logical protocol will silently stop firing for that
protocol family's extended resource rows. Both arms now match via
`j2534_protocol_id()` for this reason.

**Codex round-3 correction (protocol gating, again).** `j2534_protocol_id()`-based
matching turned out to be too WIDE, not too narrow — a third finding on
this exact mechanism, and the third round in a row, so this correction was
designed with a `design-advisor` consult rather than another ad-hoc patch
(this repo's own established practice: three rounds of findings on one
mechanism means stop guessing and get it designed properly). `j2534_protocol_id()`'s
entire purpose is to map an extended service-level protocol DOWN to its
underlying J2534 HARDWARE channel type — which collapses exactly the
distinction that matters here. `ISO_14230_3_ON_ISO_15765_2` (KWP2000
services over CAN transport), `SAE_J2190_ON_ISO_15765_2`, and
`ISO_15031_5_ON_ISO_15765_4` (OBD services) all share the ISO15765
hardware channel with genuine UDS variants but run a different diagnostic
SERVICES layer — a coincidentally SID-0x10/0x50-shaped exchange on one of
them is not a UDS DiagnosticSessionControl response and must not derive
`CP_P2Max`/`CP_P2Star`. The identical shape existed on the KWP side too
(`SAE_J2190_ON_ISO_14230_2`), even though Codex's finding only named the
ISO15765 cases.

The fix replaces family-based matching with two new exact-identity
predicates on `ChannelProtocol` (`protocol.rs`, sibling of the existing
`needs_j1850_autodetect` — already this codebase's precedent for "matches
`self` directly because `j2534_protocol_id()` collapses exactly the
distinction that matters"):

```rust
pub(super) fn kwp_access_timing_applies(self) -> bool {
    matches!(self,
        Self::ISO14230
        | Self::ISO_14230_3_ON_ISO_14230_2
        | Self::ISO_15031_5_ON_ISO_14230_4)
}
pub(super) fn uds_session_timing_applies(self) -> bool {
    matches!(self,
        Self::ISO15765
        | Self::ISO_15765_3_ON_ISO_15765_2
        | Self::ISO_14229_3_ON_ISO_15765_2
        | Self::ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5
        | Self::ISO_14229_3
        | Self::ISO_15765_3)
}
```

The exact match sets are derived from ISO 22900-2's `CP_ModifyTiming`
description and Table B.10's applicability columns (2022 edition — the
2009 edition's own Table B.10 row for this appears to have a column
misassociation the file's own nearby NOTE warns about; its defaults list
agrees with the 2022 table instead, which is what these sets follow), not
a pattern-matched guess:

- **KWP set**: bare `ISO14230`; `ISO_14230_3_ON_ISO_14230_2` (genuine
  KWP2000 services); and — a **deliberate inclusion, not a naive-symmetry
  exclusion** — `ISO_15031_5_ON_ISO_14230_4` (OBD/ISO 15031-5 services).
  Table B.10 marks the ISO 14230-4 applicability column too, and the
  `CP_ModifyTiming` description scopes the 0x83/0xC3 mechanism to the ISO
  14230-2 data link, which ISO 14230-4 (an application-layer profile
  riding the same data link) shares regardless of which services layer is
  running. A future "tidy up to services-layer-must-equal-14230-3" cleanup
  would wrongly remove this — a dedicated regression test pins it.
- **UDS set**: bare `ISO15765`, plus every extended protocol whose
  services layer genuinely is ISO 15765-3/ISO 14229-3.
- **Neither** (explicit exclusions, not oversights): `ISO_14230_3_ON_ISO_15765_2`
  (accepted residual — its services layer genuinely is KWP2000's own, but
  the 0x83/0xC3 mechanism's OWN scope is the ISO 14230-2 data link
  specifically, which this protocol does not run on, and ADR-146's
  derivation writes K-line-only ComParams — `CP_P3Min`/`CP_P4Min` —
  meaningless on an ISO15765 channel); `SAE_J2190_ON_ISO_15765_2`;
  `ISO_15031_5_ON_ISO_15765_4`; `SAE_J2190_ON_ISO_14230_2`; and
  `ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4` (the combined-bus K-line
  variant — which half of its combined bus is in use is only known
  post-init-probe, not at COP-creation time when this gate runs; matches
  its existing behavior, since it never fired for this protocol before
  this fix either).

An exhaustive table-driven test (`protocol.rs`, `kwp_and_uds_session_timing_applicability_matches_the_exhaustive_table`)
pins every declared `ChannelProtocol` constant's classification and
asserts the two predicates are mutually exclusive — `ChannelProtocol` is
an open newtype, so the compiler cannot force exhaustiveness the way a
closed `enum` would; this table is the defense against a future constant
silently inheriting the wrong bucket, with a comment directing whoever
adds a new constant to add a row here too.

**Noted, not fixed, as a candidate for a future look** (design-advisor's
own flag during this consult): `RcHandlingConfig`'s RC21/23/78 handling
gates on `is_kwp_family()`/`is_j1850_family()` — the identical
hardware-vs-services-layer question this correction just resolved for
`TimingChangeConfig` could arguably apply there too for the SAE
J2190/OBD-family protocols. Out of scope for this PR; recorded here so a
future round doesn't rediscover it from scratch.

**Close-out edge-case-hunter finding, same PR: the round-3 widening left
`CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`/`CP_ExtendedTiming`'s own
`SetComParam`/`GetComParam` allow-list gate (`comparam_support.rs`'s
`is_iso14230_only_structfield_param` arm) on the OLD exact-identity
`protocol == ChannelProtocol::ISO14230` check.** This predates round 3
(the pre-ADR-150 mechanism used the same exact-identity check on both
sides, so they were consistent until this correction widened one side
only) but was only surfaced now, by a pre-merge audit specifically run
because round 3 changed the underlying mechanism again. Effect: the
live-exchange mechanism silently activated for `ISO_14230_3_ON_ISO_14230_2`/
`ISO_15031_5_ON_ISO_14230_4` (per `kwp_access_timing_applies()`), but a
client on either protocol could never `SetComParam(CP_AccessTimingOverride,
...)` to redirect it, nor `GetComParam(CP_AccessTiming_Ecu)` to observe
what it silently recorded — both rejected `PDU_ERR_COMPARAM_NOT_SUPPORTED`,
a misleading signal since the mechanism genuinely applies there. Fixed by
having `is_iso14230_only_structfield_param`'s gate call
`kwp_access_timing_applies()` directly instead of a second, separately
hardcoded exact-identity check — the two can no longer drift apart the way
this finding shows they already had.

`PendingTimingChange.ecu_entry` becomes `Option<EcuTimingRecord>`
(`AccessTiming { timing_set, bytes }` / `SessionTiming { session,
p2_max_ms, p2_star_10ms }`), and `CopRegistrant::timing_accumulator`
becomes `Option<TimingAccumulator>` (`Kwp([u8; 5])` / `Session { p2_ms,
p2_star_10ms }`) — the same fields, generalized rather than duplicated.
Everything downstream of pairing is reused unmodified or with one added
branch: `observe_registrant_timing_change` (all four `bind_registrant`
call sites — the continuation fast-path absorb arm, the full-match absorb
arm, the open-new-buffer arm, and the plain non-concat match arm),
`select_latest_timing_changes`, the `timing_delta`/`timing_pending`
fold/push/store pipeline, `WorkingTimingSnapshot`'s fold-time
concurrent-write guard, and the `PendingTimingFrame` queue-reservation/
`timing_frame_flag_ok` machinery. This inherits, rather than re-litigates,
the seven Codex review rounds ADR-146 already spent closing races in this
exact shared code.

**Pairing and derivation.** A qualifying request (`[0x10, subFunction]`)
captures `session = subFunction & 0x7F` (masking off the suppress bit;
`session == 0` is rejected as `None`, per the `[1;127]` valid range). A
later `[0x50, session, ...]` response echoes that captured session type;
mismatched or short (< 6 bytes) responses are documented no-ops, bounds-checked
via `.get()`, never a panic or a direct index. `session_timing_to_comparams`
computes:

```rust
CP_P2Max  = p2_max_ms   * 1_000  + CP_CanTransmissionTime   // saturating
CP_P2Star = p2_star_10ms * 10_000 + CP_CanTransmissionTime   // saturating, SAME formula as CP_P2Max
```

`CP_P2Star` uses the identical full-addition formula as `CP_P2Max` — the
project owner explicitly chose this over an initially-proposed 0.5×
`CP_CanTransmissionTime` factor (which had been *inferred* from a spec
inequality plus a default-value cross-check, not independently verifiable
given ISO 14229's absence from this workspace). Both additions use
`saturating_add`, not plain `+` — `CP_CanTransmissionTime` is an
unvalidated client-supplied `SetComParam` value with no range check
anywhere in `rpc_set_com_param` (edge-case-hunter finding: a
near-`u32::MAX` value plus a genuine response panicked the whole channel's
poll task under plain `+`, which nothing restarts per ADR-146's own
accepted residual on `spawn_channel_poll_task`'s discarded `JoinHandle`).

**Functional/physical addressing.** Both `CP_P2Max` and `CP_P2Star` are
client-side timeout ceilings with no P2Min-analog direction (unlike KWP's
mixed min/max fold across `P2Min`/`P2Max`/`P3Min`/`P3Max`/`P4Min`), so a
functionally-addressed exchange's multiple responses fold toward the
maximum for both fields. Physical addressing uses the same same-registrant
supersession tracking ADR-146 already established (a later response from
the same registrant supersedes an earlier one this pass; no accumulation).
Cross-registrant (two independent SID 0x10/0x50 exchanges on one CLL in one
pass) and cross-CLL (physical channel sharing) behavior reuse ADR-146's
existing arrival-order and worst-case rules unmodified.

**Override redirect.** `CP_SessionTimingOverride`, when it has an entry for
the echoed session type, redirects which values the derived `CP_P2Max`/
`CP_P2Star` are computed from — mirroring `CP_AccessTimingOverride`'s
established redirect-derivation-only semantics. `CP_SessionTiming_Ecu`'s
recorded entry always reflects the observed/combined value, never the
override; an override present but missing the echoed session falls back to
the observed value for both derivation and recording. `CP_SessionTimingOverride`
itself is read-only from this mechanism's perspective — it is never
written, only consulted.

**UUDT-companion-channel interaction (new relative to ADR-146).** Unlike
`ISO14230` (no UUDT companion channel scenario, ADR-046), an `ISO15765`
CLL can have a second, concurrently-running poll task serving a UUDT
companion channel. Two changes close the resulting races:

1. A UUDT-routed frame is definitionally not the unicast/USDT traffic a
   DiagnosticSessionControl response is, so pairing is skipped entirely for
   any frame the companion channel routed — threaded via `ConcatFrameMeta`
   (reused rather than adding a new positional parameter to every
   `bind_registrant` call site, which would have required touching ~70
   existing test call sites for an unrelated struct).
2. `merge_registrant_writeback`'s `timing_accumulator` field previously
   overwrote unconditionally, justified by "only ever `Some` on ISO14230,
   which has no UUDT-companion scenario" — a premise this ADR falsifies.
   It now guards the overwrite on the pass's own snapshot differing from
   its baseline (`RegistrantBaseline`), the same delta pattern
   `matches_got`/`pending_rc` already use in that function, closing the
   race where a stale companion-task pass (which never observed a
   qualifying response, so its cloned snapshot is unchanged) could
   otherwise clobber a concurrently-running primary-task pass's fresher
   write for the same registrant.

**This closes the reachable race, not just a partial case.** With UUDT-routed
frames excluded from pairing (point 1), the companion poll task is
structurally walled off from ever producing a "changed since baseline"
`timing_accumulator` snapshot — only a CLL's own primary-channel poll task
can. A "two passes that both differ from baseline, in different
directions" scenario would require two concurrent *primary*-kind writers
for the same registrant, which the one-primary-task-per-physical-channel
design never produces. The baseline-diff guard therefore fully closes this
field's reachable concurrency surface today — this is stated positively
here (edge-case-hunter audit, confirmed by tracing every `RxEntryKind`
delivery path) so a future change to either the UUDT-gating logic or the
poll-task topology does not silently reopen it without anyone noticing the
dependency this invariant rests on.

**`CP_EnableConcatenation` (ADR-148) interaction.** `concat_enabled` is
gated to the KWP/J1850 protocol family only (ISO 22900-2 Table B.11), so a
`CP_EnableConcatenation=1` + `CP_ModifyTiming=1` configuration on an
`ISO15765` CLL can never actually produce `concat_enabled: true` — the
four-call-site `observe_registrant_timing_change` placement is therefore
defensive-but-currently-inert for the UDS variant specifically (a test
pins this, forcing `concat_enabled` to exercise the otherwise-unreachable
path). This mirrors ADR-146's own real interaction bug with ADR-148 found
during the two ADRs' merge (a KWP registrant with both `timing_cfg` and
`concat_enabled` set) — the shared placement exists so the UDS variant is
protected the same way if `CP_EnableConcatenation`'s protocol scope ever
widens, not because today's configuration space can reach it.

## Consequences

- Extends ADR-146's state machine and RxFlag scheme (`ECU_TIMING_CHANGE`)
  to a second protocol family via an enum, rather than introducing a
  second mechanism — inherits that ADR's seven rounds of concurrency
  fixes instead of re-deriving them.
- Accepted residual: the SID 0x10/0x50 wire byte layout (not the D-PDU
  struct's own resolutions, which ARE independently verified against ISO
  22900-2 §B.3.3.2.2) rests on a mapping the project owner supplied
  directly, since ISO 14229 is absent from this workspace's spec
  repository. If ISO 14229 is added to `vehicle-comm-specs` in the future,
  this mapping should be re-verified against it and this note updated or
  removed.
- Accepted residual: a short (< 6-byte) SID 0x50 response is a documented
  no-op — no ComParam changes, no error surfaced. This can occur for a
  pre-2013-style UDS stack or a non-conformant ECU; the client's own
  `CoptSendrecv` result already reflects whatever the ECU actually sent.
- No hardware `SET_CONFIG` mapping exists for `CP_P2Max`/`CP_P2Star`
  (service-level parameters, like `CP_P2Star`'s existing RC78-reload
  consumer) — the shared `apply_params_to_hardware` push this mechanism
  reuses from ADR-146 is a vacuous success for this derived pair.
  `WorkingTimingSnapshot`'s guard is still required regardless, since the
  fold/push/store pipeline's await window (and thus the concurrent-`SetComParam`
  race it protects against) exists independent of whether the push itself
  does anything.
- `CP_SessionTiming_Ecu` stores raw wire-resolution values (ms / 10 ms
  units), mirroring `CP_AccessTiming_Ecu`'s "record wire, derive
  separately" split — not the derived µs-resolution ComParam values.
- The `merge_registrant_writeback` `timing_accumulator` guard's soundness
  depends on the UUDT-routed pairing exclusion (Decision, point 1) — see
  that section's closing paragraph for the explicit dependency this rests
  on.
- Codex round-1 also found `comparam_support::validate_structfield_shape`'s
  `SessionTiming` arm validated only the oneof shape, not field VALUES —
  `session`/`p2_max`/`p2_star` are `u32` proto fields narrowed to `u16` by
  `session_timing_structfield_entries`/`store_session_timing_ecu_entry`
  with no range check, so a client-supplied `session=65539` would silently
  alias session 3's entry and `p2_max=65536` would truncate to 0. Fixed by
  adding a per-field `> u16::MAX` rejection, mirroring the `AccessTiming`
  arm's existing `> u8::MAX` wire-range check a few lines above it.
  **Round 2** tightened `session`'s own check further, to `1..=127` rather
  than the full `u16` range: `SessionTimingConfig::with_request` only ever
  captures `1..=127` (the request subFunction's low 7 bits, `& 0x7F`-masked,
  zero filtered out — ISO 22900-2 §B.3.3.2.2's documented valid range), so a
  client-supplied `session` outside that range (in-range for the round-1
  `u16` check but never producible by `with_request`) could never match any
  captured request — silently dead input, not merely a wraparound risk.
  `p2_max`/`p2_star` keep the wider `u16` range (no equivalent narrower
  documented range exists for either field).
- Accepted test-coverage residual: the `handle_send_recv`/`TxItem::SendRecv::logical_protocol`
  threading fix (the third `TimingChangeConfig::from_params` call site,
  covering the ordinary `CoptSendrecv` path — the primary way a client
  actually drives this mechanism) is verified by the Rust compiler's
  struct-literal/destructuring exhaustiveness (any missed construction or
  match site is a hard compile error) and by the `from_params` protocol-matching
  unit tests, but has no dedicated `tests/grpc_mock/` integration test
  exercising `handle_send_recv` itself end-to-end through a real poll-task
  dispatch. No such integration-test scaffolding exists yet for either this
  mechanism or ADR-146's KWP half; building it is a reasonable future
  addition but was judged disproportionate to this specific fix, whose
  change at the call site itself is a direct one-line substitution with no
  new logic of its own to exercise beyond what the unit tests already
  cover.
