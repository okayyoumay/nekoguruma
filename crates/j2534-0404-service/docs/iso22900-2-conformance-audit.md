# ISO 22900-2 Conformance Audit — j2534-0404 Subsystem

## Document Control

- Status: Active. This line makes **no aggregate claim about how much of Part A
  is resolved** — see each Part A finding's own heading below for its current
  disposition, and see "How to use this document"'s authoritative-record
  convention for why that heading, not any summary sentence here, is the thing
  to trust. A free-text "X% resolved"/"fully resolved"/"all resolved" claim
  in this spot was tried and repeatedly found stale by Codex review across
  four separate wordings and directions — no substring or synonym check can
  validate arbitrary natural-language paraphrase of a claim like that, so the
  claim itself was removed rather than chased through a fifth phrasing (see
  ADR-163). Document Control's resolved-items list below is a checked,
  mechanically-verified derived index of the Part A headings and Part B rows
  (`scripts/ci-checks.sh audit-doc-check`) — not an aggregate summary sentence,
  a per-ID roster. Part B's already-decided-deviation triage is separate,
  ongoing work, tracked below. Resolved items are listed below,
  **one item per line, sorted by finding ID**. When
  marking an item resolved, insert it at its sorted position as its OWN line and do
  not touch neighboring lines — parallel fix PRs used to conflict constantly on the
  previous single shared status sentence, and only collide under this format when
  two concurrently-fixed IDs happen to sort adjacently (in which case the resolution
  is a mechanical union, keeping both lines in ID order):
  - A1-1 fixed
  - A1-2 fixed
  - A1-3 fixed
  - A1-4 fixed
  - A1-5 reclassified (Part B/B21)
  - A2-1 fixed
  - A2-2 reclassified (Part B/B26)
  - A2-3 fixed
  - A2-4 reclassified (Part B/B22)
  - A2-5 fixed
  - A2-6 fixed
  - A2-7 fixed
  - A2-8 fixed
  - A2-9 reclassified (Part B/B11)
  - A2-10 fixed
  - A2-11 reclassified
  - A2-12 fixed
  - A2-13 fixed
  - A2-14 fixed
  - A2-15 fixed
  - A2-16 fixed
  - A2-17 fixed
  - A2-18 fixed
  - A2-19 fixed
  - A2-20 reclassified (Part B/B23)
  - A2-21 fixed
  - A2-22 reclassified (Part B/B24)
  - A2-23 fixed
  - A2-24 fixed
  - A2-25 fixed
  - A2-26 fixed
  - A2-27 reclassified (Part B/B20)
  - A2-28 fixed
  - A3-1 fixed
  - A3-2 fixed
  - A3-3 verified
  - A3-4 fixed
  - A3-5 fixed
  - A3-6 fixed
  - B4 fixed
  - B11 partially fixed
  - B21 fixed
  - B23 fixed
  - B26 fixed
- Owner: j2534-0404-service maintainers
- Created: 2026-07-20
- Update trigger: Whenever a finding below is fixed, superseded by a new ADR,
  or re-verified against a newer ISO 22900-2 edition.
- Purpose: This document is the durable record of a full-subsystem spec-vs-implementation
  audit conducted in a single session. It is written so a **different session/agent** can
  pick up any item below without re-deriving it — re-running the audit from scratch is
  wasteful; re-reading this file is not.

## Scope and Methodology

Target: the J2534 v04.04 subsystem (`j2534-0404-sys`, `j2534-0404`, `j2534-0404-service`,
`j2534-0404-mock`, `j2534-0404-registry`), audited against the normative text of
**ISO 22900-2:2009(E)**, converted to markdown in the sibling `vehicle-comm-specs`
repository at `iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md` (not in this
repository — `vehicle-comm-specs` must be checked out alongside this repository).

**Caveat — spec version.** `docs/j2534-0404-architecture.md` states the implementation's
target is **ISO 22900-2:2022**, but only the **2009** edition text was available for this
audit. Findings below that hinge on a specific clause are annotated "verify against 2022"
where a 2009→2022 revision could plausibly change the verdict; most findings (constants,
Annex D tables, core RPC semantics) are stable across editions and are not so annotated.

**Method, in two passes:**

1. **Six parallel design-advisor audits**, each comparing a cohesive subsystem area's
   implementation directly against the relevant ISO 22900-2:2009 sections, producing the
   **Part A** findings below (concrete defects — not yet decided anywhere, not covered by
   an ADR).
   - ComParam system (Get/SetComParam, Working/Active split, class/lock semantics, defaults, UniqueRespIdTable)
   - Resource/Object ID model (GetResourceStatus/Ids/ConflictingResources, GetObjectId, Lock/UnlockResource, CreateComLogicalLink)
   - IOCTL command set (PDUIoCtl, the 17 `PDU_IOCTL_*` commands + 4 legacy raw IDs)
   - Error/status model (GetStatus, the ADR-105 rich-error-model rework — the most recently changed area at audit time)
   - ComPrimitive execution & response binding (StartComPrimitive/CancelComPrimitive, PDU_COP_CTRL_DATA cycle semantics, the two-tier response-binding registry)
   - TX/RX message construction (TxFlag/RxFlag, addressing, header/footer framing, ISO-TP, P3 timing)
2. **A full sweep of all ~90 in-scope ADRs** (`docs/adr/ADR-001` through `ADR-105`,
   excluding `Configuration & Deployment`/`J2534 v05.00 Subsystem`/`vci-service-manager`
   themes, which are out of scope for ISO 22900-2 conformance), classifying each as a
   genuine, acknowledged spec deviation vs. a conformance fix vs. an internal-only change.
   This produced **Part B** below — deviations that already have a decision-of-record, so
   they need triage (accept as-is / tighten / re-open), not rediscovery.

Every Part A finding below was independently verified by the auditing agent against the
actual ISO 22900-2:2009 text (section + line number cited); it is not a suspicion list.

## How to use this document

- **Fixing a Part A item**: read the cited spec section yourself before changing code — the
  scenario/severity here is a summary, not a substitute. After fixing, update the
  affected documents in the same change, since several of these touch `docs/rpc-api-guide.md`/`j2534-0404-architecture.md`/per-package
  `implementation-notes.md`.
- **Triaging a Part B item**: these already have an ADR. The only decision needed is
  whether the existing decision should stand (most should — they are J2534 v04.04 hardware
  capability limits, not oversights), be tightened, or be promoted to a full fix. Do **not**
  re-implement one without first reading its ADR in full — the "why" is usually load-bearing.
- **Priority tags** (`P1`/`P2`/`P3`) follow this repository's existing convention (see the
  Prioritized Backlog): P1 = real interop
  risk / silent data loss / silent protocol violation; P2 = real but narrower-blast-radius
  defect or a currently-inert capability gap; P3 = cosmetic, pedantic, or requires the 2022
  spec text to confirm.
- Findings are **not** deduplicated against currently-open PRs — check `git log`/open
  branches before starting work on an item in case it has already been addressed since this
  document was written.
- **Authoritative record, checked index — not "one place states status."** Each
  finding's own record is the authoritative source for its disposition: for a
  Part A finding, that's its heading plus body; for a Part B item, that's its
  table row (the `ADR`/`Suggested triage` cells). Document Control's
  resolved-items list is a **derived index** of those records, not a second
  independent source — the same duplication-with-enforcement pattern
  `docs/adr/INDEX.md` already uses against the ADR files themselves, backstopped
  here by `scripts/ci-checks.sh audit-doc-check` (ADR-163) instead of prose
  discipline alone. **Every roster line carries only its ID and disposition
  token, plus a `(Part B/BXX)` target for a reclassified item — nothing
  else, ever, for any disposition** (not an ADR reference, not a
  verification date, not a "combined with"/"see also" note): that detail
  already lives at the finding's own record, and the roster is not allowed
  to hold a second, uncompared copy of it. This was tightened from an
  earlier version that let FIXED/VERIFIED roster lines keep their ADR
  reference (`A1-1 fixed (ADR-106)`) as a "convenience" mirror of the
  heading — `audit-doc-check` never actually compared that reference
  against the heading's own, so it could drift silently (Codex proved this
  by changing a roster ADR reference to a nonexistent one and getting a
  clean pass); rather than keep widening the comparison every time another
  scrap of unchecked detail turns up, the roster carries no such detail at
  all. Every other location that references overall
  status — the Status line, the Part A heading note, "Recommended next steps",
  this package's `implementation-notes.md` — states no
  per-finding disposition, only points at Document Control (or at a specific
  finding). **A cross-reference from one finding's record to another finding's
  record is a bare pointer, never a restatement** — "see B21" or "see A2-10",
  never that target's disposition, ADR, or status — since that duplicates the
  target's own authoritative record and can silently go stale (Part B item
  B26's triage cell once claimed finding A2-24 "remains a separate, still-open
  gap" for that exact reason, after A2-24 had already been fixed via ADR-118;
  see ADR-163). This document went through five Codex review rounds across
  PRs #38-#40 before landing here: each round found the same underlying defect
  — status asserted in more than one place — in a new location, because a
  literal "only one place may state disposition" invariant can never hold for
  a document whose primary content (each finding's own record) inherently
  states disposition. See ADR-163 for the full history and the
  `audit-doc-check` mechanism.

---

## Part A — Unresolved implementation defects (no ADR covers these)

**The heading name below is kept for continuity** with this document's original
audit-pass distinction (Part A: undecided defects found during the audit, vs. Part B:
already-ADR-decided deviations needing triage only) — not necessarily because anything
below is still open. See Document Control's resolved-items list above for current status;
not restated here.

### A.1 — P1 (high interop risk)

**A1-1. FIXED (ADR-106): `GetConflictingResources` doesn't implement §9.4.26's actual semantics.**
- Fixed by replacing the live-CLL-based scan with a static `RESOURCE_TABLE` pin-overlap
  scan (`rpc_link.rs::rpc_get_conflicting_resources`, `resources::rows_conflict`), matching
  §9.4.26's static resource-table query semantics and Annex G.1.4's worked example without
  misreporting spec-legal channel sharing as a conflict — see ADR-106 for the full decision.
- File: `j2534-0404-service/src/service/rpc_link.rs:704-797`
- Spec: §9.4.26.1 (line 2717, 2725) — a static resource-table query ("same pin/same
  controller"), callable *before* any CLL exists (Annex G.1.4, lines 7949-7961).
- Current behavior: reports only *currently-connected* links sharing the same protocol.
  Never reports pin/controller conflicts among unconnected resources; conversely reports
  spec-legal channel *sharing* as if it were a conflict.
- Fix shape: `resources.rs::ResourceDef` already carries typed `dlc_pins`; compute static
  pin-overlap conflicts from the table, independent of connection state.
- Doc-sync: `docs/rpc-api-guide.md`, possibly a new ADR (design decision, not a one-liner).

**A1-2. FIXED: `GetObjectId(OBJT_BUSTYPE)`/`GetObjectId(OBJT_PROTOCOL)` don't resolve most of the resource table's canonical names — breaks the `GetObjectId → GetResourceIds` discovery round trip.**
- Fixed by extending ADR-069's `OBJT_RESOURCE` table-first resolution to the `OBJT_PROTOCOL`
  arm (a new `find_protocol_for_name` helper — deliberately not `find_table_row_by_name`,
  since that helper's `hw_protocol_override` ambiguity check is wrong for a protocol-identity
  query; a PR #115 Codex review caught this regressing `"SAE_J2610_on_SAE_J2610_SCI"`) and the
  `OBJT_BUSTYPE` arm (via a new `find_bustype_id_by_name` helper — a plain case-insensitive
  `bus_type_name` lookup with no ambiguity handling, since `bus_type_name` is 1:1 with
  `bus_type_id` for every row by construction, unlike `protocol_name`) — all fall back to the
  legacy alias maps unchanged when no table row matches. See
  `j2534-0404-service/src/service/names.rs::resolve_object_id`.
- File: `j2534-0404-service/src/service/names.rs:1097-1117`
- Spec: §9.4.23-25, Annex B.1.
- Concrete failures: `"ISO_11898_2_DWCAN"` → wrong/partial match; `"SAE_J2610_UART"` →
  zero rows; `"ISO_9141_2_UART"`, `"ISO_14230_1_UART"`, `"SAE_J1850"`, and several others →
  `PDU_ERR_INVALID_PARAMETERS` even though `resources.rs` defines them as legitimate
  `bus_type_name`/`protocol_name` values.
- Fix shape: try a `resources.rs` table-name match first (mirroring the fix ADR-069 already
  applied to `OBJT_RESOURCE`), before falling back to `map_bustype_name`/`map_protocol_name`.

**A1-3. FIXED (ADR-110): `CoptUpdateparam`'s lock-conflict handling contradicted §9.4.16 d)'s event-based error model.**
- Fixed by removing all three synchronous `ResourceExhausted`/`PduErrRscLockedByOtherCll`
  rejections this gap actually spanned (`CoptUpdateparam`, `temp_param_update`
  `CoptSendrecv`/`CoptStartcomm`, and `SetComParam` itself — Table 25 has no lock-related
  return code for `SetComParam` at all) and replacing them with a live, per-param resolution
  inside `handle_update_param` (`events.rs`): a `PDU_PC_BUSTYPE`-class ComParam another CLL's
  `LOCK_PHYSICAL_COM_PARAMS` protects is unconditionally excluded from the hardware push
  while locked (never applied, regardless of whether it "looks" unchanged to this CLL's own
  bookkeeping), the CLL's own local promotion falls back to its pre-call Active value for
  that key, exactly one `PDU_ERR_EVT_RSC_LOCKED` event fires when this CLL's own Working
  genuinely differs from its own Active on a locked key, and the COP always finishes normally
  (`PDU_COPST_FINISHED`) — see ADR-110 (supersedes ADR-044; amends ADR-067 §C) for the full
  decision, including why the fix needed three independently-computed pieces
  (`hw_set`/`promote_set`/`rsc_locked`) rather than one shared exclusion check.
- **Process note for future readers**: the first implementation of this fix (correct per the
  spec text and the design review at the time) shipped a real regression — it used "this
  CLL's own Working vs. its own Active" as a proxy for "does this write conflict with the
  real, currently-locked hardware state." Since `LogicalLinkState.active` is per-CLL with no
  cross-CLL sync, a non-owning CLL with stale-but-self-consistent bookkeeping could silently
  clobber another CLL's lock-protected physical hardware value with zero error signaling —
  caught by an `edge-case-hunter` pass with a live repro, not by the initial design review or
  the full test suite. The corrected design (ADR-110) never lets a `PDU_PC_BUSTYPE`-class
  value from a locked-out CLL reach hardware at all, decoupling hardware safety from any
  single CLL's own bookkeeping. Worth re-reading before trusting a similar "compare my own
  Working to my own Active" pattern anywhere else in this codebase's lock-conflict logic.
- File (pre-fix): `j2534-0404-service/src/service/rpc_primitive.rs:851-870`, `rpc_link.rs:2616-2641`
- Spec: §9.4.16 d) (line 2133) — on a lock conflict, the spec expects the COP to be
  created, non-physical params applied, and a `PDU_COPST_FINISHED` event emitted (with a
  `PDU_ERR_EVT_RSC_LOCKED` error event) — not outright rejection.
- Prior behavior: synchronous `ResourceExhausted`/`PduErrRscLockedByOtherCll` rejection;
  nothing is enqueued or applied.
- Scenario: CLL A holds `LOCK_PHYSICAL_COM_PARAMS`; CLL B stages params and calls
  `CoptUpdateparam` — a conformant client expects a COP handle and a FINISHED event; gets a
  synchronous error instead.
- Note: ADR-043/044/045 document *where* this lock is enforced thoroughly but never engage
  with §9.4.16 d)'s actual event model — this is a genuine gap, not a documented tradeoff.

**A1-4. FIXED (ADR-111): `CoptStartcomm`'s optional request message is silently dropped for non-init protocols (CAN/J1850).**
- Fixed by reusing `CoptStopcomm`'s existing one-shot transmit(+receive) machinery
  (`StopCommTx` renamed `OneShotCommTx`, ADR-085/ADR-087): a non-empty `cop_data` on a
  non-K-line link is now resolved eagerly at `StartComPrimitive` call time (against
  `binding.resolved()`, so `temp_param_update` is honored) into a `TxItem::StartComm::tx`,
  transmitted by `handle_start_comm`, and — when `NumReceiveCycles != 0` — awaited via the
  same `wait_for_expected_response` engine `CoptSendrecv`/`CoptStopcomm` use, with matched
  responses delivered as `ResultData` attributed to the COP's own `cop_handle`. Unlike
  `CoptStopcomm`'s non-cancellable receive phase, this one is fully `cancellable: true`
  (nothing has committed CLL state yet at this point). An RX timeout is non-fatal — the CLL
  still reaches `PDU_CLLST_COMM_STARTED` (ISO 22900-2 §9.2.6.3.2 b)'s state-change sentence is
  unconditional); a genuine transmit failure is fatal (no `COMM_STARTED`, and no
  `PduErrEvtInitError` — that event is K-line-init-specific). `NumReceiveCycles == -1`
  (IS-CYCLIC) is rejected synchronously, mirroring `CoptStopcomm`'s identical rejection. See
  ADR-111 for the full decision.
- File: `j2534-0404-service/src/service/events.rs` (`handle_start_comm`'s new
  `else if let Some(tx) = tx` branch), `src/service/rpc_primitive.rs` (`rpc_start_com_primitive`'s
  `CoptStartcomm` arm), `src/service.rs` (`OneShotCommTx`, `TxItem::StartComm::tx`).
- Spec: §9.2.6.3.2 b) (line 682-684) — CAN/J1850 `CoptStartcomm` may carry an optional
  request message; if `NumReceiveCycles != 0` the API waits for a response.
- Prior behavior: `!cop_data.is_empty() && !protocol_requires_init(protocol_id)` logged and
  discarded `cop_data` — nothing was transmitted. `TxItem::StartComm` did not even carry
  `expected_response`/`num_receive_cycles` for this path.
- Test coverage: `j2534-0404-service/tests/grpc_mock/startcomm_optional_message_tx.rs`.

**A1-5. Moved to Part B — see B21.** ADR-079 item 13 explicitly reasons about
`CLEAR_MSG_FILTER`'s "logged, best effort... so the client can retry" design, and
`rpc_misc.rs`'s `STOP_MSG_FILTER` implementation (`:1386-1410`) uses the identical
tracked-for-retry code shape and log wording as `CLEAR_MSG_FILTER` (`:1476-1510`) — the
same deliberate design, even though ADR-079 item 12's bullet doesn't spell out the same
reasoning for `STOP_MSG_FILTER` explicitly. Listing this as an undecided Part A defect
contradicted Part A's own definition. See B21 for its current disposition.

### A.2 — P2 (real but narrower blast radius)

**A2-1. FIXED (ADR-112): Async error events never carried `cop_handle` attribution.**
- Fixed by giving `TrackedError` an `Option<CopRef { cll_handle, cop_handle }>` field
  (`j2534-0404-service/src/service.rs`) so the `GetEventItem` drain, the `SubscribeEvent`
  live-notification path, and the RPC-fallback `error_event_data_for` snapshot all derive
  cop_handle attribution from one source of truth. `send_error_event` (`events.rs`) gained a
  `cop_handle: Option<u32>` parameter, threaded `Some(cop_handle)` at every call site raised
  from inside a specific COP's own execution and `None` at the two genuinely module/CLL-scoped
  sites (the lost-comm-to-VCI hard-error broadcast, and `PDU_IOCTL_CLEAR_PERIODIC_MSGS`'s
  channel-wide administrative error). `send_tester_present_once`/`frame_tester_present_data`
  — discovered mid-implementation to be shared between a COP-driven arm/re-arm path and a
  COP-less periodic-dispatch tick — each gained their own `cop_handle: Option<u32>` parameter
  so both call chains supply the value correct for their own context, rather than one answer
  hardcoded for a function with two different callers. See ADR-112 for the full decision,
  including the correction to ADR-022's now-superseded claim that `send_error_event` always
  passes `None`.
- Files: `j2534-0404-service/src/error.rs:102-110`, `src/service.rs` (`TrackedError`,
  `CopRef`), `src/service/events.rs` (`send_error_event` and its 14 call sites),
  `src/service/rpc_primitive.rs` (`rpc_get_event_item`'s `CllQueueItem::Error` drain),
  `src/service/rpc_misc.rs` (`PDU_IOCTL_CLEAR_PERIODIC_MSGS`).
- Spec: §9.4.7 c) (line 1467) / §9.6.2 (line 3749) — an error associated with a specific
  ComPrimitive must carry that ComPrimitive's handle.
- Scenario (now fixed, regression-tested): two concurrent `CoptSendrecv` COPs on one CLL; one
  hits an N_Bs-style receive timeout — the client's error event now carries the failing COP's
  own handle, verified against the still-live sibling COP never receiving it
  (`tests/grpc_mock/cop_ctrl_cycles.rs::sendrecv_error_event_carries_the_failing_cops_own_handle_not_a_concurrent_sibling`).
- Note: this was the freshest area (ADR-105 rework) and was under-covered by that ADR's own
  scope — see ADR-105's own P2 backlog item in `implementation-notes.md` for a related,
  narrower, still-open gap (CLL/COP *status* events, not error events).

**A2-2. Moved to Part B — see B26.** ADR-021 explicitly and deliberately defines a
queued-but-not-yet-started COP as `PDU_COPST_WAITING` (its own state table: "In the queue,
not yet started"), with a full state machine and Consequences section built on that
decision. Even though this audit's own analysis is that ADR-021's D.1.4 citation
misreads the spec, an ADR-decided, reasoned choice is Part B material, not an
undecided Part A gap — contradicting Part A's own "no ADR covers these" definition to
list it here. See B26 for its current disposition.

**A2-3. FIXED (ADR-127): `GetResourceStatus` now surfaces lock state, and counts a
created-but-unconnected CLL as "in use."**
- Fixed by computing, under `rpc_get_resource_status`'s existing `logical_links` lock guard
  (no second acquisition), each resolved candidate's own `hw_protocol_id` (derived exactly the
  way `CreateComLogicalLink` derives it) and matching any link in `logical_links` against that
  set — regardless of `link.connected` — for both the "in use" bit and the lock bits. Bit 0
  (Usage Status) is now true for any created CLL, matching (not `active_candidate`'s
  connected-only match, which is unchanged and still serves only the `resource_id` echo
  tie-break). Bits 2/3 (TX-queue/ComParam lock) are the OR of `held_lock_mask`
  (ADR-123's `LockResource`/`UnlockResource` state) across every matching link. Matching by
  `hw_protocol_id` rather than `ChannelProtocol` (`active_candidate`'s own comparison) is
  deliberate — see ADR-127 — so a software-ISO-TP raw-CAN sibling correctly surfaces on its
  ISO15765 sibling's query, and vice versa (ADR-046).
- Files: `j2534-0404-service/src/service/rpc_link.rs` (`rpc_get_resource_status`).
- Spec: Table D.1 (lines 6352-6359) bits 2/3 = TX-queue/ComParam lock; §9.4.9.2 c) (line
  1574) marks a resource "in use" at *creation*, not connection.
- Test coverage: `j2534-0404-service/tests/grpc_mock/resources.rs::get_resource_status_reports_in_use_for_a_created_but_unconnected_cll`/`get_resource_status_surfaces_a_siblings_held_lock_bits`.
- Accepted residual (unchanged, carried from ADR-123): the §9.4.13.3 use case 4 / §9.4.14.2 c)
  `PDU_IT_INFO` lock-status-change push callback remains unimplemented — a client must still
  poll `GetResourceStatus` to observe a lock change.

**A2-4. Moved to Part B — see B22.** ADR-069's own text explicitly documents
`GetResourceStatus`/`GetConflictingResources` using "the same two-step resolution
`CreateComLogicalLink` uses" — table lookup first, falling back to the legacy
`ChannelProtocol::from_raw` interpretation for backward compatibility — and its
Consequences section confirms `CreateComLogicalLink`'s legacy/extended fallback is
deliberate, not an oversight. Listing all three call sites as an undecided Part A gap
contradicted Part A's own definition. Carried forward to B22 below.

**A2-5. FIXED (ADR-126): `SET_EVENT_QUEUE_PROPERTIES` has no pre-Connect guard; can shrink an in-use RX buffer live.**
- Fixed by adding a guard in `ioctl_set_event_queue_properties` that checks
  `link.pdu_connect_begun()` (`link.connected`, OR a `ConnectComLogicalLink` call already
  in flight for this CLL — round 2, see below) before mutating
  `event_queue_cap`/`event_queue_mode`, returning `PDU_ERR_CLL_CONNECTED`
  (`Code::FailedPrecondition`) via `state_guard_status` — matching the crate's established
  connect-state guard pattern (e.g. `rpc_primitive.rs:670-676`'s identical error for a
  different guard). The pre-existing immediate-trim-on-lower-cap logic is retained unchanged:
  it remains reachable via a disconnect-then-reconfigure-then-reconnect flow, since
  `DisconnectComLogicalLink` clears `connected`/`channel_id` but not `rx_buf`. Round 2 (Codex
  review) added `LogicalLinkState::connect_in_flight: Weak<()>`, claimed atomically at the top
  of `rpc_connect_com_logical_link` before any `.await` and held as a function-scoped local for
  the RPC's lifetime, to close the window between `ConnectComLogicalLink` being called and
  `finalize_connected_link` publishing `connected = true` — during which the guard would
  otherwise still see `connected == false`. See ADR-126 for the full decision, including why a
  `Weak` token was chosen over a `bool` (cancellation-safety) and the alternatives considered.
- Files: `j2534-0404-service/src/service/rpc_misc.rs:1653-1739` (`ioctl_set_event_queue_properties`,
  guard at line 1715); `j2534-0404-service/src/service/rpc_link.rs:1493-1528`
  (`rpc_connect_com_logical_link`'s `connect_in_flight` token claim);
  `j2534-0404-service/src/service.rs:1744,1916-1925` (`LogicalLinkState::connect_in_flight`,
  `pdu_connect_begun`).
- Spec: §9.5.16 (line 3620) — usable only before `PDUConnect`; connected → `PDU_ERR_CLL_CONNECTED`.
- Test coverage: `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs::set_event_queue_properties_rejects_when_connected`
  (new regression test for the guard itself);
  `set_event_queue_properties_trims_rx_buf_immediately_when_lowering_the_cap` (restructured around
  a disconnect-then-shrink flow, confirming the immediate-trim logic remains reachable);
  `subscribe_only_limited_mode_delivers_every_frame_live_with_no_loss`,
  `no_subscriber_limited_mode_drops_per_push_cll_event_semantics`,
  `backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`,
  `backlog_discarded_then_subscribe_emits_exactly_one_lost_then_drains_fifo` (each restructured to
  configure the event queue before `Connect` instead of on an already-connected CLL);
  `rpc_link.rs::tests::pdu_connect_begun_reflects_the_connect_in_flight_token` (round 2, a
  synchronous unit test for `pdu_connect_begun()`'s toggle logic — the full concurrency race is
  verified by code-structure argument only, see ADR-126's round-2 Consequences).

**A2-6. FIXED (ADR-115): `PDU_EVT_DATA_LOST` is never generated.**
- Fixed by having `push_cll_event` (`events.rs`) return a `PushOutcome` enum
  (`Inserted`/`Evicted`/`Discarded`) instead of a bare `bool`, and having all three call
  sites (`poll_rx_inner`'s frame fan-out, `send_error_event`, `handle_start_comm`'s fast-init
  synthetic frame) send exactly one `EventNotificationData::Lost(LostEventItemNotification{})`
  to the CLL's `SubscribeEvent` subscriber, if any, whenever a drop occurs — `DiscardNewest`
  sends `Lost` *instead of* the discarded item's own notification (never both, avoiding a
  self-contradictory "lost, then delivered" sequence caught by an `edge-case-hunter` pass on
  the first implementation attempt); `OverwriteOldest` sends `Lost` *plus* the newly-inserted
  item's own notification. `GetEventItem`/`last_error`/`TrackedError` are deliberately
  untouched — the shared proto's `EventItem.data` has no `Lost` variant, matching D.1.8's rule that
  no event data goes into the event queue — so a poll-only (no `SubscribeEvent`) client still
  gets no signal, by design, not as an accepted gap. See ADR-115 for the full decision,
  including why `iso22900-service`'s existing `EventNotificationData::Lost` precedent doesn't
  transplant directly (its live notification drains the same native queue that overflows;
  `j2534-0404-service`'s `rx_buf` and its live `SubscribeEvent` tap are two independent
  delivery paths for one incoming item).
- Files: `j2534-0404-service/src/service/events.rs` (`push_cll_event`, `PushOutcome`,
  `make_lost_notification`, and its three call sites).
- Spec: §9.5.16 (line 3634) — a queue-full state must generate `PDU_EVT_DATA_LOST`.
- Test coverage: `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs::queue_overflow_emits_one_lost_notification_per_drop_discard_newest`/`_overwrite_oldest`.
- Known residual (out of scope, tracked in the backlog): `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s immediate-trim-on-cap-lowering path bypasses
  `push_cll_event` entirely and silently discards buffered items with no `Lost` signal at
  all — a different failure mode (synchronous, client-commanded resize) than the
  buffer-overrun case ADR-115 scopes `Lost` to.

**A2-7. FIXED (ADR-129): `START_MSG_FILTER`/`STOP_MSG_FILTER`/`CLEAR_MSG_FILTER` now accept pre-connect configuration instead of rejecting an unconnected CLL.**
- Fixed by adding `LogicalLinkState::pending_client_filters` (`service.rs`): when
  `channel_id` is `None`, `ioctl_start_msg_filter` (`rpc_misc.rs`) validates the request
  (ISO15765/type/shape/duplicate checks, all connection-state-agnostic) and stores it there
  instead of rejecting with `CLL_NOT_CONNECTED`; `ioctl_stop_msg_filter`/`ioctl_clear_msg_filter`
  remove from/clear the same map pre-connect, with no native calls. `rpc_connect_com_logical_link`
  (`rpc_link.rs`) installs any pending filters onto a newly-created channel via the new
  `install_client_message_filters` helper (shared with the connected path so the two can't drift),
  or fails the connect outright with `PDU_ERR_FCT_FAILED` if honoring them would mean joining an
  already-shared channel (extends ADR-082's sole-channel-ownership invariant symmetrically).
  `finalize_connected_link` now takes its caller's `shared_channels` guard by reference and folds
  the installed ids into `client_filters` atomically with `channel_id` becoming visible, closing a
  join-guard race the naive version of this design would have reopened.
- Files: `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/rpc_misc.rs`,
  `j2534-0404-service/src/service/rpc_link.rs`.
- Spec: §9.5.13 (line 3503) — filters may be configured before Connect and become active
  once Online; Table 54 (3532-3541) lists no `CLL_NOT_CONNECTED` return. §9.4.11.2 d) (line 1702)
  explicitly names all three IOCTLs as usable before `PDUConnect`.
- Accepted residual (tracked in the backlog): filter definitions still do
  not survive `PDUDisconnect` (only `PDUDestroyComLogicalLink`, per §9.5.13's own text, "should"
  delete them) — a reconnect requires re-issuing `START_MSG_FILTER`.

**A2-8. FIXED: Module-scoped IOCTLs (READ_VBATT etc.) auto-open the device instead of rejecting when the module isn't connected.**
- Fixed by adding `J2534Service::require_connected_device_for` (`service.rs`), a sibling of the
  pre-existing `ensure_open_device_for`/`lock_device_for`: it rejects with the same
  `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition` as those two on a module-handle mismatch, but
  additionally rejects with `PDU_ERR_MODULE_NOT_CONNECTED` (`module_not_connected_status`)
  rather than opening a device or proceeding as a no-op when nothing is open at all.
  `ioctl_read_vbatt`/`ioctl_set_prog_voltage`/`ioctl_read_prog_voltage` (`rpc_misc.rs`) now call
  this instead of `ensure_open_device_for` (which used to silently open/connect the device on the
  caller's behalf); `ioctl_reset` now calls it instead of `lock_device_for` (which used to
  silently proceed as a no-op reset when nothing was open). The `GENERIC`/`GET_CABLE_ID`/
  `READ_IGNITION_SENSE_STATE` module-scoped IOCTLs — unsupported by this adapter regardless, and
  previously rejecting with `Status::unimplemented` with no connection check at all — now also
  call `require_connected_device_for` first, so a pre-connect caller sees
  `PDU_ERR_MODULE_NOT_CONNECTED` rather than the usual unsupported-command error (completeness
  gap found by Codex review, PR #143: NOTE 1 applies to every `PDUIoCtl` call, not just the ones
  this adapter happens to implement). `ModuleConnect`/`GetVersion`/`CreateComLogicalLink`'s own
  lazy-open behavior (ADR-107) is unchanged — this fix only closes the connection-check gap, not
  the module-selection design ADR-107 already settled.
- Files: `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/rpc_misc.rs`.
- Spec: ISO 22900-2:2009(E) §9.4.29.2 NOTE 1 (line 3026) — every D-PDU API function taking a
  `hMod` parameter other than `GetResourceIds`/`GetObjectId`/`GetConflictingResources`/
  `GetStatus` must reject with `PDU_ERR_MODULE_NOT_CONNECTED` while the module isn't in
  `PDU_MODST_READY`; `PDUIoCtl` is not in that allow-list. Table 12 (line 1360) — `PDUIoCtl`'s own
  return-value table lists `PDU_ERR_MODULE_NOT_CONNECTED`.
- Ordering fix (Codex review, PR #143): `ioctl_set_prog_voltage` used to parse `input_data`
  (`Status::invalid_argument` on missing/malformed `PDU_IT_IO_PROG_VOLTAGE`) before calling
  `require_connected_device_for`, so a disconnected module with bad input got `InvalidArgument`
  instead of `PDU_ERR_MODULE_NOT_CONNECTED` — inconsistent with the other 6 module-scoped IOCTLs,
  where the connection check always runs first. Fixed by moving the `require_connected_device_for`
  call before payload parsing.
- Module-status fix (Codex review, PR #143, P1): `require_connected_device_for` originally treated
  an open `device_id` alone as proof of "connected," but `events::handle_channel_hard_error` sets
  `module_state.status` to `PduModstNotAvail` on a lost-comm event WITHOUT closing `device_id` — so
  a module whose comms had died, but whose stale `device_id` slot was still `Some`, incorrectly
  passed the connection gate. Fixed by additionally requiring `module_state.status ==
  PduModstReady` (ISO 22900-2 §9.4.29.2.1 use case (a): only `PDU_MODST_READY` allows API function
  calls to the module) — `device_id` open is necessary but not sufficient.
- Recovery-path fix, corrected twice (Codex review, PR #143, P1, then a second P1 on the same
  mechanism — see **ADR-131** for the full account): the module-status fix above introduced a new
  stuck state of its own — `ensure_open_device_for`'s already-open no-op branch (taken by a repeat
  `ModuleConnect` for the same handle) never itself resets `module_state`, so a module a hard error
  had marked `PduModstNotAvail` stayed stuck rejecting every module-scoped IOCTL even after a
  client called `ModuleConnect` again and received a success response. A first attempt fixed this
  by unconditionally resetting `module_state` to `ModuleState::default()` after every successful
  `ensure_open_device_for` call — but a second Codex review round correctly found this was itself
  wrong: the already-open branch performs no native call at all, so declaring the module `READY`
  again was a blind assertion with nothing behind it. **ADR-131** resolved this by reading the spec
  more carefully: ISO 22900-2 §9.4.29.2 Behaviour (d) (the step that moves the module status to
  PDU_MODST_READY) only applies after Behaviour (a)-(c) succeed, and the spec's own state machine defines no
  `NOT_AVAIL -> READY` transition via `ModuleConnect` at all — NOTE 2 and §9.4.30 prescribe the
  only recovery sequence, `ModuleDisconnect` then `ModuleConnect` again. `rpc_module_connect` now
  rejects with `PDU_ERR_FCT_FAILED` (carrying the tracked `PDU_ERR_EVT_LOST_COMM_TO_VCI`) when the
  already-open branch finds `module_state.status != PduModstReady`, rather than either staying
  stuck or reporting a false success; `module_state` is written back to `Ready` in exactly one
  place, `ensure_open_device_inner`'s fresh-`PassThruOpen` branch.
- Regression tests:
  `j2534-0404-service/tests/grpc_mock/pdu_ioctl.rs::module_scoped_ioctls_reject_when_module_not_connected`,
  `::module_scoped_ioctls_succeed_once_module_connected`,
  `::module_scoped_ioctls_succeed_after_lazy_open_via_create_cll`,
  `::module_scoped_unsupported_ioctls_reject_when_module_not_connected`,
  `::set_prog_voltage_rejects_module_not_connected_even_with_missing_input_data`;
  `j2534-0404-service/src/service.rs::tests::require_connected_device_for_rejects_a_module_marked_not_avail_by_a_hard_error`,
  `::require_connected_device_for_accepts_an_open_device_with_ready_status`;
  `j2534-0404-service/src/service/rpc_module.rs::tests::module_connect_rejects_a_device_a_hard_error_marked_not_avail`,
  `::module_disconnect_then_module_connect_recovers_a_device_marked_not_avail`.
- Bypass fix (Codex review, PR #143, round 3 on the same mechanism — see ADR-131's Amendment):
  `rpc_create_com_logical_link` never checked `module_state.status` at all, and
  `spawn_new_shared_channel` unconditionally reset it to `PduModstReady` whenever a CLL's first
  physical channel opened successfully — together, a client could create a fresh CLL and connect
  it on a `NotAvail` module (a hard error on a DIFFERENT channel leaves other channels connectable)
  and have that success silently clear `NotAvail`, completely bypassing `ModuleConnect`'s own
  sticky rejection with no `ModuleDisconnect` ever having run. Fixed by adding the same
  `module_state.status` check `ModuleConnect` uses to `CreateComLogicalLink` (rejecting with
  `PDU_ERR_MODULE_NOT_CONNECTED`, per Table 9's own listed return, rather than `ModuleConnect`'s
  `PDU_ERR_FCT_FAILED`) and removing `spawn_new_shared_channel`'s reset entirely. Regression tests:
  `j2534-0404-service/src/service/rpc_link.rs::tests::create_com_logical_link_rejects_when_module_marked_not_avail`,
  `::connect_com_logical_link_rejects_when_module_marked_not_avail` (renamed and inverted by ADR-134
  below — see that entry for why).
- Message-correctness fix (Codex review, PR #143, round 4 on the same mechanism — see ADR-131's
  Correction): the `NotAvail`-but-open rejection at both call sites above reused
  `module_not_connected_status`'s generic "call ModuleConnect first" message — actively wrong
  advice for this specific state, since `ModuleConnect` itself rejects it too (per the fix above).
  A new `module_not_avail_status` helper gives the correct instruction ("call ModuleDisconnect,
  then ModuleConnect"); `module_not_connected_status` is now reserved for the genuinely-nothing-open
  case. ADR-107 Decision (d)'s own description of `ModuleConnect`'s already-open branch (still
  describing the superseded intermediate blind-reset behavior) is corrected to match.
- Two residuals ADR-131 left deferred to the backlog, closed by **ADR-134**
  (design-advisor-reviewed): `GetVersion` still never checked `module_state.status` at all, and
  `ConnectComLogicalLink` was still not gated on it either. Auditing every code path that can reach
  `spawn_new_shared_channel` for `ConnectComLogicalLink`'s gap found a third one beyond the primary
  channel: `CoptUpdateparam` -> `promote_unique_resp_id_table` -> `ensure_uudt_companion_channel`,
  entirely independent of `ConnectComLogicalLink`. ADR-134 added the same scoped `module_state`
  check to `rpc_get_version`, to `rpc_connect_com_logical_link` (covering the primary channel), and
  to `ensure_uudt_companion_channel` itself (the single choke point for every companion-channel
  open/join, covering all three of its callers), plus hardened `probe_can_channel_mode`'s cache
  write-back to also require `PduModstReady`. Two further Codex review rounds on the same PR found
  the `module_state` gates alone were not sufficient: round 5 showed `events::handle_channel_hard_error`
  never acquires `device_id`, so a hard error on another channel can flip `module_state` to `NotAvail`
  between a gate's check and the native work it guards completing (accepted for a fresh
  `PassThruConnect`, since no lock can close it and a genuinely dead device fails the native call
  regardless); round 6 showed that acceptance did NOT hold for the existing-channel-**join** branches
  in `rpc_connect_com_logical_link`/`ensure_uudt_companion_channel`, which reuse a `SharedChannel`
  entry's `channel_id` with no liveness check at all -- a join racing ahead of the hard-error
  handler's `module_state` write could publish a CLL as `connected` on a channel whose poll task is
  exiting, a permanent silent hang no future RX poll would ever self-heal. Fixed by giving
  `SharedChannel` a `dead: bool` tombstone, set by `handle_channel_hard_error` under the same
  `shared_channels` guard as its per-CLL teardown (before `module_state` is ever touched), checked
  by both join branches. See ADR-134's two Correction sections for the full account. Regression
  tests:
  `j2534-0404-service/src/service/rpc_module.rs::tests::get_version_rejects_a_device_a_hard_error_marked_not_avail`;
  `j2534-0404-service/src/service/rpc_link.rs::tests::ensure_uudt_companion_channel_rejects_when_module_marked_not_avail`,
  `::connect_com_logical_link_rejects_a_dead_shared_channel_even_when_module_is_ready`,
  `::ensure_uudt_companion_channel_rejects_a_dead_shared_channel_even_when_module_is_ready`.
- Accepted residual (Codex review, PR #143; design-advisor-confirmed; recorded in full as ADR-107
  Accepted Residual #5): `require_connected_device_for` gates on "a device is open under this
  handle, with `module_state.status` still `PduModstReady`," not on whether `ModuleConnect` was
  the literal RPC that opened it — `GetVersion`/`CreateComLogicalLink`'s own pre-existing
  lazy-open (ADR-107 Decision (d)) also satisfies it, so these module-scoped IOCTLs will succeed
  (or, for the 3 unsupported ones, proceed past the connection gate to their usual
  `Status::unimplemented`) after a lazy open with no prior `ModuleConnect` call. This is
  deliberate, not a gap: NOTE 1 keys `PDU_ERR_MODULE_NOT_CONNECTED` to the module's
  *status* (`PDU_MODST_READY`), not call history, and this service's READY-equivalent state is
  "device open, `module_state.status` still `PduModstReady`" — `ModuleConnect` itself does nothing
  beyond `ensure_open_device_for` plus an event. A stricter "was `ModuleConnect` literally called"
  flag would invent a state ISO 22900-2 doesn't have (a live, communicating CLL whose module still
  reports not-connected) and would
  require rewriting the ~450 `grpc_mock` tests that lazily open via `create_cll` with no prior
  `ModuleConnect`. See ADR-107 for the full rationale.

**A2-9. Moved to Part B — see B11.** ADR-060's own "Out of scope" section already
documents and accepts RC21/RC23 re-requests going ungap-checked as a known limitation;
listing it again here duplicated an already-ADR-decided deviation, contradicting Part A's
own definition (defects *not* covered by an ADR). The one implementation-level detail not
covered by ADR-060 itself — an explicit `CP_RC2xRequestTime = 0` being coerced to a fixed
25 ms instead of `CP_P3Min` (`service.rs:215-218`) — is carried forward in B11's entry
below rather than lost.

**A2-10. FIXED (ADR-116): `TxFlagRaw` is now decoded as ISO 22900-2 D.2.1 layout, matching the proto's own documented contract and `iso22900-service`'s treatment of the same field.**
- Fixed in `j2534-0404-service/src/service/rpc_primitive.rs`'s `compute_j2534_tx_flags`
  (`TxFlagRaw` arm): raw bytes are now decoded bit-by-bit per ISO 22900-2 D.2.1 (Table D.4)
  — byte 2 bit 1 (`WAIT_P3_MIN_ONLY`) and byte 3 bit 6 (`ISO15765_FRAME_PAD`) map to their
  J2534 `TxFlags` equivalents (`0x200`/`0x40`) — instead of the previous byte-copy-as-u32
  interpretation. `CAN_29BIT_ID`/`ISO15765_ADDR_TYPE` raw positions are deliberately not
  decoded (same reasoning as the named-bit arms: `apply_resolved_tx_flags` always overrides
  those two bits from resolved CAN addressing, ADR-062); `SUPPRESS_POS_RESP`/
  `ENABLE_EXTRA_INFO` (byte 0) have no J2534 `TxFlags` equivalent and are dropped.
  `vci-service-interface/src/proto/service.proto`'s `tx_flag_raw` comment now spells out the
  D.2.1 layout explicitly; `src/bindings/vci.service.rs` regenerated to match.
  Product decision (asked before changing anything — this was a load-bearing wire-
  contract ambiguity, not a bug with one obvious fix) confirmed the ISO 22900-2 D.2.1
  interpretation, cross-checked against `iso22900-service::convert.rs`'s `tx_flag_to_iso`,
  which already passes the same shared proto field's raw bytes straight through to the
  native D-PDU API's `PDU_COP_CTRL_DATA.TxFlag` (i.e. the D.2.1 byte array itself).
  Regression tests:
  `j2534-0404-service/tests/grpc_mock/comparam_tx.rs::iso15765_frame_pad_and_wait_p3_min_only_decoded_from_raw_tx_flag_iso_d21_layout`,
  `::iso15765_raw_tx_flag_can_addressing_bits_ignored_and_overridden`.

**A2-11. RECLASSIFIED (ADR-124, superseding ADR-121): `N_WFTmax` (`CP_CanMaxNumWaitFrames`) governs WAIT frames this module transmits as a receiver, not WAIT frames it tolerates as a sender — the software TX driver's liveness fix is retained as an internal safety bound, not comparam enforcement.**
- ADR-121 (this document's prior entry) enforced `CP_CanMaxNumWaitFrames` in `isotp_send`'s
  FC-wait loop, on the assumption that the comparam bounds how many `FS_WAIT` frames this
  service tolerates from a peer while sending. A post-merge Codex review round and
  re-investigation (ADR-124) found this backwards: the comparam bounds WAIT frames this
  service itself transmits when acting as RECEIVER of a segmented message, evidenced by its
  `names.rs` grouping alongside `CP_BlockSize`/`CP_StMin` (the RX-side FlowControl-generation
  parameters) rather than the distinct `*_TX` override constants, and by J2534-1's native
  default of `0` being incoherent under the sender-tolerance reading for a
  "Pass-Thru Vehicle Programming" spec.
- This module's software RX/reassembly path never sends `FS_WAIT` (always
  `FS_CONTINUE_TO_SEND`, `isotp.rs:397`), so `CP_CanMaxNumWaitFrames` is satisfied trivially
  as-is; native `SET_CONFIG` forwarding is unaffected. The original liveness bug (an ECU
  emitting `FS_WAIT` faster than `N_Bs` re-arming the TX driver's wait indefinitely) is still
  closed, but by a fixed internal constant (`ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES = 1027`,
  ISO 22900-2's own maximum for this comparam's range), not by the comparam's configured
  value.
- File: `j2534-0404-service/src/service/events.rs` (`isotp_send`).
- See ADR-124 for the full direction analysis and its accepted residuals.

**A2-12. FIXED: `CP_CanRespUUDTId` default (`0x5E8`) contradicted spec default (`0xFFFFFFFF` = unused).**
- Fixed by changing all five default-preset call sites to `0xFFFF_FFFF`, matching the
  spec's "not used" sentinel for `ISO_15765_2`/`ISO_15765_4`/`ISO_11898_RAW` (Table B.20
  gives no non-sentinel default for any bus type this service presets).
- File: `comparam_defaults.rs:426,851,919,1009,1080` (functions `iso_11898_raw`,
  `iso_14230_3_on_iso_15765_2`, `iso15765_4_common`, `iso_15765_3_on_iso_15765_2`,
  `sae_j2190_on_iso_15765_2`).
- Spec: Table B.20 (line 6001).
- Codex follow-up (same PR, caught in review): the default-value fix alone was
  insufficient — `rpc_link.rs`'s UUDT filter/companion-channel gating (`rpc_link.rs`,
  formerly `:1841,2178,2278,441`) tested only `PARAM_CAN_RESP_UUDT_ID` key *presence*,
  and the key is always present in an unedited `GetUniqueRespIdTable` template regardless
  of value, so the fix merely swapped a `0x5E8` phantom route for a `0xFFFFFFFF` phantom
  route/filter. Fixed by adding a `uudt_resp_id` helper that treats the `0xFFFFFFFF`
  sentinel as absent, used at all four call sites (`can_connect_flags`,
  `ConnectComLogicalLink`'s `has_uudt_ids`, `install_point_to_point_fc_filters`,
  `promote_unique_resp_id_table`'s `has_uudt_ids`).
- Scenario (now closed): a client using the spec's template-copy pattern for
  `SetUniqueRespIdTable` no longer registers a phantom UUDT route/filter, whether or
  not it edits the inherited `CP_CanRespUUDTId` — the sentinel value is honored
  end-to-end, not just at the default-value layer.

**A2-13. FIXED (ADR-130): `ISO_11898_3_DWFTCAN` default baud rate (500 kbps) exceeded the bus's own 125 kbps physical limit.**
- Fixed by changing `iso_11898_3_dwftcan`'s `DATA_RATE` default from `500_000` to
  `125_000`, matching Table B.21's `CP_Baudrate` row (`ISO_11898_3_DWFTCAN = 125k`).
  While reading the full Table B.21 row set to fix this, also found the same
  function's `CP_CanBaudrateRecord` default (`250_000`) had no basis in spec (Table
  B.21 only defines a `CP_CanBaudrateRecord` default for `ISO_11898_2_DWCAN` and
  `SAE_J1939_11_DWCAN`) and independently exceeded the same 125 kbit/s physical
  limit; removed it. See ADR-130 for the full rationale.
- File: `comparam_defaults.rs:80-97` (was `82-95`) — the function's own doc comment
  said "up to 125 kbit/s," but `DATA_RATE = 500_000`.
- Spec: Table B.21 (line 6075, 6078).

**A2-14. FIXED: `SetUniqueRespIdTable`'s non-UNIQUE_ID rejection now attaches the rich `ErrorDetail` other rejections in the same RPC family use post-ADR-105.**
- Fixed by converting both the `Unum32` and `Bytefield` non-`PDU_PC_UNIQUE_ID`-class
  rejections in `rpc_set_unique_resp_id_table` from a bare `Status::invalid_argument`
  to `state_guard_status(..., PduError::PduErrComparamNotSupported, last_error)` — not
  `PDU_ERR_INVALID_PARAMETERS`, which Table 37 (§9.4.28.5, line ~2860) reserves for a
  NULL `pUniqueRespIdTable` pointer; `PDU_ERR_COMPARAM_NOT_SUPPORTED` is the table's own
  code for a list that contains a ComParam not of class PDU_PC_UNIQUE_ID, matching
  the reciprocal direction already implemented in `comparam_support.rs::check_param_allowed`
  (ADR-042). Also fixed a stale-`last_error`-snapshot bug found across two review
  rounds (`edge-case-hunter`, then a Codex round on the fix's own PR): a single
  snapshot taken before the ComParam-validation loop could go stale by the time a
  later table entry triggers a rejection, since the loop holds no lock across its
  (client-controlled, unbounded) iterations and this service runs a multi-threaded
  `#[tokio::main]` runtime where a concurrent RPC can update the same CLL's tracked
  error mid-loop. Both rejection branches now read `last_error` fresh, immediately
  at their own guard decision, matching `state_guard_status`'s documented
  same-lock-acquisition contract, instead of sharing one upfront read.
- File: `rpc_misc.rs` (`rpc_set_unique_resp_id_table`).
- Spec: ISO 22900-2:2009(E) Table 37 (§9.4.28.5, line ~2860).
- File: `rpc_misc.rs:1784-1801`

**A2-15. FIXED (ADR-123): `LockResource`/`UnlockResource` now conform to ISO 22900-2
§9.4.13/§9.4.14.**
- Fixed in `j2534-0404-service/src/service/{rpc_link.rs,rpc_primitive.rs,rpc_misc.rs,
  events.rs,service.rs}`:
  - `UnlockResource` now atomically validates the mask (no partial unlock) and returns
    `PDU_ERR_RSC_NOT_LOCKED` for a bit no one holds, or `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` for a
    bit another CLL holds (spec 9.4.14.5, lines 1887/1889).
  - `LockResource` now checks "active transmissions on the resource" (§9.4.13.2 b, line 1804)
    when granting `LOCK_PHYSICAL_TX_QUEUE` — rejects with `PDU_ERR_FCT_FAILED` if another CLL
    has an actively-executing COP on the resource; not applied to a `LOCK_PHYSICAL_COM_PARAMS`
    -only request (spec use case 2, line 1814).
  - TX-queue lock now implements spec use case 1 (line 1810): granting `LOCK_PHYSICAL_TX_QUEUE`
    suspends (queues, in `tx_held`) other CLLs' new `COPT_SENDRECV`/`COPT_STARTCOMM`/non-empty-
    data `COPT_STOPCOMM` COPs instead of rejecting them outright, and resumes them on unlock —
    reusing the existing `PDU_IOCTL_SUSPEND_TX_QUEUE`/`_RESUME_TX_QUEUE` machinery, now split
    into independent `tx_suspended_by_ioctl`/`tx_suspended_by_lock` flags (`LogicalLinkState`,
    `service.rs`) so neither source can clobber the other's suspension, with a new
    `recompute_lock_tx_suspensions` sweep re-evaluating every CLL's lock-driven suspension on
    every lock/unlock/connect/disconnect/destroy. The prior synchronous hard-reject in
    `rpc_primitive.rs`'s `StartComPrimitive` handler is removed.
  - `LockResource`/`UnlockResource` now reject a `lock_mask` that is zero or contains any bit
    outside `LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE` with
    `PDU_ERR_INVALID_PARAMETERS`, instead of silently masking off undefined bits.
  - Also corrected: `LockResource`'s conflicting-holder rejection now returns
    `PDU_ERR_RSC_LOCKED` instead of `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` (Table 21, spec lines
    1836-1845, does not list the latter as a legal `PDULockResource` return — it is legal only
    for `PDUUnlockResource`, Table 22).
  - Regression tests: `j2534-0404-service/tests/grpc_mock/locks_and_param_classes.rs` (6 new
    tests covering each fix above, including a newly-connecting CLL inheriting a sibling's
    TX-queue-lock suspension); `pdu_ioctl.rs`/`stopcomm_data_tx.rs` (2 pre-existing tests
    asserting the old hard-reject rewritten to assert queue-then-dispatch-after-unlock).
  - See ADR-123 for the full design rationale, alternatives considered, and accepted
    residuals (TesterPresent bypassing TX-suspend per pre-existing ADR-081; a single-read-point
    check-vs-in-flight-send window on the grant, same class as ADR-110's own accepted residual;
    the `PDU_IT_INFO` lock-status-change callback remains unimplemented).

**A2-16. FIXED: RC21/RC23/`CP_P3Func`/`CP_P3Phys`/`CP_TesterPresentTime` default values diverged from Table B.19 (line 5913) in specific, verified presets — re-verified with concrete `file:line` citations and actual/expected values (superseding the original per-area audit transcript, which is not stored in this repo).**
- Fixed in `j2534-0404-service/src/service/comparam_defaults.rs`:
  `kwp_on_kline_common` (`:445` RC21, `:448` RC23 -> `0`; `:459` TesterPresentTime -> `3_000_000`),
  `iso_14230_3_on_iso_14230_2` (`:508` RC21, `:516` RC23 -> `0`),
  `iso_15031_5_on_iso_14230_4` (`:537` RC21, `:546` RC23 -> `0`),
  `sae_j2190_on_iso_14230_2` (`:566` RC21, `:575` RC23 -> `0`),
  `iso15765_4_common` (`:857` RC23 -> `0`; `:867` TesterPresentTime -> `3_000_000`; added
  `PARAM_P3_FUNC`/`PARAM_P3_PHYS` = `50_000` each, previously never seeded),
  `iso_15765_3_on_iso_15765_2` (`:939-940` `CP_P3Func`/`CP_P3Phys` `150_000` -> `50_000`).
  Regression test: `j2534-0404-service/tests/grpc_mock/resources.rs::create_com_logical_link_a2_16_comparam_defaults`.
  `CP_P2Max` for ISO_15765_4 and the J1850/J1939 preset families were left open pending
  investigation as of this fix (out of scope for it) — since resolved, see the notes below:
  J1939's is already correctly seeded (no code change needed), J1850's `100_000` already
  matches spec unchanged across editions (`j1850_common:1223`), and ISO_15765_4's `50_000`
  is confirmed a genuine 2009→2022 spec revision, not a bug (2026-08-07 VERIFIED entry below).
  **Correction (Codex review, PR #120):** `kwp_on_kline_common` is also called by
  `iso_obd_on_k_line` (resource `0x0213`, `ISO_OBD_on_K_Line`) — a 4th caller not cited above
  and not part of A2-16's verified scope, since it maps to `ISO_9141_2`, which Table B.19 has
  no `CP_RC21RequestTime`/`CP_RC23RequestTime`/`CP_TesterPresentTime` row for at all. The shared
  helper's fix silently leaked into this resource too; restored its original
  `200_000`/`200_000`/`2_000_000` values with an explicit post-common-call override
  (`comparam_defaults.rs:761-762`) and extended the regression test to assert the resource
  0x0213 CLL keeps them unchanged.
  **Citation accuracy note (edge-case-hunter, A2-16 verification pass):** in each of the three
  K-line preset functions, only one of the RC21/RC23 sites listed above per protocol is actually
  load-bearing — insertion order into the `HashMap`-backed `ComParamSet` means the *other* site
  is silently shadowed. For RC21, `kwp_on_kline_common`'s own `:445` always wins (nothing
  overrides it afterward), so each caller's pre-common-call `:508`/`:537`/`:566` was dead before
  this fix and remains dead after it — updated to `0` anyway for consistency with the value that
  actually applies, not because it has any runtime effect. For RC23, the reverse holds: each
  caller's post-common-call override (`:516`/`:546`/`:575`) always wins, so `kwp_on_kline_common`'s
  own `:448` is the dead one — same reasoning for updating it anyway. This was true before this
  fix too (a pre-existing code-structure quirk, not introduced here) and doesn't change the
  correctness of the fix (isolation-tested: reverting any load-bearing site alone fails the new
  regression test; reverting a shadowed site alone does not) — noted here only so a future editor
  changing just one site of a pair doesn't assume it alone is sufficient.
  **Known residual (edge-case-hunter, A2-16 verification pass):** seeding a literal `CP_RC21RequestTime`/
  `CP_RC23RequestTime = 0` here is the spec-correct *value* (`GetComParam` now correctly reports
  `0`). The runtime RC21/RC23 retry-wait handling of that value is B11's concern, not A2-16's —
  see B11 for its current disposition.
- File: `j2534-0404-service/src/service/comparam_defaults.rs`. All expected values below are
  quoted from Table B.19 (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5937-5960`).
- **`CP_RC21RequestTime`/`CP_RC23RequestTime` hardcoded to 200 000 µs for every K-line
  (ISO14230/KWP) preset, but spec wants 0 for all of them:**
  - `kwp_on_kline_common` (`:445`, `:448`) sets both to `200_000`; re-affirmed unchanged in
    every caller: `iso_14230_3_on_iso_14230_2` (`:508` RC21, `:516` RC23 — **ISO_14230_3**,
    spec = 0/0), `iso_15031_5_on_iso_14230_4` (`:537` RC21, `:546` RC23 — **ISO_14230_4**,
    spec = 0/0), `sae_j2190_on_iso_14230_2` (`:566` RC21, `:575` RC23 — **SAE_J2190**,
    spec = 0/0).
  - `iso15765_4_common` (`:854` RC21 = `200_000`, matches spec `ISO_15765_4=200000`; but
    `:857` RC23 = `200_000` vs. spec `ISO_15765_4=0` — RC23 only is wrong here).
  - `iso_15765_3_on_iso_15765_2`'s `:943` `RC21_REQUEST_TIME=10_000` matches spec
    (`ISO_15765_3=10000`) — **not a bug**, included for contrast.
- ~~`CP_RC21Handling` for ISO_14230_4~~ — **removed, not a bug.** `iso_15031_5_on_iso_14230_4`
  writes `2` at `:536`, but its `kwp_on_kline_common(&mut p, 0x02, 0)` call at `:542` runs
  *after* that insert and unconditionally overwrites `PARAM_RC21_HANDLING` back to `0`
  (`:444`) — the same HashMap key, last write wins. The preset's actual final value is `0`,
  matching spec `ISO_14230_4=0`.
- **`CP_TesterPresentTime` (`PARAM_TESTER_PRESENT_INTERVAL_US`) hardcoded to 2 000 000 µs for
  every K-line preset via `kwp_on_kline_common:459`** (the ISO_15765_3 CAN-protocol spec
  value, misapplied to K-line) — spec wants `3000000` for `ISO_14230_3`/`ISO_14230_4`/
  `SAE_J2190` (all K-line). Also `iso15765_4_common:867` sets `2_000_000` for **ISO_15765_4**
  vs. spec `3000000`.
- **`CP_P3Func`/`CP_P3Phys` for ISO_15765_3** (`iso_15765_3_on_iso_15765_2:939-940`):
  implemented `150_000` for both vs. spec `ISO_15765_3=50000` for both (3x too high).
- **`CP_P3Func`/`CP_P3Phys` for ISO_15765_4 are never seeded at all** — neither
  `iso15765_4_common` nor the `can_iso15765_common` helper it calls (`:765-776`) inserts
  either param, unlike spec's explicit `ISO_15765_4=50000` for both — a client reading
  either gets `0`/absent rather than the spec default.
- **Resolved: J1850/SAE_J1939_73 re-verified against Table B.19.** The J1850 family
  (`j1850_common`, `comparam_defaults.rs:1189-1242`) carried the same stale-hardcoded-literal
  bug class A2-16's original pass already found and fixed in the K-line presets: `CP_RC21RequestTime`
  (`:1211`, was `200_000`) and `CP_RC23RequestTime` (`:1214`, was `200_000`) both want `0` per
  Table B.19's `CP_RC21RequestTime`/`CP_RC23RequestTime` rows (spec doc lines 5939/5942,
  `SAE_J1850_VPW`/`SAE_J1850_PWM` both `0`). The `200_000` figure has no clean single-protocol
  Table B.19 basis for J1850 -- it happens to numerically match `ISO_15765_4`'s
  `CP_RC21RequestTime` default (spec doc line 5939, `ISO_15765_4=200000`), not `ISO_15765_3`'s
  (`ISO_15765_3=10000`/`0` for RC21/RC23 respectively, same line/line 5942); `CP_TesterPresentTime`
  (`PARAM_TESTER_PRESENT_INTERVAL_US`, `:1224`, was `2_000_000`, the `ISO_15765_3` value, spec doc
  line 5959) wants `3_000_000` per Table B.19's `CP_TesterPresentTime` row (spec doc line 5959,
  `SAE_J1850_VPW`/`SAE_J1850_PWM` both `3000000`). Fixed all three literals in the shared
  helper — the sole call site for these three params across all five J1850 callers
  (`iso_15031_5_on_sae_j1850_pwm`, `iso_15031_5_on_sae_j1850_vpw`, `iso_obd_on_sae_j1850`,
  `sae_j2190_on_sae_j1850_pwm`, `sae_j2190_on_sae_j1850_vpw`); none override these three params
  after the `j1850_common` call. Regression test:
  `j2534-0404-service/tests/grpc_mock/resources.rs::create_com_logical_link_a2_16_j1850_comparam_defaults`
  (resources `0x0215`/`0x0217`/`0x0218`/`0x021D`, covering both PWM/VPW bus variants, both
  ISO_15031_5/SAE_J2190 protocol families, and the distinct `iso_obd_on_sae_j1850()` preset
  reached only via 0x021D's name-based lookup).
  `CP_P3Func`/`CP_P3Phys` absence for J1850 is confirmed spec-correct, not a bug — Table B.19
  defines no default for either param under `SAE_J1850_VPW`/`SAE_J1850_PWM`, and Table B.10's
  applicability matrix has blank cells there too. Table B.10's `CP_RC21RequestTime`/
  `CP_RC23RequestTime` rows (spec doc lines ~5629/5632) are less clean corroboration for the
  `0` default above: they show a blank cell for `SAE_J1850_VPW` but `S,T` for `SAE_J1850_PWM`,
  an asymmetry that doesn't cleanly match Table B.19's symmetric `SAE_J1850_VPW=SAE_J1850_PWM=0`.
  Per ADR-133 (`docs/adr/ADR-133-getcomparam-class-and-typed-empty-fallback.md:32-40`), Table
  B.10/B.11's OCR row-association is already independently known to be unreliable, so this is
  treated as an open, low-stakes corroboration gap rather than a reason to doubt Table B.19 (the
  value table, and this repo's established primary source).
  SAE_J1939_73 (`j1939_can_common`) was also re-checked: Table B.19 defines no default at all
  for `SAE_J1939_73` under `CP_RC21CompletionTimeout`/`CP_RC21Handling`/`CP_RC21RequestTime`/
  `CP_RC23CompletionTimeout`/`CP_RC23Handling`/`CP_RC23RequestTime`/`CP_P3Func`/`CP_P3Phys`/
  `CP_TesterPresentTime` — its only three `SAE_J1939_73` rows (`CP_MessageIndicationRate`,
  `CP_P2Max`, `CP_SuspendQueueOnError`) are already correctly seeded. No code change needed for
  SAE_J1939_73.
  Per ADR-125's `RcHandlingConfig::from_params` (`service.rs`), the `is_j1850_family()` branch
  already treats an explicit `CP_RC2xRequestTime = 0` verbatim at runtime (no `CP_P3Min` floor,
  since J1850 has no `CP_P3Min` per Table B.10/B.19) — so these corrected defaults take effect
  with no further runtime change needed. See B11 for the K-line runtime-handling disposition.
  **Declined (Codex review, PR #7, design-advisor consult):** the bare native `SAE_J1850_PWM`/
  `SAE_J1850_VPW` channels (resources `0x0216`/`0x0219`, ADR-069's allowlist) do not need these
  Table B.19 values seeded. Table B.10/B.19's transport-named columns (`SAE_J1850_VPW`/
  `SAE_J1850_PWM`/`ISO_9141_2`/`ISO_14230_4`) denote the `ISO_15031_5` OBD application stack keyed
  by its transport (Table B.2), not the bare native protocol itself — `ISO_15031_5` has no column
  of its own precisely because its values vary per transport and live in these columns instead
  (the `ISO_9141_2` column defaulting tester-present *on* with the OBD $01/$00 keep-alive would be
  nonsensical seeded onto a bare, application-layer-free channel). ADR-069's own allowlist rationale
  was corrected in the same commit (it cited a nonexistent "ISO 22900-3" and mischaracterized why
  these five are exempt; the Decision itself — no protocol-layer overlay for the five bare native
  resources — was already correct). **VERIFIED against the 2022 edition (2026-08-07): confirmed,
  not just re-checked.** The 2022 text's own Table B.10 (Application layer ComParam summary table)
  carries an explanatory note absent from the 2009 text, stating that these five columns
  (`ISO_15765_4`/`ISO_14230_4`/`ISO_9141_2`/`SAE_J1850_VPW`/`SAE_J1850_PWM`) were originally
  presented as one grouped set under a shared, OBD-emissions-labeled heading spanning all five —
  i.e. those columns represent `ISO_15031_5`'s values as carried over each transport, exactly the
  reading this finding already inferred from the 2009 text alone. No code change; this closes the
  flag with higher confidence than the original 2009-only reading had. The same conclusion applies
  to bare `ISO_9141_2` (`0x0210`)/`ISO_14230_4` (`0x020C`) per the identical reasoning.
- **VERIFIED against the 2022 edition (2026-08-07): `CP_P2Max` for ISO_15765_4 is a confirmed
  2009→2022 spec revision, not a bug — no code change.** The 2022 text's own per-ComParam
  definition (Annex B.5's per-ComParam detailed-definition section, paralleling the 2009 Table B.19 the
  original finding cited) lists `ISO_15765_4=50000` — the exact value already implemented
  (`iso15765_4_common:902`, 50 000 µs) — where the 2009 text listed `140000`. The other eleven
  protocols' `CP_P2Max` defaults are unchanged between editions (including J1850's `100_000`,
  `j1850_common:1223`, and J1939's already-correct seeding noted above). Confirms this finding's
  own hypothesis: 50 ms P2 is what real ISO 15765-4 ECUs use server-side, and the 2022 edition
  formalizes that in the spec text itself.
- `CP_P2Star`'s absent preset seeding is **moved to Part B — see B25.** ADR-102 explicitly
  removes the ADR-056 sync step that copied `CP_RC78CompletionTimeout` into `CP_P2Star`,
  relying on `RcHandlingConfig`'s own 5000 ms runtime fallback instead — a deliberate,
  reasoned decision (confirmed: no `PARAM_P2_STAR` insert exists in `comparam_defaults.rs`
  outside a test asserting its absence, `:1633`), not an undecided Part A gap.

**A2-17. FIXED (ADR-133): `GetComParam` always reports `com_param_class = PDU_PC_SPECIFIED`, never the ComParam's real Annex B.3.2 class.**
- File: `rpc_link.rs` (`rpc_get_com_param`), `comparam_support.rs`.
- Note: documented as a known limitation in `docs/rpc-api-guide.md`, but diverged
  from `iso22900-service`'s behavior (real classes) and was user-visible.
- Fixed by a new `comparam_support::com_param_class`, reporting `PDU_PC_BUSTYPE`
  (via the existing `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` lists) and `PDU_PC_TESTER_PRESENT`
  (a new `TESTER_PRESENT_CLASS_PARAMS` list, the 9 `CP_TesterPresentxxx` params plus
  `CP_TesterPresentImmed`) — the two classes with verifiable spec citations and
  D-PDU-API-visible behavioral consequences. `PDU_PC_UNIQUE_ID` is provably
  unreachable here (rejected earlier by `check_param_allowed`). Every other
  ComParam still reports `PDU_PC_SPECIFIED` (0): the only in-workspace source
  that could classify the rest (ISO 22900-2:2009(E) Tables B.10/B.11) is
  flagged unreliable by its own conversion note and internally inconsistent
  where spot-checked, so a full TIMING/INIT/COM/ERRHDL classification could
  not be verified to this audit's own standard — see ADR-133 for the full
  analysis, including the `CP_Parity`/`CP_TesterPresentImmed` edge cases an
  `edge-case-hunter` review surfaced during this fix.

**A2-18. FIXED (ADR-133): `GetComParam` returns `Unum32(0)` for an unseeded Bytefield/Structfield param instead of the correct empty-bytes shape.**
- File: `rpc_link.rs` (`rpc_get_com_param`, `rpc_set_com_param`), `comparam_support.rs`,
  `comparam_defaults.rs`.
- Fixed by extending ADR-130's `CP_CanBaudrateRecord`-only fix to every
  Bytefield-typed (9) and Structfield-typed (2) ComParam this service
  supports, via new exhaustive-by-construction `BYTEFIELD_PARAMS`/
  `STRUCTFIELD_PARAMS` lists shared between `rpc_get_com_param`'s fallback
  and `rpc_set_com_param`'s write validation. `CP_SessionTimingOverride`'s
  empty shape reuses `session_timing_empty()`; `CP_ExtendedTiming` gets a new,
  distinct `access_timing_empty()` (zero entries, matching Table B.19's own
  default text), deliberately not the pre-existing `access_timing_zero()`
  (one all-zero-valued entry — a genuine seed default for 4 KWP presets, not
  the "nothing configured" shape). `rpc_set_com_param`'s `Unum32`-branch
  type-mismatch rejection (previously `CP_CanBaudrateRecord`-only, per
  ADR-130's Amendment 2) is widened to every Bytefield-/Structfield-typed
  param, closing the same write-masking hazard ADR-130 fixed once, now for
  all 11 typed params. See ADR-133.

**A2-19. FIXED: `PDU_IOCTL_STOP_MSG_FILTER`'s unknown `FilterNumber` now returns `PDU_ERR_INVALID_PARAMETERS` instead of `PDU_ERR_INVALID_HANDLE`.** (Corrected command name — the cited code is `ioctl_stop_msg_filter`'s lookup, not `START_MSG_FILTER`, which installs newly supplied filter numbers rather than looking up existing ones.)
- Fixed both "unknown FilterNumber" miss branches in `ioctl_stop_msg_filter` — the
  pre-connect `pending_client_filters` miss and the connected `client_filters` miss —
  changing from `unknown_handle_status(...)` (hardcoded `PDU_ERR_INVALID_HANDLE`) to
  `state_guard_status(Code::InvalidArgument, ..., PduError::PduErrInvalidParameters,
  last_error)`, per Table 55's "...or the Filter Number is invalid" clause. The
  unrelated `LOCK_PHYSICAL_COM_PARAMS` lock-holder rejection a few lines below is
  unaffected.
- File: `rpc_misc.rs:1347-1355`
- Spec: Table 55 (line 3587).

**A2-20. Moved to Part B — see B23.** ADR-079 explicitly chooses `PDU_ERR_CABLE_UNKNOWN`
for `GET_CABLE_ID` and its Consequences section labels this an explicit product decision
("rejected... by explicit product decision... not an oversight or a placeholder"). Even
though this audit's own analysis is that the decision misreads Table 53, listing it as an
unrecorded Part A defect contradicted Part A's own definition — an ADR-decided choice needs
Part B triage/reopening, not a unilateral Part A fix. See B23 for its current disposition.

**A2-21. FIXED: `SET_PROG_VOLTAGE` now distinguishes pin/resource failures (`PDU_ERR_MUX_RSC_NOT_SUPPORTED`) from voltage-value failures (`PDU_ERR_VOLTAGE_NOT_SUPPORTED`).**
- Fixed by inspecting the native `PassThruSetProgrammingVoltage` failure in
  `ioctl_set_prog_voltage`: `j2534_0404::ERR_PIN_INVALID` now maps to
  `PduError::PduErrMuxRscNotSupported` (Table 49's code for "the specified pin/Resource
  are not supported"); every other native failure keeps the prior
  `PduError::PduErrVoltageNotSupported` mapping. `j2534-0404-mock` gained a new
  error-injection hook (`prog_voltage_error`/`__mock_set_prog_voltage_error`) to test
  both branches end to end.
- File: `rpc_misc.rs:856-864`
- Spec: Table 49 (line 3378) reserves `PDU_ERR_MUX_RSC_NOT_SUPPORTED` for pin/resource
  failures.

**A2-22. Fully moved to Part B — see B24.** The truncation half is ADR-079 item 10's
explicit, deliberate design. The remaining "accepts `0` instead of erroring" half is not a
confirmed defect (Table 52, line 3456-3459, §9.5.11, lists `PDU_ERR_INVALID_PARAMETERS` only
generically and never states that `MaxRxBufferSize=0` specifically is out of range — no
governing clause was found to cite), which is why it moved to B24 rather than staying in the
confirmed-defect list, per this audit's own "independently verified, not a suspicion list"
standard — see B24 for its current disposition.
- File: `rpc_misc.rs:905-924`

**A2-23. FIXED (ADR-128): `CancelComPrimitive` returned `PDU_ERR_INVALID_HANDLE` for a COP that finished but hadn't been read yet — spec says this should succeed.**
- Fixed by adding `J2534Service::terminal_cops`, a service-wide ledger recording every COP's
  terminal status (`PduCopstFinished`/`PduCopstCancelled`), populated centrally inside
  `events::send_cop_status` (the single function every terminal-status emission funnels
  through) so every current and future emission site is covered without per-site logic.
  `rpc_cancel_com_primitive` now checks `terminal_cops` on a `primitives` miss and returns
  success (no further action, per §9.2.6.6) instead of `PDU_ERR_INVALID_HANDLE`; a miss on
  both maps still correctly returns `PDU_ERR_INVALID_HANDLE` for a genuinely unknown or
  already-destroyed handle. `rpc_get_status`'s miss-fallback consults the same ledger before
  defaulting to Finished, which also fixes a related bug where a deferred-cancel COP's status
  visibly flipped from Cancelled to Finished once the poll task's deferred emission ran.
  Entries are purged per-CLL only when that CLL is destroyed (`rpc_destroy_com_logical_link`)
  — this models COP destruction as happening at CLL-destroy time rather than literally at
  event-queue-read time, the closest spec-compatible approximation available given this
  service has no reliable "the client read the terminal status" signal across its two
  delivery paths (`SubscribeEvent` push, `GetStatus` pull). See ADR-128 for the full decision,
  alternatives considered, and accepted residuals (a sub-microsecond remove-vs-record race, a
  `u32`-wraparound handle-reuse edge case, and unbounded `terminal_cops` growth across a
  long-lived CLL's COP churn — tracked in `docs/implementation-notes.md`).
- File: `rpc_primitive.rs` (`rpc_cancel_com_primitive`, `rpc_get_status`), `events.rs`
  (`send_cop_status` and its callers), `rpc_link.rs` (`rpc_destroy_com_logical_link`), `service.rs`.
- Spec: §9.4.18.2 d) (line 2333), §9.2.6.6 (line 1174), §9.2.6.7 (line 1178).

**A2-24. FIXED (ADR-118): Periodic (cyclic) `CoptSendrecv` never emits WAITING↔EXECUTING transitions between cycles.**
- Fixed by emitting `PDU_COPST_EXECUTING` unconditionally at the top of every send cycle in
  `handle_send_recv` (removing the `is_continuation` guard that suppressed it on every cycle
  after the first — the bug's embodiment) and adding a single `PDU_COPST_WAITING` emission at
  `dispatch_tx_item`'s tail, gated on `continuation.is_some() && primitives.contains_key(...)`
  — see ADR-118 for the full decision, including the accepted residual (a rare late WAITING
  delivered after a disconnect's CANCELLED event, in a narrow race window).
- File: `j2534-0404-service/src/service/events.rs` (`handle_send_recv`, `dispatch_tx_item`)
- Spec: §9.2.6.2.3 (line 646), §9.4.17.2.2 b/c/d (2255-2265).
- Note: this fix's scope is `SubscribeEvent`/notification-driven clients only — `GetStatus`
  polling is a separate code path tracked at B26 (formerly A2-2); see B26 for its current
  disposition.

**A2-25. FIXED (ADR-132): `GetModuleIds` hardcoded `PDU_MODST_READY`, ignoring the real tracked `ModuleState` `GetStatus` uses.**
- Fixed by having `rpc_get_module_ids` read `self.device_id` to find which
  configured `module_handle` (if any) is currently open, then report the
  real `self.module_state.status` — the same value `GetStatus`'s
  `ModuleHandle` branch already returns — for that one row, and
  `PDU_MODST_AVAIL` (ISO 22900-2 §9.4.24.2.1 Use Case (a), the spec's own
  detected-but-not-connected initial state, matching Table 32's example and
  this repo's own `rpc-api-guide.md` "pick a handle with `PDU_MODST_AVAIL`"
  flow) for every other row, instead of hardcoding `READY` everywhere. This
  closes ADR-107's own Accepted Residual #1, which explicitly anticipated
  this exact fix ("a future ADR could track 'is this the currently-open
  handle'"). Probing a *closed* device's real reachability remains out of
  reach — J2534 v04.04 has no enumeration primitive for that — so an
  unopened, misconfigured, or physically-absent entry still cannot be told
  apart from a genuinely-present-but-unconnected one; this is a narrower,
  honest claim than the previous `READY`, not a claim of confirmed
  reachability, and is recorded as an accepted residual in ADR-132.
- File: `rpc_module.rs` (`rpc_get_module_ids`).
- Spec: §9.4.24.2.1 (Use Cases (a)-(e), line ~2631), Table 32 (line ~2618).

**A2-26. FIXED (ADR-120): Timestamp unit is milliseconds; spec says microseconds — and the repo's own docs disagree with each other about it. The defect is actually mixed clock sources, not just a unit mismatch.**
- Fixed by replacing the three independent Unix-ms `SystemTime` implementations
  (`events::event_timestamp_ms()`, `rpc_primitive::current_timestamp_millis()`,
  `rpc_module::rpc_get_timestamp`'s own inline logic) with one shared function,
  `events::module_timestamp_us()`: a monotonic (`Instant`-backed) microsecond `u32`
  clock, matching §9.1.6.1's unit requirement. All synthetic timestamp call sites
  (status/error events, `GetStatus`, `GetTimestamp`) now derive from this single
  function, closing the "mixed clock sources" defect for the synthetic side. RX-frame
  timestamps (`events.rs`, `let timestamp = msg.timestamp()`) are deliberately left
  untouched — still the raw native `PASSTHRU_MSG.Timestamp` device value on a
  vendor-defined epoch, per ADR-120's decision that there is no calibration primitive
  to rebase it onto the synthetic clock without destroying its accuracy. Both
  `docs/rpc-api-guide.md` and `docs/j2534-0404-architecture.md` updated to describe the
  actual (and now internally consistent) behavior.
- Files: `events.rs` (`module_timestamp_us()`, `reset_module_clock()`), `rpc_primitive.rs`,
  `rpc_module.rs`, `rpc_misc.rs` (`ioctl_reset`).
- Spec: §9.1.6.1, §9.4.6.4, §9.4.31.1.
- Note: §9.1.6.1's boot/reset-relative time-base requirement was only half-implemented
  at first: `module_timestamp_us()` originally captured its `Instant` once, permanently,
  at process start, so `PDU_IOCTL_RESET` did not rebase it as the spec also requires. A
  Codex review on PR #129 caught this gap; the ADR-120 Amendment (2026-07-23) makes the
  clock resettable (`events::reset_module_clock()`, called from
  `rpc_misc.rs::ioctl_reset`), closing the reset half of the requirement.
- Note: no cross-service alignment with `iso22900-service` — both services now share
  timestamp *semantics* (opaque, boot/reset-relative 32-bit microsecond counter) but not
  timestamp *values*; see ADR-120 Decision §4 for why no shared reference clock is
  available under J2534 v04.04.

**A2-27. Moved to Part B — see B20.** ADR-105's own `StatusCode`→`PduError` decision table
(`docs/adr/ADR-105-rich-error-model-replaces-get-last-error.md:164`) explicitly maps both
`ERR_EXCEEDED_LIMIT` and `ERR_BUFFER_OVERFLOW` to `ResourceError` — this is an ADR-decided
mapping, not an undecided defect, contradicting Part A's own definition. The tension with
D.3's "Not used by the D-PDU API" designation is carried forward in B20's entry for triage
rather than lost.

**A2-28. FIXED: Duplicate-`CoptStopcomm` guard now uses `PDU_ERR_RESOURCE_BUSY` (the correct D.3 code) instead of `PDU_ERR_RSC_LOCKED` (lock semantics).**
- Fixed by changing the guard's `PduError` from `PduErrRscLocked` to
  `PduErrResourceBusy` in `rpc_primitive.rs`, matching D.3's "the requested resource is
  already in use" semantics (this is a same-operation-already-running guard, not a
  `LockResource`-holder conflict) and this codebase's existing precedent for the same
  situation (`service.rs`'s `open_under_different_handle_status`).
- File: `rpc_primitive.rs`.
- File: `rpc_primitive.rs:1321-1330`

### A.3 — P3 (cosmetic / low-likelihood / needs 2022 text to confirm)

**A3-1/A3-2. FIXED: `GetResourceStatus` now rejects an unrecognized `ResourceName` with `PDU_ERR_INVALID_PARAMETERS` instead of silently echoing `resource_id: 0, status: 0`.**
- Fixed by rejecting with `state_guard_status(Code::InvalidArgument, ..., PduError::PduErrInvalidParameters, None)`
  when a `ResourceName` matches neither the resources table (`find_table_rows_by_name`) nor a
  legacy protocol-name alias (`map_protocol_name`), instead of falling through to an all-zero
  echo indistinguishable from a real idle/unlocked resource. Matches §9.4.8.5 Table 16, which
  lists a resource id this call can't recognize among the conditions that return
  `PDU_ERR_INVALID_PARAMETERS`, and this service's `ADR-078` reject-unrecognized-names
  philosophy elsewhere (e.g.
  `parse_protocol_id_from_resource`, used by `CreateComLogicalLink`).
- File: `rpc_link.rs` (`rpc_get_resource_status`).
- Regression test: `tests/grpc_mock/resources.rs::get_resource_status_rejects_unrecognized_resource_name`.

**A3-3. VERIFIED (2026-08-07): the 2022 edition carries the identical tension unchanged — ADR-078's rejection stance stands as the settled interpretation, not a defect.**
- Checked 2022 §8.4.23.4 (Parameters) / §8.4.23.5 (Return values, Table 31) against the 2009
  citation this finding was originally raised against (§9.4.23.4/§9.4.23.5, line 2581): the
  same two readings coexist verbatim in substance in both editions — only the clause numbering
  shifted (9.4.23.x → 8.4.23.x) — the Parameters section's PDU_ID_UNDEF-on-no-match sentence
  and Table 31's `PDU_ERR_INVALID_PARAMETERS`-on-invalid-ShortName return value are both still
  present, with no cross-reference or precedence note resolving which governs a well-formed
  but unmatched shortname. The 2022 text does not settle this either way.
- Since the spec text itself cannot resolve the tension, ADR-078's existing choice (explicit
  rejection via `PDU_ERR_INVALID_PARAMETERS` for `OBJT_BUSTYPE`/`OBJT_COMPARAM`/`OBJT_PINTYPE`/
  `OBJT_RESOURCE`/`OBJT_IO_CTRL`) is retained as this service's settled interpretation — it
  favors the return-value table's explicit enumeration over the parameter description's
  success-path wording, consistent with the reject-over-silent-fallback design ADR-078 already
  applied uniformly across all five object types (a caller error should be observable, not
  silently absorbed into `PDU_ID_UNDEF`/`0`). See ADR-078's Consequences for the verification
  note recording this outcome.
- **`OBJT_PROTOCOL` is deliberately excluded from the above, unchanged by both ADR-078 and
  this verification** (`names.rs::resolve_object_id`'s `ObjtProtocol` arm, `:1918-1927`): an
  unmatched name that also fails to parse as a raw `u32` is rejected via
  `Status::not_found("unknown protocol shortname")` — a different gRPC code and message style
  than the other five types' `PDU_ERR_INVALID_PARAMETERS`, with no `PDU_ERR_` prefix at all.
  This is not a gap A3-3 leaves open: ADR-078's Decision section explicitly carves `OBJT_PROTOCOL`
  out ("aligning its status code/message style to the other five types was considered but is
  explicitly out of scope for this decision") because a bare numeric shortname here is a
  legitimate raw `ChannelProtocol` value the caller may intentionally pass — semantically
  different from the other five types' numeric string, which was always an arbitrary,
  meaningless passthrough. `OBJT_PROTOCOL` therefore was never claimed to follow either of
  A3-3's two spec readings uniformly with the other five types, and this verification doesn't
  need it to — the closure applies to the five types ADR-078's Decision actually covers.
- Closed: documentation-only closure, no code or behavior change.

**A3-4. FIXED: native and emulated `PDU_ERR_INVALID_HANDLE` rejections now share the same outer gRPC code (`NotFound`).**
- Fixed by special-casing `map_native_error_as` (`error.rs`) to use `Code::NotFound` when the
  resolved `PduError` is `PduErrInvalidHandle`, matching `unknown_handle_status`'s existing
  `NotFound` convention for this adapter's own emulated "unrecognized handle" rejections — every
  other `PduError` keeps the pre-existing blanket `Code::Internal` for native call failures.
  Found while auditing this pair a third outer code for the same `PduError`
  (`require_module_handle`'s `Code::InvalidArgument` for an out-of-range `module_handle`); left
  unchanged as out of scope for this specific finding — tracked in
  the backlog.
- File: `error.rs` (`map_native_error_as`).
- Regression test: `error.rs::tests::map_native_error_for_link_uses_not_found_for_invalid_handle`.

**A3-5. FIXED: `docs/rpc-api-guide.md`'s DoIP error-code range corrected to "0xB0–0xBC".**
- Fixed by updating the doc table row to match the proto (`service.proto`'s
  `PDU_ERR_DOIP_RESPONSE_TIMEOUT = 0x000000BC`, one past the previously-documented `0xBB`
  ceiling) — pure doc bug, no code change.
- File: `docs/rpc-api-guide.md`.

**A3-6. FIXED: ADR-100's citation corrected to match the repo's own 2009(E) text conversion.**
- Fixed by correcting `ADR-100`'s claim that the §9.2.6.3.4 receive-handling clause is
  grammatically plural ("literal, not prose") where the repo's own converted 2009(E) text
  (Table 9's "Receive Message Handling" row and its NOTE) uses the singular form in both
  places. The ADR's Tier 1 multi-candidate grouping is re-grounded in the state machine's own
  structure (a periodic SendRecv COP and a separately executing one-shot COP are simultaneous
  match candidates by construction) rather than a grammatical-number argument. **VERIFIED
  against the 2022 edition (2026-08-07):** the 2022 text's own equivalent passage (Table 9's
  receive-handling row and its NOTE, content-equivalent to the 2009 §9.2.6.3.4 citation) also
  uses singular phrasing throughout, with no plural instance found — the same reading holds
  in both editions. No functional impact (execution was already serialized to at most one
  live `ActiveSendReceive` registrant); documentation-only fix.
- File: `docs/adr/ADR-100-cop-registry-two-tier-binding.md`.

---

## Part B — ADR-documented, already-decided deviations (triage only, do not "rediscover")

These all have a decision-of-record. Listed for completeness per the user's explicit
request; **do not implement a fix for these without first reading the cited ADR in full**
— in most cases the deviation is a J2534 v04.04 hardware/API capability limit with no
available workaround, and re-litigating it wastes a session.

| # | Area | Deviation | ADR | Suggested triage |
|---|---|---|---|---|
| ~~B1~~ | ~~ModuleDisconnect~~ | **Removed — not a deviation.** ADR-001's own rationale cites ISO 22900-2:2022 §9.3.3 as *requiring* `ModuleDisconnect` to release all of a module's resources; `rpc_module.rs:58` implements this on that basis. This is a conformance fix, not a spec deviation, and was misclassified during the ADR sweep — no conflicting 2009 requirement was found to justify listing it here. | ADR-001 | N/A |
| B2 | Protocol scope | J2534-2-only features (SWCAN/GM_UART) rejected outright | ADR-017 | Accept — explicit scope boundary |
| B3 | ComParam unit conversion | `CP_StMinOverride`→`STMIN_TX` conversion is lossy by construction | ADR-037 | Accept — inherent to J2534's coarser native encoding |
| B4 | ISO15765 filters | Unaddressed CLLs fall back to a non-conformant zero-mask filter (ADR-039 says so itself) | ADR-039 | **FIXED (ADR-122)** — the zero-mask pass-all fallback is removed entirely from every lifecycle point (`CoptUpdateparam` promotion, `CLEAR_MSG_FILTERS`; `ConnectComLogicalLink` was already fixed by ADR-048). An unaddressed CLL now gets no `FLOW_CONTROL_FILTER` of its own anywhere, matching the spec's own "nothing is received without a matching filter" default; `events.rs`'s `route_frame` software-layer broadcast-to-empty-table policy is unchanged (out of scope — needed by non-ISO15765 protocols and ADR-048's shared-channel semantics) |
| B5 | CAN ID Format | Padding-Overwrite bits (Table B.13 bits 5,4) never decoded/applied | ADR-040 | Accept as backlog — no TX path currently needs it |
| B6 | UUDT filter | Flow-Control bit intentionally ignored (asymmetric vs. USDT) | ADR-041 | Accept — deliberate, documented rationale |
| B7 | Software ISO-TP | Same CAN ID answering multiple AEs (functional, multi-target) unsupported | ADR-047 | Accept — narrow use case |
| B8 | TX size validation | Validated against SAE J2534-1's table, not ISO22900-2's own; RX unvalidated; ISO14230 Manual-Checksum variant unimplemented | ADR-049 | Accept SAE-table choice; **consider** whether RX-side validation is worth adding |
| B9 | TX addressing | Only `UniqueRespIdTable`'s first entry drives outgoing addressing — no per-request ECU target | ADR-050 | Accept — would need a proto change to fix properly |
| B10 | ComParam | `CP_P2Star_Ecu`/`CP_P2Max_Ecu` (per-ECU overrides) stored but never applied | ADR-056 | Backlog — genuine unimplemented feature |
| B11 | P3 gap | RC21/RC23 re-requests not gap-checked (`events.rs:7854-7996`, Annex I.1.2 Fig I.2/I.3, I.1.4.3 step 2 line 8354, `Max(CP_P3Min, CP_RC2xRequestTime)`); moved here from Part A's former A2-9 | ADR-060 (implementation-slip half fixed by ADR-125) | **PARTIALLY FIXED (ADR-125)** — the implementation slip is fixed: `RcHandlingConfig::from_params` (`service.rs`) now computes `rc21_request_time_ms`/`rc23_request_time_ms` as `Max(CP_P3Min, CP_RC2xRequestTime)` per Annex I.1.4.3 steps 1-3, instead of coercing an explicit `CP_RC2xRequestTime = 0` (the "re-request after P3Min" case) to a hardcoded 25 ms with no spec basis. `CP_P3Min` is a K-line-only concept (never allowlisted for CAN CLLs' `SetComParam`/`GetComParam` via `comparam_support.rs`, and undefined for J1850 in Table B.10/B.19/J2534-1), so `RcHandlingConfig::from_params` now takes an explicit `protocol: ChannelProtocol` parameter and applies two different rules: on K-line (`is_kwp_family()`), the full `Max(CP_P3Min, CP_RC2xRequestTime)` floor; on J1850 (new `is_j1850_family()` helper), the explicit request time is used verbatim with no P3Min floor (Annex I.1.4's own title covers "SAE J1850 VPW and ISO 14230 protocols", and RC21/RC23 request-time ComParams are legally settable on J1850 too) — both additionally require `CP_RC2xRequestTime` itself being present in the ComParamSet. Five Codex/adversarial review rounds on PR #136 caught this design's prior mistakes before merge (Codex hit its usage limit after round 4; round 5 was an `edge-case-hunter` adversarial pass run in its place per user direction, with a `design-advisor` consult resolving the genuine J1850 spec-interpretation question it raised): an initial, ungated version silently dropped the CAN-side 25 ms fallback to an immediate 0 ms retry for a client that runtime-enables `CP_RC23Handling`; a value-based gate (`p3_min_ms > 0`) then couldn't distinguish `CP_P3Min` being absent from a K-line client explicitly `SetComParam(CP_P3Min, 0)`-ing; a presence-based gate on `CP_P3Min` (`contains_key`) then broke on resource `ISO_14230_3_on_ISO_15765_2` (0x0204), a CAN-family preset that nonetheless seeds a literal, client-inaccessible `CP_P3Min = 55_000`; a protocol-only gate then still misread a legacy/raw K-line CLL's genuinely empty `ComParamSet` (no resources-table row matched) as the explicit-`0` case instead of the "unconfigured" 25 ms fallback; and the K-line-only gate then excluded J1850 entirely, silently coercing an explicit `CP_RC23RequestTime = 0` on a J1850VPW/PWM CLL to the hardcoded 25 ms — the exact defect this whole fix exists to eliminate, just unreached for that protocol family. ADR-060's own accepted scope — RC21/RC23 re-requests are not gap-*tracked* against `CP_P3Func`/`CP_P3Phys` state — is **unchanged, still Accept**; this fix only corrects the request-time *value* fed into the existing wait, not whether it participates in ADR-060's own gap-tracking. |
| B12 | TxFlags | Client-requested `CAN_29BIT_ID`/`ISO15765_ADDR_TYPE` always overridden by service-computed values | ADR-062 | Accept — deliberate "objective fact wins" design; see A2-10 above for the raw-field version of this ambiguity |
| B13 | Connect flags | A joining CLL's addressing mismatch vs. an already-open shared channel's `Flags` is undetected | ADR-065 | Accept — genuine J2534 limitation (can't reconnect a live channel) |
| B14 | UartConfig | 2-stop-bit / 9-data-bit configurations unrepresentable | ADR-071 | Accept — no native J2534 equivalent |
| B15 | K-line resource | Combined ISO9141/ISO14230-1 bus always connects as ISO9141, no runtime auto-select | ADR-069/017 | Accept — same class as ADR-070's J1850 fix, but K-line has no analogous active-probe mechanism available |
| B16 | 5-baud init | `CP_ExtendedTiming` settable but unimplemented (key-byte-gated ISO14230-2 timing) | ADR-076 | Backlog (P2 per ADR-076's own note) |
| B17 | IOCTL filters | `PDU_FLT_PASS`/`_PASS_UUDT` always rejected — only BLOCK filtering works | ADR-079 | Accept — would need per-CLL software RX filtering, explicitly deferred |
| B18 | Tester-present | Mode-1 first frame sent immediately, not after full idle wait (spec text read literally) | ADR-084 | Accept — explicit domain-expert sign-off on record |
| B19 | CoptStopcomm receive | IS-CYCLIC rejected; receive phase never cancellable; synthetic ceiling on IS-MULTIPLE not in spec | ADR-087 | Accept — all three exist to guarantee COP termination |
| B20 | Error mapping | `ERR_EXCEEDED_LIMIT`/`ERR_BUFFER_OVERFLOW` map onto `PDU_ERR_RESOURCE_ERROR` (0x32) (`error.rs:71-73`), which D.3 marks "Not used by the D-PDU API"; moved here from Part A's former A2-27 | ADR-105 | **Revisit** — the decision table gives no rationale for this specific mapping against D.3's "not used" designation; worth a closer look at whether a different `PduError` (e.g. `FctFailed`, the table's own fallback for unmapped codes) fits better before accepting as final |
| B21 | Msg filter stop | `STOP_MSG_FILTER`/`CLEAR_MSG_FILTER` report success even when the native stop call fails (`rpc_misc.rs:1386-1410`/`1476-1510`), keeping the id "tracked for retry" instead of returning `PDU_ERR_FCT_FAILED` (Table 55/56); moved here from Part A's former A1-5 | ADR-079 (item 13, `CLEAR_MSG_FILTER`; item 12 doesn't spell out the same reasoning for `STOP_MSG_FILTER` despite identical code); items 12-13's silent-success reporting superseded by ADR-114 | **FIXED (ADR-114)** — `STOP_MSG_FILTER`/`CLEAR_MSG_FILTER` now return `PDU_ERR_FCT_FAILED` when any underlying native stop call fails, naming the affected `FilterNumber`(s); ADR-079's best-effort iteration and retry-tracking (a failed stop's id stays in `client_filters`) are unchanged |
| B22 | Resource ID validation | `GetResourceStatus`/`CreateComLogicalLink` never reject invalid/nonexistent `ResourceId` values, falling back to unvalidated `ChannelProtocol::from_raw` (Tables 16/17/35 require `PDU_ERR_INVALID_PARAMETERS`); moved here from Part A's former A2-4. `GetConflictingResources` no longer shares this behavior as of ADR-106: an unmapped `ResourceId` yields an empty conflict list, not a `ChannelProtocol::from_raw` fallback match (a raw protocol value carries no pin/controller metadata to compute a static conflict from) | ADR-069 (ADR-106 for `GetConflictingResources`) | Accept the documented two-step/legacy-fallback resolution design for `GetResourceStatus`/`CreateComLogicalLink` (explicit backward-compatibility decision, asserted by `tests/grpc_mock/resources.rs`); **consider** whether a literal `PDU_ID_UNDEF` (`0xFFFFFFFE`) or other clearly-undefined value should still be rejected rather than silently accepted |
| B23 | Cable detection | `GET_CABLE_ID` rejects with `PDU_ERR_CABLE_UNKNOWN`; Table 53 (line 3497) reserves that code for "detection ran, cable unrecognized," assigning `PDU_ERR_FCT_FAILED` to "doesn't support cable detection at all"; moved here from Part A's former A2-20 | ADR-079 (item 15 superseded by ADR-135, then ADR-187) | **FIXED (ADR-187)** — `GET_CABLE_ID` now rejects with `PDU_ERR_ID_NOT_SUPPORTED` (ADR-135's Table-53-based `PDU_ERR_FCT_FAILED` choice did not survive the 2022-edition re-check); ADR-079's underlying rejection decision (no hardware capability) is unchanged |
| B24 | Buffer truncation | `SET_BUFFER_SIZE` silently truncates oversized `GetComPrimitiveData` results to `result_buffer_limit` instead of erroring; moved here from Part A's former A2-22 (both halves) | ADR-079 | Accept the truncation design (item 10) — explicit, reasoned. The "accepts `0`" half is **unconfirmed against spec** (Table 52/§9.5.11 doesn't state `MaxRxBufferSize=0` is out of range) — worth considering as a robustness improvement, not a cited spec violation |
| B25 | ComParam | `CP_P2Star` has no preset-seeded default (`GetComParam` returns `0`, runtime fallback `RcHandlingConfig` uses 5000 ms); moved here from Part A's former A2-16 P2Star bullet | ADR-102 | Accept — explicit removal of the former ADR-056 preset-sync step in favor of the runtime fallback |
| B26 | COP status | `GetStatus` reports a freshly-queued COP as `PDU_COPST_WAITING` instead of `PDU_COPST_IDLE` (D.1.4, line 6272/6284); moved here from Part A's former A2-2 | ADR-021 (superseded by ADR-117) | **FIXED (ADR-117)** — confirmed ADR-021's D.1.4 citation misread the spec; `J2534Service::primitives`'s value type now carries a `CopEntry { cll_handle, dispatched }` (dispatch state folded directly into the existing per-COP entry, not a separate side-set) to distinguish a never-dispatched COP (`PDU_COPST_IDLE`) from a cyclic COP resting between send cycles (`PDU_COPST_WAITING`) in `GetStatus`; ADR-021 is formally superseded. See A2-24 for the related SubscribeEvent notification-pairing work. |

---

## Recommended next steps

1. This item's original guidance (work Part A **P1** items first, since they're the
   ones most likely to break a real, spec-conformant D-PDU client) no longer applies
   once Part A has no open work — see Document Control's resolved-items list, and
   each Part A finding's own heading, for whether that's the case and for each
   finding's disposition; not restated here (see item 5 for Part B).
2. A2-10 (`TxFlagRaw` layout ambiguity) illustrates this note's own guidance: the product
   decision (ISO 22900-2 D.2.1 layout, cross-checked against `iso22900-service`'s
   treatment of the same shared proto field) was obtained before any code change. See
   Document Control's resolved-items list for its current disposition.
3. Any fix here likely touches one or more of:
   `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`,
   `j2534-0404-service/docs/implementation-notes.md`, `docs/adr/INDEX.md` (new ADR for
   anything that changes an externally-observable contract).
4. Part A "verify against 2022" flags: see A3-3, A2-16, and A3-6's own records above
   for current status (their Document Control roster lines carry no verification detail
   of their own — ADR-163) — not restated here. Unrelated
   "verify against 2022" residuals existed outside Part A — `docs/adr/ADR-125-rc2x-
   request-time-p3min-floor.md`'s J1850 `CP_P3Min` scoping question and `docs/adr/
   ADR-135-get-cable-id-rejects-with-fct-failed.md`'s `GET_CABLE_ID`/Table 53
   residual — neither a Part A flag this audit tracks nor checked by this closure at
   the time it was written. Both have since been checked against the 2022 edition
   (2026-08-11, see each ADR's own record for the outcome): ADR-125 confirmed
   unchanged; ADR-135's premise did not survive the 2022 renumbering, and the
   follow-up decision this triggered was made and resolved by `docs/adr/
   ADR-187-get-cable-id-rejects-with-id-not-supported.md`.
5. Part B items are informational unless explicitly re-opened — most represent the ceiling
   of what J2534 v04.04 hardware can express, not gaps in this codebase's effort.

## Addendum: 2022-edition delta audit (2026-08-11)

This audit's Part A/Part B findings were derived against the ISO 22900-2:2009(E) edition only
(see Scope and Methodology above). On 2026-08-11, with the 2022(en) edition now available in
`vehicle-comm-specs`, a follow-up delta audit ran six parallel design-advisor passes — one per
the same six areas Part A used above — comparing the 2022 edition's normative text against this
subsystem's current implementation, instructed to skip anything already tracked and report only
genuinely new 2022-specific findings.

Eleven new findings resulted (two P2 flagged as needing their own design-advisor consult and
possible ADR — a `CP_CyclicRespTimeout` scope question in ComPrimitive execution, and a J1850
`UniqueRespIdTable` class-list contradiction in the ComParam system — plus one P2 IOCTL gap,
one P2 `PduError` enum gap, and seven P3 items, one of which is purely informational: the removed
2009 Annex A.1 J2534-mapping annex has no 2022 counterpart, requiring no code change), and one
previously-open backlog item (the ADR-114 Table 55/56 2009-vs-2022 comparison) was resolved with
no change needed. Per this document's own convention (Document Control, above), the durable
record for all twelve items is the Prioritized
Backlog (search "2022-edition delta audit") rather than new Part A/B entries here — this addendum
is a pointer, not a duplicate source of truth, so it is intentionally excluded from
`scripts/ci-checks.sh audit-doc-check`'s Part A/B roster verification (ADR-163).

A full six-area 2022 delta audit like this one had not been run before this date; the areas
above were spot-checked only for previously-flagged 2009 residuals (A3-3, A2-16, A3-6, and the
two external ADR-125/ADR-135 residuals noted in "Recommended next steps" above), not swept for
genuinely new 2022-only content. Given how much larger the 2022 edition is than 2009's (roughly
17,000 lines vs. 9,700), a residual risk remains that content outside what these six agents
specifically compared (each was scoped to its area's core clauses, not an exhaustive line-by-line
diff) could hide further findings — treat this addendum as a substantial pass, not an exhaustive
one.
