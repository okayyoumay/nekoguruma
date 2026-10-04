# ADR-138: `CP_TesterPresentAddrMode` Selects Tester-Present Addressing via an `AddrModeSource` Selector in `tx_header`

**Date:** 2026-07-27
**Status:** Accepted
**Affects:** `j2534-0404-service` tx_header, rpc_primitive, comparam_defaults

## Context

`CP_TesterPresentAddrMode` (`PARAM_TESTER_PRESENT_ADDR_MODE`, id `0x8003`, `service_params.rs:31`)
has been a fully registered D-PDU ComParam since it was added: it is `Get`/`SetComParam`-reachable
on CAN/ISO15765, KWP, and J1850 (`comparam_support.rs`'s per-family allowlists; not SCI), named in
`names.rs`, seeded with a per-protocol default in every `comparam_defaults.rs` preset, and a member
of `TESTER_PRESENT_CLASS_PARAMS` (`comparam_support.rs:672`, consumed by `tester_present_params_differ`,
`rpc_primitive.rs:870`, so a live promotion already triggers re-resolution). But no runtime code ever
read its *value* — the same gap class ADR-137 closed for `CP_TesterPresentHandling`: unlike every
sibling tester-present accessor, there was no `tester_present_addr_mode` consumer, and
`resolve_tester_present` (`rpc_primitive.rs:477-642`) never consulted it.

ISO 22900-2:2009(E) (the only edition available in this workspace; a 2022-edition revision to this
clause's legend has not been checked) Table B.20 (`.../ISO_22900-2_2009(E)-...md:5952`) describes it as
the addressing mode applied to the periodically sent Tester Present messages. Type `PDU_PT_UNUM32`, `Value:
[0;1]`. Per-protocol defaults: `ISO_15765_4=0, ISO_14230_4=0, ISO_9141_2=0, SAE_J1850_VPW=0,
SAE_J1850_PWM=0, ISO_14230_3=0, ISO_15765_3=1`. Unlike `CP_RequestAddrMode`'s own table row (`:6044`,
"1=Physical, 2=Functional"), this row has no explicit legend — but `1`=functional is corroborated by
the default pattern: `ISO_15765_3=1` pairs with that identity's default `CP_TesterPresentMessage
{0x3E, 0x80}` and `CP_TesterPresentReqRsp=0` — the canonical UDS *functional*, suppress-response
TesterPresent — while `ISO_14230_4=0` pairs with `CP_TesterPresentReqRsp=1` and an expected `{0x7E}`
response, i.e. a response-expecting TesterPresent sent physically.

Design-advisor was consulted (this repo's `.claude/README.md` cost-policy rule 3 gate: ISO 22900-2
spec interpretation of an unlabeled legend, plus a cross-cutting signature change in `tx_header.rs`,
an area with heavy prior ADR precedent — ADR-050, ADR-054, ADR-055, ADR-062, ADR-083, ADR-084,
ADR-088, ADR-137 all shape this exact subsystem).

Before this ADR, tester-present's outgoing message addressing (CAN ID / KWP / J1850 header target and
format) was built by `resolve_tester_present` (`rpc_primitive.rs:514,528`) calling
`tx_header::resolve_can_addressing`/`build_tx_message`, both of which derive functional-vs-physical
from a single private helper, `use_functional_addressing(active)` (`tx_header.rs:47`), reading
`CP_RequestAddrMode` (value `2` = functional, ADR-054) — the *general* request-addressing ComParam,
shared with `CoptSendrecv`. Tester-present therefore silently inherited whatever `CoptSendrecv`'s own
addressing was configured to, instead of having its own, spec-intended, independent addressing
decision. This is not merely a missing edge case: `ISO_15765_4` defaults `CP_RequestAddrMode` to
functional (`2`, ADR-054's own default-pattern rationale) while `CP_TesterPresentAddrMode` defaults to
physical (`0`) for that same identity — the spec deliberately decouples the two, and this service
collapsed them.

## Decision

### Value semantics

`== 1` selects functional; `0`, absent, or any out-of-range value defaults to physical. This follows
the same-family precedent `CP_TesterPresentHandling` established (ADR-137): a boolean-gate role
(`handling == 1`, `tester_present_req_rsp() == 1`), not `CP_TesterPresentSendType`'s validated-enum
role (which `resolve_tester_present` rejects outright for `!= 0 && != 1`). No `SetComParam`-time
validation is added — consistent with `CP_RequestAddrMode`'s own precedent (ADR-054: any value other
than the functional sentinel defaults to physical, no rejection) and with the coercing-decode
convention this whole ComParam family already uses.

**Accepted residual:** a client who sets `2` (confusing this ComParam's functional sentinel with
`CP_RequestAddrMode`'s) silently gets physical — identical in shape to ADR-137's accepted out-of-range
coercion.

### Threading mechanism: an explicit `AddrModeSource` selector, not a caller-computed bool

`tx_header.rs` gains:

```rust
enum AddrModeSource {
    /// `CP_RequestAddrMode` (ADR-054): functional iff `== Some(2)`.
    Request,
    /// `CP_TesterPresentAddrMode` (this ADR): functional iff `== Some(1)`.
    TesterPresent,
}
```

`use_functional_addressing` becomes `fn use_functional_addressing(source: AddrModeSource, active:
&ComParamSet) -> bool`, matching on the variant to read the correct ComParam with the correct
sentinel. `resolve_can_addressing`, `build_tx_message`, `kwp_header_bytes`, and `j1850_header_bytes`
all gain an `addr_source: AddrModeSource` parameter, threaded through to their own
`use_functional_addressing` call.

Rejected alternatives:

- **A caller-computed `functional: bool` parameter.** Loses the source identity `build_tx_message`'s
  error branch needs (see below) and scatters the two differing decode sentinels (`2` vs `1`) across
  call sites instead of keeping the decode rule in one place next to its module doc.
- **A second, tester-present-specific entry point duplicating the CAN/KWP/J1850 dispatch.** This file's
  own history (ADR-137's round-4 restructure, elsewhere in this subsystem) is exactly the lesson that
  duplicated per-caller logic drifts; the enum-parameter approach keeps one dispatch, forced to declare
  its source at every call site by the type checker.
- **Inheriting from `CP_RequestAddrMode` when `CP_TesterPresentAddrMode` is absent.** Defeats the
  ComParam's spec-intended independence — the whole point of this ADR.

`build_tx_message` has exactly three call sites in the crate; `resolve_can_addressing` has two more of
its own. Only `resolve_tester_present`'s two calls (`rpc_primitive.rs:514`, `:528`) pass
`TesterPresent`; every other call site — `resolve_send_recv_tx`'s two calls (`:145`, `:161`,
`CoptSendrecv`), `resolve_init_tx_flags` (`:657`, init TX flags), and the K-line fast-init optional
request frame inside `rpc_start_com_primitive` (`:1274`, `CoptStartcomm`'s own init-sequence byte, not
tester-present) — passes `Request`, unchanged from today's behavior.

### Error message names the driving ComParam

`build_tx_message`'s existing missing-functional-ComParam error (`tx_header.rs:305-315`,
`"sending functionally on this protocol requires CP_CanFuncReqId to be set"`) is extended to name
whichever ComParam actually selected functional addressing for that call, via the same `match` on
`addr_source` — e.g. `"functional addressing (selected by CP_TesterPresentAddrMode = 1) requires
CP_CanFuncReqId to be set"` for the `TesterPresent` branch, existing `CP_RequestAddrMode` phrasing for
`Request`. Both branches point at the same underlying functional-addressing ComParam family
(`CP_CanFuncReqId`/`CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`) — there is no separate,
tester-present-specific functional-address ComParam in the spec — but without naming the source, a
client whose `CP_RequestAddrMode` is physical would be told they're "sending functionally" with no
visible cause when only `CP_TesterPresentAddrMode` was actually `1`.

### Downstream fields: `can_functional`/`target_can_ids` need no independent change; `same_wire_behavior` does

`ResolvedTesterPresent::can_functional`, `target_can_ids` (ADR-088), and `same_wire_behavior`'s
existing `can_functional` comparison (ADR-084's re-arm gate) all derive from
`resolve_can_addressing`'s return value at `rpc_primitive.rs:514,517,600`; once addressing resolution
itself reads the correct ComParam, they automatically reflect `CP_TesterPresentAddrMode` with no
further change — and the CAN-family P3Func/P3Phys gap-timing bucket (`events.rs`, which reads
`tester_present.can_functional`) becomes more correct as a side effect: it now matches
tester-present's own addressing instead of `CoptSendrecv`'s.

**Correction (Codex review, PR #158):** the first version of this ADR argued `CP_TesterPresentAddrMode`
itself did not need to join `same_wire_behavior`'s comparison set, reasoning that a live flip always
changes `data` (a different CAN ID or KWP/J1850 header) or, on the CAN family, `can_functional` — and
that the one corner where neither changes (physical and functional ComParams configured to produce
byte-identical KWP/J1850 headers) was "genuinely the same wire behavior," so no re-arm was correct
there. Codex correctly identified this as conflating *instance-level byte coincidence* with *the
ComParam not affecting wire content* — those are not the same thing. `can_functional` is populated
only for CAN-family protocols (`protocol.is_can_family()`-gated in `resolve_tester_present`), so for
KWP/J1850 it is always `None` regardless of `CP_TesterPresentAddrMode`; a client who explicitly
configures `CP_PhysReqFormatPriorityType == CP_FuncReqFormatPriorityType` and matching target
addresses (both diverge from every seeded default, but nothing prevents an explicit `SetComParam`)
reaches exactly the byte-identical-headers state, and a subsequent live `CP_TesterPresentAddrMode`
flip then changes neither `data` nor `can_functional` — so `same_wire_behavior` reports equality and
`handle_update_param` skips the immediate resend NOTE 1 requires. NOTE 1 (a change to any of the
tester present ComParams makes the tester present message go out right away; see ADR-137
Context) ties the send trigger to the ComParam *value* changing, not to whether the resulting bytes
happen to coincide for one particular configuration — and unlike `CP_TesterPresentReqRsp`/
`ExpPosResp`/`ExpNegResp` (ADR-088's deliberate exclusions, which are RX-classification-only and never
affect wire content), `CP_TesterPresentAddrMode` is categorically a TX-addressing selector.

**Corrected decision:** `ResolvedTesterPresent` gains `addr_mode_functional: bool` — the decoded
`CP_TesterPresentAddrMode` functional/physical reading (`tx_header::use_functional_addressing(
AddrModeSource::TesterPresent, active)`, which becomes `pub(super)` for this to compile), populated
truthfully in **both** branches of `resolve_tester_present` including the `handling_enabled == false`
early return (the lookup is infallible — it's a `HashMap` read, not a fallible resolution step — so
populating it there does not reopen ADR-137's "master switch checked before any fallible resolution"
ordering rule, and avoids ever leaving a vacuous placeholder that a later comparison could act on
incorrectly). Added to `same_wire_behavior`'s comparison set alongside `send_type`/`can_functional`:
any `CP_TesterPresentAddrMode` value change that decodes to a different functional/physical reading
now re-arms unconditionally, independent of whether `can_functional`/`data` happen to also differ.
Comparing the **decoded bool**, not the raw `Option<u32>` value, matches how every other field in this
comparison already operates at resolved-value granularity (e.g. `CP_TesterPresentTime` compares
post-`us_to_ms` `interval_ms`, not the raw microsecond value) — a raw ComParam change that decodes to
the same reading (e.g. `0` → `5`, both physical) does not re-arm, consistent with that existing
convention, while any `0`/absent ↔ `1` transition always does.

**Accepted residual:** a raw `CP_TesterPresentAddrMode` write that changes the stored value but not
its decoded functional/physical reading (e.g. `2` → `3`, both physical) does not trigger a re-arm —
the same resolved-value-granularity behavior every other field in `same_wire_behavior`'s comparison
already has, not a new gap this correction introduces.

ISO 15765-2's functional-single-frame requirement (ADR-055) needs no new enforcement for
tester-present: the arm-time framer already treats a tester-present payload that doesn't fit one
SingleFrame as a soft arm failure regardless of addressing (`events.rs`), independent of this change.

### `comparam_defaults.rs`: four presets corrected to match the spec table

Checked every preset's seeded `PARAM_TESTER_PRESENT_ADDR_MODE` value against Table B.20's per-protocol
defaults above. Four seed `1` (functional) where the spec defines `0` (physical) for that identity —
apparently copied from those same presets' own `CP_RequestAddrMode = 2`, which the spec deliberately
does *not* mirror for this ComParam:

- `iso_15031_5_on_iso_14230_4` (`comparam_defaults.rs:564`, `ISO_14230_4`, `// override common`)
- `iso_15031_5_on_iso_9141_2` (`:713`, `ISO_9141_2`)
- `iso_obd_on_k_line` (`:784`, maps to `ISO_9141_2` per Annex D.1 Table D.4, same as A2-16's own
  precedent for this preset's protocol identity)
- `iso15765_4_common` (`:899`, `ISO_15765_4`; inherited by both `iso_15031_5_on_iso_15765_4` and
  `iso_obd_on_iso_15765_4`)

All four corrected to `0`. This is not cosmetic: these K-line/ISO15765-4 OBD presets default
`CP_TesterPresentHandling = 1` (ADR-137's own defaults survey), so once this ComParam is consumed,
those links' live tester-present keep-alive would otherwise have silently started transmitting
functionally against the spec table the moment this ADR shipped.

Already-correct: `kwp_on_kline_common` (`:476`) = `0`; `iso_14230_3_on_iso_15765_2` (`:830`) = `0`;
`iso15765_3_on_iso_15765_2` (`:988`) = `1` (`ISO_15765_3 = 1`, the sole spec-functional default);
`j1850_common` (`:1218`) = `0`. The three `SAE_J2190`-based presets (`sae_j2190_on_iso_14230_2`,
`sae_j2190_on_iso_9141_2`, `sae_j2190_on_iso_15765_2`) have no corresponding row in the spec's Table
B.20 at all — kept at their existing repository-default values, unchanged, per ADR-137's identical
precedent for its own out-of-table preset (`sae_j2190_on_iso_15765_2`'s `CP_TesterPresentHandling`).

## Consequences

- Tester-present's addressing is now spec-independent of `CoptSendrecv`'s `CP_RequestAddrMode`,
  closing a real conformance gap: before this ADR, a CLL configured for functional `CoptSendrecv`
  requests (e.g. the `ISO_15765_4` OBD default) also sent its periodic/idle-triggered tester-present
  functionally even though the spec's own default for that identity is physical, and vice versa for
  any client relying on `CP_TesterPresentAddrMode`'s spec default alone.
- `resolve_can_addressing`, `build_tx_message`, `kwp_header_bytes`, and `j1850_header_bytes` gain an
  `AddrModeSource` parameter — a breaking signature change within this crate (`pub(super)`/private, not
  part of the external gRPC contract); every call site is updated in the same commit.
- Four `comparam_defaults.rs` presets' seeded `CP_TesterPresentAddrMode` values change from `1` to `0`
  — an externally observable `GetComParam` default change for `iso_15031_5_on_iso_14230_4`,
  `iso_15031_5_on_iso_9141_2`, `iso_obd_on_k_line`, and both `iso15765_4_common`-derived presets, and
  (combined with this ADR's runtime consumption) a behavior change for any client that never explicitly
  set this ComParam on one of those four resources.
- **Accepted residual — value-2 confusion:** a client that sets `CP_TesterPresentAddrMode = 2`
  (mistaking it for `CP_RequestAddrMode`'s functional sentinel) silently gets physical rather than an
  error — the same coercion shape ADR-137 already accepted for `CP_TesterPresentHandling`.
- **Accepted residual — 2022 edition legend unverified:** only the ISO 22900-2:2009(E) edition is
  available in this workspace; if the 2022 edition adds an explicit legend to this table row that
  contradicts the default-pattern-inferred `1 = functional` reading here, this ADR's Q1 decision would
  need revisiting. Flagged per CLAUDE.md's spec-reference rule rather than treated as settled.
