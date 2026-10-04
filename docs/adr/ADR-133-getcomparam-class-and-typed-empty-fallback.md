# ADR-133: `GetComParam` Reports Real `com_param_class` (BUSTYPE/TESTER_PRESENT Only) and a Typed Empty Shape for Unseeded Bytefield/Structfield Params

**Date:** 2026-07-26
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/comparam_support.rs`, `j2534-0404-service/src/service/comparam_defaults.rs`, `j2534-0404-service/tests/grpc_mock/locks_and_param_classes.rs`, `docs/rpc-api-guide.md`, `j2534-0404-service/docs/implementation-notes.md`, `j2534-0404-service/docs/comparam-protocol-support.md`, `j2534-0404-service/docs/iso22900-2-conformance-audit.md`

## Context

Conformance-audit findings A2-17 and A2-18 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`):

- **A2-17:** `rpc_get_com_param` always reported `com_param_class = PDU_PC_SPECIFIED`
  (0), never the ComParam's real ISO 22900-2 Annex B.3.2 class.
- **A2-18:** `rpc_get_com_param` returned `Unum32(0)` for any Bytefield- or
  Structfield-typed ComParam that had no seeded default for the connected
  protocol, instead of the correct empty-typed shape (an existing, narrower
  instance of this same defect for `CP_CanBaudrateRecord` was already fixed
  by ADR-130).

**A2-17 scope.** The real `E_PDU_PC` enum (ISO 22900-2:2009(E) Table B.6,
`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5337-5359`)
defines seven classes (TIMING/INIT/COM/ERRHDL/BUSTYPE/UNIQUE_ID/TESTER_PRESENT);
the proto's `PDU_PC_SPECIFIED = 0` has no ISO counterpart at all (a proto3
required zero value). Only three classes have a checkable spec citation:

- `PDU_PC_UNIQUE_ID` is provably unreachable from this RPC: `is_unique_id_param`
  already rejects any such param in `check_param_allowed` before a class
  would ever be assigned (ISO 22900-2 §9.3.3.6, ADR-042).
- `PDU_PC_BUSTYPE` and `PDU_PC_TESTER_PRESENT` are both already tracked by
  existing service data (`BUSTYPE_UNUM32`/`BUSTYPE_BYTES`, ADR-067/110; and
  the `CP_TesterPresentxxx` naming rule stated directly in Table B.6).

The only in-workspace source that could classify the remaining ~80 params
(TIMING/INIT/COM/ERRHDL) is Tables B.10/B.11's PARAM-CLASS column
(`...md:5605-5714`), but that conversion is flagged unreliable by the
document's own note and is internally inconsistent where spot-checked
(e.g. `CP_CanRespUUDTId` listed TIMING beside `CP_CanRespUSDTId` listed
UNIQUE_ID; `CP_TesterPresentExpPosResp` listed COM while
`CP_TesterPresentExpNegResp` is listed TESTER_PRESENT). Per this audit's own
"independently verified, not a suspicion list" methodology, no full
classification can currently be verified from available sources.

**A2-18 scope.** The full set of Bytefield-typed (9) and Structfield-typed
(2) ComParams this service supports is already exhaustively enumerable from
`rpc_set_com_param`'s own match arms (any other `param_id` there is
rejected with `unimplemented`), so no new type registry needed to be
invented — only shared between the Get and Set sides so they cannot drift.

## Decision

**A2-17:** `rpc_get_com_param` reports `PDU_PC_BUSTYPE` for any param in the
existing `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` union, `PDU_PC_TESTER_PRESENT` for
the 10 `CP_TesterPresentxxx` params (including `CP_TesterPresentImmed`, see
below), and `PDU_PC_SPECIFIED` (0, unchanged wire value) for everything else
— documented as "unclassified/not reported", not a false claim of the real
ISO 0-class (there is none). This is upgradeable once a citable ISO
22900-2:2022 text is available in-workspace.

**A2-17, sub-decision — "class report follows enforcement" invariant
(`CP_Parity`).** `is_bustype_param` reuses `BUSTYPE_UNUM32`/`BUSTYPE_BYTES`
directly rather than a separate class-only list, even though that list is
documented as keyed by *physical hardware effect*, not strictly by ISO
`PDU_PC_BUSTYPE` label membership (see its own doc comment, added by
ADR-110's amendment for `CP_Parity` specifically: `CP_Parity` has no
distinct D-PDU `CP_*` name of its own — D-PDU folds parity into
`CP_UartConfig`/`DATA_BITS` — and was added to the list on a hardware-effect
basis). Reuse is correct anyway, by a different argument than "every member
happens to also be ISO-labeled": every member of this list *receives* full
BUSTYPE enforcement by construction — `apply_bustype_lock`/`strip_bustype_keys`
lock it out under another CLL's physical-ComParam lock, and
`bustype_params_differ` rejects staging a difference for it via
`temp_param_update` (ADR-067 §E, confirmed spec-correct per ISO 22900-2
§9.4.16.2.1 c) NOTE 2 by ADR-110's own amendment note). A client relying on
`com_param_class` to predict `GetComParam`/`SetComParam`/lock behavior would
be actively misled by `PDU_PC_SPECIFIED` ("no class restrictions known") for
`CP_Parity`, since it then immediately hits the same
`PDU_ERR_EVT_RSC_LOCKED`/`PDU_ERR_TEMPPARAM_NOT_ALLOWED` handling as every
other BUSTYPE param. Reporting `PDU_PC_BUSTYPE` is therefore the
behavior-accurate answer regardless of `CP_Parity`'s ISO-naming status. This
generalizes: if a future param is ever added to `BUSTYPE_UNUM32`/
`BUSTYPE_BYTES` for hardware-effect reasons whose *intended* class handling
is not actually BUSTYPE, the bug to fix at that point is the enforcement
list itself, not this reporting function's reuse of it.

**A2-17, sub-decision — `CP_TesterPresentImmed` classification.**
`PARAM_TESTER_PRESENT_IMMED` (0x80B1) is included in
`TESTER_PRESENT_CLASS_PARAMS` on two grounds: (1) ISO 22900-2 explicitly
permits supplier-specific ComParams (spec lines 5328/5557), and Table B.6's
TESTER_PRESENT definition is name/function-scoped (it applies to the
tester-present family of ComParams, `CP_TesterPresentxxx`, line 5359), not restricted to the
nine standardized members — `CP_TesterPresentImmed`'s semantics (toggling
immediate-send-at-`CoptStartcomm` behavior) are purely tester-present
handling; (2) this repo's own `comparam-protocol-support.md` groups it under
an ID-range/storage-layer heading ("Protocol-Layer Service Params"), which
organizes by *where the value lives*, not by ODX class — that grouping is
not counter-evidence for its ISO class. Classifying it does **not** depend
on whether it is reachable via any protocol's allowlist today (it is not —
see the residual below); this is a classification-correctness question, and
it also keeps the class list consistent with the deferred TESTER_PRESENT
temp-update guard residual below, which should key off this same list once
implemented.

**A2-18:** `rpc_get_com_param`'s fallback chain, before defaulting to
`Unum32(0)`, now checks whether `param_id` is one of the 9 Bytefield-typed
or 2 Structfield-typed params this service declares (via
`comparam_support::BYTEFIELD_PARAMS`/`STRUCTFIELD_PARAMS`, the same lists
`rpc_set_com_param` validates writes against) and reports the correct empty
shape instead: an empty `Bytefield`, or — for the two Structfield params —
`CP_SessionTimingOverride` → `SessionTiming { entries: [] }` (reusing
`session_timing_empty()`, matching Table B.19's own default text) and
`CP_ExtendedTiming` → a **new**, distinct `AccessTiming { entries: [] }`
shape, not the existing `access_timing_zero()` (which returns one
all-zero-valued entry). `access_timing_zero()` is a genuine *seed default*
for the 4 KWP presets that call it — a meaningful value a client could act
on — which is exactly the kind of not-actually-configured-but-looks-like-a-
value response this fix exists to stop returning, one level up, for a
protocol where nothing was ever seeded at all.

`rpc_set_com_param`'s existing `Unum32`-branch type-mismatch rejection
(previously scoped to `is_bustype_bytes_param` only, per ADR-130's
Amendment 2) is widened to cover every Bytefield- and Structfield-typed
param. Without this, the new fallback branches above would introduce the
identical write-masking bug ADR-130 already fixed once: a client that wrote
`SetComParam(param, Unum32(v))` against, say, `CP_TesterPresentMessage`
would previously get `v` echoed back (type-blind but self-consistent);
after adding the Bytefield/Structfield fallback branches, that same write
would instead be silently discarded behind an empty-typed `GetComParam`
response. Rejecting the mismatched write at the source closes this for all
10 newly-covered params the same way ADR-130 closed it for
`CP_CanBaudrateRecord`.

## Alternatives Considered

1. **Classify every ComParam into TIMING/INIT/COM/ERRHDL/BUSTYPE/UNIQUE_ID/
   TESTER_PRESENT (full A2-17 fix).** Rejected: the only per-param class
   source available in this workspace (Tables B.10/B.11) is unreliable by
   its own conversion note and demonstrably self-contradictory; ~80 new
   unverifiable judgment calls is exactly what this audit's methodology
   exists to avoid. Revisit once ISO 22900-2:2022 is available in-workspace
   (`docs/j2534-0404-architecture.md` already targets that edition).
2. **Leave `PDU_PC_SPECIFIED` reporting for the RC21/RC23/RC78 family as
   `PDU_PC_ERRHDL`, since Table B.6's ERRHDL description almost names it
   directly.** Rejected: drawing a "these specific rows are trustworthy"
   line inside a table already proven corrupted elsewhere is the same
   unverifiable judgment call as (1), just narrower — no different in kind.
3. **Exclude `CP_Parity` from `is_bustype_param`, falling back to
   `PDU_PC_SPECIFIED` for it, to keep the hardware-effect-keyed
   `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` list untouched by an ISO-label-keyed
   consumer.** Rejected: this would report "no class restrictions known"
   for a param this same service actively locks/temp-update-rejects as
   BUSTYPE — a class-report/behavior mismatch, exactly what A2-17 exists to
   eliminate, not a safer default.
4. **Exclude `CP_TesterPresentImmed` from `TESTER_PRESENT_CLASS_PARAMS`
   since it is unreachable via any protocol's allowlist today.**
   Rejected: reachability and classification are independent questions; ISO
   22900-2 permits supplier-specific ComParams, and this param's function is
   unambiguously tester-present regardless of whether the separate,
   pre-existing allowlist gap (see Consequences) is ever fixed.
5. **Reuse `access_timing_zero()`'s one-entry shape as the A2-18 fallback
   for `CP_ExtendedTiming` too, avoiding a second builder function.**
   Rejected: Table B.19's own text for this param's default is "ParamActLen
   = 0 (not enabled)" — zero entries, not one all-zero entry; reusing the
   seed-value shape would silently report a meaningful timing-set-0 value
   for a protocol that never configured anything, the same class of bug
   A2-18 fixes elsewhere.

## Consequences

- `GetComParam` now returns a caller-typechecked-safe response for every
  Bytefield/Structfield param regardless of whether the connected
  protocol's preset ever seeded it — closes the same defect class ADR-130
  fixed for `CP_CanBaudrateRecord`, generalized to all 11 typed params.
- `com_param_class` is now accurate for the two classes with real D-PDU-API
  behavioral consequences (lock rules, TempParamUpdate prohibition); it
  remains silent (`PDU_PC_SPECIFIED`/0) for the rest rather than guessing.
- **Closed (Codex review finding on PR #147, `comparam_support.rs:703`'s
  `com_param_class` function):** ISO 22900-2 §9.4.5 f) / Table B.6 prohibit
  `TempParamUpdate` on `PDU_PC_TESTER_PRESENT`-class params, the same way
  `bustype_params_differ` (`comparam_support.rs`) already enforces it for
  `PDU_PC_BUSTYPE`. Once `GetComParam` started reporting
  `PDU_PC_TESTER_PRESENT` for `CP_TesterPresentxxx` params, nothing enforced
  the matching `temp_param_update` rejection, so a client could stage a
  TESTER_PRESENT-class Working value differing from Active and start a COP
  with `temp_param_update=1` unrejected. Closed by
  `comparam_support::tester_present_params_differ`, keyed off
  `TESTER_PRESENT_CLASS_PARAMS`, consulted alongside `bustype_params_differ`
  in `rpc_primitive.rs`'s `temp_param_update` guard; see
  `tests/grpc_mock/locks_and_param_classes.rs::tester_present_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`.
- **Residual (backlog, `j2534-0404-service/docs/implementation-notes.md`,
  not fixed here):** the 4 KWP presets that call `access_timing_zero()` to
  seed `CP_ExtendedTiming` (`comparam_defaults.rs`) do so for a param that
  `is_kwp_param` does not actually allow through `GetComParam`/
  `SetComParam` — those seeds are dead/unreachable via RPC for those
  specific presets. Whether that's a pre-existing bug (the allowlist should
  include it) or dead seed code (the seed should be removed) needs its own
  investigation; noted here only as a residual this ADR's work surfaced,
  not resolved by it.
- **Residual (backlog, `j2534-0404-service/docs/implementation-notes.md`,
  not fixed here — pre-existing, found while classifying
  `CP_TesterPresentImmed`):** `PARAM_TESTER_PRESENT_IMMED` is a dead param
  with a three-way contradiction: `comparam-protocol-support.md` documents
  it as supported for CAN/KWP/J1850/SCI, `comparam_defaults.rs` seeds it
  into ~10 presets, but it is absent from every per-protocol allowlist in
  `comparam_support.rs`, so `GetComParam`/`SetComParam` reject it on every
  real protocol today, and nothing in this crate ever reads the seeded
  value at runtime. Safe-conservative today (a clean rejection); not fixed
  here since it is orthogonal to A2-17/A2-18's `GetComParam`-correctness
  scope. Its classification in `TESTER_PRESENT_CLASS_PARAMS` is correct
  under either backlog resolution (wire the allowlist up, or delete the
  param entirely along with its class-list entry).
- Spec-edition caveat (standing repository policy): all clause/table
  citations above are ISO 22900-2:2009(E). A 2022 revision could change
  either finding's verdict; re-verify if/when that edition becomes
  available in-workspace.

## Amendment (Codex review, PR #147, round 2): `bustype_params_differ`/`tester_present_params_differ` must compare effective, not raw, values

**Context.** Round 2 of Codex review on PR #147 found that
`tester_present_params_differ` (added by the round-1 amendment above to
close the TESTER_PRESENT `temp_param_update` residual) compares `Working`
vs. `Active` `ComParamSet` maps by raw `Option` presence
(`working.unum32.get(&id) != active.unum32.get(&id)`, similarly for
`.bytes`). But `rpc_get_com_param` reports a *default* value for an
unseeded param (`0` for Unum32, empty `Bytefield`/`Structfield`) rather than
"absent". A client that reads that default via `GetComParam` and writes it
straight back via `SetComParam` — a completely normal save/restore
round-trip — ends up with `Working = Some(default)` while `Active` stays
`None` for that id. The differ functions saw this as a real change and
permanently rejected every subsequent `temp_param_update` COP with
`PDU_ERR_TEMPPARAM_NOT_ALLOWED`, even one staging a totally unrelated
param. The identical shape was found live today in the pre-existing
`bustype_params_differ` too, not just the just-added TESTER_PRESENT analog:

- `CP_CANFDBaudrate`/`CP_CANFDBitSamplePoint`/`CP_CANFDSyncJumpWidth` are
  allowed for the entire CAN family (`is_can_param`) but seeded only in
  CAN-FD presets — confirmed unseeded on plain-CAN/29-bit presets by
  existing `comparam_defaults.rs` tests (`assert!(!p.unum32.contains_key(&PARAM_CANFD_BAUDRATE))`).
  A plain-CAN client round-tripping `CP_CANFDBaudrate` hit this via
  `bustype_params_differ`.
- `CP_Parity` has a *derived, non-zero* effective value on a KWP link with
  `CP_UartConfig` set (e.g. 8E1 → derived parity `2`, ADR-071) — round-
  tripping it hit the same bug with a non-zero false-positive value.
- `CP_TesterPresentMessage` (Bytefield) and `CP_TesterPresentHandling`
  (Unum32) both hit it via `tester_present_params_differ`, on any protocol
  where they are allowed but unseeded (e.g. raw CAN).

**Fix.** Root cause: the two differ functions compared raw map presence
instead of the *effective, `GetComParam`-observable* value. Two shared
helpers, `comparam_support::effective_unum32`/`effective_bytes`, now
compute that effective value once — `effective_unum32` mirrors
`rpc_get_com_param`'s Unum32 fallback exactly (an explicit entry if
present, else the ADR-071 `CP_UartConfig`-derived parity for `PARITY`
specifically, else `0`); `effective_bytes` mirrors the Bytefield fallback
(an explicit entry if present, else empty). `bustype_params_differ` and
`tester_present_params_differ` now compare via these helpers instead of raw
map lookups, and `rpc_get_com_param`'s own Unum32 fallback was refactored
to call `effective_unum32` too (a pure extraction, not a behavior change),
so there is now exactly one place that knows "what does an unseeded/derived
param report" — the fallback logic and the differ comparison can no longer
independently drift, which is literally how this bug happened (two
independent copies of the same fact).

**Invariant for future readers:** class-differ guards
(`bustype_params_differ`, `tester_present_params_differ`, and any future
guard of this shape) compare *effective, `GetComParam`-observable* values
via a single shared helper — never raw map presence. ISO 22900-2
§9.4.16.2.1 c) NOTE 2: a value the service itself already reports as
current (via `GetComParam`) is not a rejectable "change" the
`temp_param_update` prohibition needs to guard against.

**ADR-130 Amendment 3's write-side normalization.** `rpc_set_com_param`'s
Bytefield-arm normalization (empty `CP_CanBaudrateRecord` write ⇒ remove
the Working entry rather than storing an explicit empty one) is retained
unchanged, but is no longer load-bearing for `bustype_params_differ`
specifically — `effective_bytes` now treats an explicit empty entry and an
absent one as equal there either way. It remains load-bearing for
`apply_bustype_lock`'s `hw_set` push-path computation, where an explicit
empty entry is still a distinct map key.

**Accepted residual.** If a future differ function ever covers
`STRUCTFIELD_PARAMS`, the two Structfield empty shapes
(`session_timing_empty()`/`access_timing_empty()`) are per-param and would
need to be sourced from the same builders `rpc_get_com_param` uses — the
same single-source-of-truth rule this amendment establishes for
Unum32/Bytefield. Not yet needed: no differ function covers Structfield
params today.

See `j2534-0404-service/src/service/comparam_support.rs`'s
`effective_unum32`/`effective_bytes` and their test coverage
(`bustype_params_differ_false_on_unum32_zero_roundtrip_for_unseeded_param`,
`bustype_params_differ_false_on_derived_parity_roundtrip_but_true_on_actual_uart_config_change`,
`tester_present_params_differ_false_on_unum32_zero_roundtrip_for_unseeded_param`)
and `j2534-0404-service/tests/grpc_mock/locks_and_param_classes.rs::tester_present_bytefield_empty_roundtrip_does_not_block_temp_param_update`.

## Amendment (Codex review, PR #147, round 3): declined finding — the differ gate is not the BUSTYPE hardware-push safety boundary

**Context.** Round 3 of Codex review raised a P1 concern against round 2's
fix: that letting `bustype_params_differ` under-trigger on an
effective-equal round-trip (e.g. `GetComParam(CP_BitSamplePoint)` on an
unseeded, bare-`protocol_id`-only CLL reports `0`; `SetComParam(..., 0)`
writes an explicit `Working` entry) would let a `temp_param_update` COP
proceed and push that explicit entry to hardware via
`apply_params_to_hardware_locked`, violating ISO 22900-2's BUSTYPE
`temp_param_update` prohibition.

**Declined — verified by two independent traces (Claude session +
design-advisor) to be a false positive.** The `temp_param_update` call-time
differ check is a rejection *courtesy* (an early, spec-required error
signal), not the actual safety boundary against a BUSTYPE hardware push.
That boundary is `comparam_support::strip_bustype_keys`, applied
**unconditionally** — independent of the differ, independent of whether
anything "changed" — at every real hardware-apply site a `temp_param_update`
COP can reach:

- `events.rs:4191` (`handle_send_recv`'s per-cycle apply) and `events.rs:6008`
  (the StartComm-family temp-init apply) both shadow `ParamBinding::Temp`'s
  `effective` with `strip_bustype_keys(effective)` before ever calling
  `apply_params_to_hardware`.
- `events.rs:7659-7660` (the temp bracket's revert-to-Active) does the same.
- An exhaustive inventory of this crate's only two `PassThruIoctl
  SET_CONFIG` call sites confirms there is no third route: the
  `apply_bustype_lock`-consuming site (`events.rs:7898`) belongs to
  `CoptUpdateparam`, a different COP where pushing an unlocked BUSTYPE
  change to hardware is the spec-intended Working→Active promotion, not a
  `temp_param_update` path at all; the other (`rpc_link.rs:608`) is
  `ConnectComLogicalLink`'s channel-setup apply, unreachable from any temp
  bracket.

So round 2's fix changed only the rejection gate; the strip that actually
keeps BUSTYPE keys off the wire was untouched and remains unconditional.
Net effect of round 2's fix on the reported scenario: before, a spurious
`PDU_ERR_TEMPPARAM_NOT_ALLOWED`; after, the COP proceeds and pushes zero
BUSTYPE keys — bitwise-identical hardware interaction to a CLL that never
round-tripped the unseeded default.

**Reply posted on the PR** with this trace rather than silently resolving
the thread — see `finding-routing.md`'s 2c convention.

**Adjacent, pre-existing, NOT fixed by this PR:** `CoptUpdateparam` itself
has no differ-style gate at all — it never checked `bustype_params_differ`
before this PR and still doesn't. A client that blind round-trips an
unseeded BUSTYPE param (Working gains an explicit `Some(0)`) and then
issues `CoptUpdateparam` while unlocked *will* push that explicit `0` to
hardware via `apply_bustype_lock`'s unfiltered `hw_set`. This predates PR
#147 entirely (root cause: `GetComParam` reporting `0` for an unseeded
BUSTYPE Unum32 param, not anything this PR's differ fix touches) and is
tracked as a backlog item in
`j2534-0404-service/docs/implementation-notes.md` rather than fixed here.
