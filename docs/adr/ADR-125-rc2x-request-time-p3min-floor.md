# ADR-125: RC21/RC23 Request-Time Floors to CP_P3Min, Not a Hardcoded Default

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`RcHandlingConfig::from_params`),
             `j2534-0404-service/src/service/protocol.rs` (new `ChannelProtocol::is_j1850_family`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`'s
             `from_params` call site), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`CoptStartcomm`/`CoptStopcomm`'s `from_params` call sites)

## Context

`iso22900-2-conformance-audit.md`'s B11 finding (originally Part A's A2-9,
reclassified because ADR-060 already accepts the narrower "RC21/23
re-requests aren't gap-checked" deviation) carries forward one implementation
slip that ADR-060 never scoped: `RcHandlingConfig::from_params` read
`CP_RC21RequestTime`/`CP_RC23RequestTime` via the same `get_us_as_ms(id,
default_ms)` helper used for every other RC-family timing ComParam —

```rust
rc21_request_time_ms: get_us_as_ms(PARAM_RC21_REQUEST_TIME, 25),
rc23_request_time_ms: get_us_as_ms(PARAM_RC23_REQUEST_TIME, 25),
```

— where `get_us_as_ms` treats a stored `0` identically to "absent" and
substitutes `default_ms`. That convention is correct for `CP_P2Star`/
`CP_RC78CompletionTimeout` (ADR-057, ADR-102): those really do mean "no
override configured, use the runtime default" at `0`.

It is wrong for `CP_RC21RequestTime`/`CP_RC23RequestTime`. Annex I.1.4.3 of
ISO 22900-2:2009(E) (under the Annex I.1.4 heading "Additional RC23/RC21 handling
description for SAE J1850 VPW and ISO 14230 protocols") is explicit that `0` is a real,
spec-defined configured value, not an absence. Summarized: on receiving an
RC23 for an active CoP, the Protocol Handler arms a message receive timer
set to `CP_RC23RequestTime`, waiting for the ECU's positive response. For
SAE J1850 VPW that timer is 1 second; for ISO 14230-3 it is defined as `0`
— under 14230-3 there is no "final" response at all, so the tester must
re-request the service after each RC23 negative response, waiting
`CP_P3Min` before each re-transmission. Whichever of `CP_RC23RequestTime`
and `CP_P3Min` is larger governs the actual wait; for ISO 14230-3
specifically, since `CP_RC23RequestTime` is defined as `0`, that always
resolves to `CP_P3Min`, and the re-request fires as soon as `CP_P3Min`
elapses.

The same steps apply symmetrically to RC21 (§I.1.4.2's setup assumption is
stated for RC23 but the RC21 handling in the same Annex mirrors it, and this
codebase's own preset data treats the two identically — see
`comparam_defaults.rs`'s `kwp_on_kline_common`, which seeds
`CP_RC21RequestTime = CP_RC23RequestTime = 0` together for every ISO
14230/KWP preset it backs). Coercing an explicit `0` to a made-up 25 ms
literal has no basis in the spec text and silently changes the actual
inter-request wait for every K-line client that does the natural,
spec-compliant thing: enable `CP_RC21Handling`/`CP_RC23Handling` and rely on
the Table B.19-seeded `RequestTime` default rather than also setting a
`RequestTime` explicitly.

This is a live, reachable bug: `iso_15031_5_on_iso_14230_4`
(`comparam_defaults.rs`) seeds `CP_RC21Handling = 2` (enabled) with
`CP_RC21RequestTime = 0`, and (via `kwp_on_kline_common`) `CP_P3Min =
55_000` µs. Before this fix, an RC21 on that CLL re-requested after a
hardcoded 25 ms; the spec requires `Max(CP_P3Min, CP_RC21RequestTime) = 55
ms`.

`CP_P3Min` (native J2534 `P3_MIN`) is a K-line-only *concept* — `is_kwp_param`
(`comparam_support.rs`) is the only allowlist arm that includes it, so
`SetComParam(CP_P3Min, ...)`/`GetComParam(CP_P3Min)` are rejected outright
(`PDU_ERR_COMPARAM_NOT_SUPPORTED`) on any CAN-family CLL — a client can never
read or write `CP_P3Min` there. This is *not* the same claim as "no CAN
preset's `ComParamSet` ever contains a `P3_MIN` entry": resource
`ISO_14230_3_on_ISO_15765_2` (0x0204, `comparam_defaults.rs`'s
`iso_14230_3_on_iso_15765_2`, a KWP-2000-over-CAN application-layer preset
whose hardware protocol — `ChannelProtocol::ISO_14230_3_ON_ISO_15765_2`,
`0x0102`, in the `0x0100..=0x0107` range `j2534_protocol_id()` maps to
`j2534_0404::ISO15765` — is CAN-family) seeds a literal
`ComParamId(j2534_0404::P3_MIN) = 55_000` anyway, inert and
client-invisible since `is_can_param` (not `is_kwp_param`) governs this
resource's actual `SetComParam`/`GetComParam` allowlist. CAN's own
equivalent inter-request pacing (`CP_P3Func`/`CP_P3Phys`) is a distinct
mechanism already covered by ADR-060 and explicitly out of scope for RC21/23
re-requests there.

**`CP_P3Min` is equally undefined for SAE J1850 VPW/PWM, but for a different
reason than CAN's access-rule rejection — it simply has no value there at
all** (design-advisor consult, Round 5): ISO 22900-2 Table B.10's
`SAE_J1850_VPW` column is blank for the `CP_P3Min` row
(`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5625`,
modulo the OCR conversion's own column-alignment caveat at `:5601` — blank
under any plausible shift), Table B.19's per-protocol defaults
(`:5935`) list `CP_P3Min` only for the ISO9141/ISO14230 family, and SAE
J2534-1 v04.04 itself scopes the native `P3_MIN` config parameter to
the ISO 9141 and ISO 14230 protocol IDs
(`vehicle-comm-specs/j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru Vehicle Programming.md:1237`).
Consistent with this, Annex I.1.4.3 step 1 (the tester waits for `CP_P3Min`)
and step 3 (the re-request goes out once `CP_P3Min` has expired)
both appear only inside their respective ISO-14230-3 sub-bullets
(spec lines 8348, 8358) — the SAE-J1850-VPW sub-bullets (lines 8347, 8359)
never mention `CP_P3Min` at all, describing the VPW request-time as a plain
literal (`1 second`) with no P3Min-substitution mechanism attached.

**Verify against 2022** (per `CLAUDE.md`'s spec-version caveat and the
design-advisor consult's own recommendation): this reading rests on the
only copy of ISO 22900-2 available in this workspace, the **2009(E)**
edition (`docs/j2534-0404-architecture.md` targets 2022) — a 2009→2022
revision could plausibly add a `CP_P3Min` definition for J1850 or
otherwise change Table B.10/B.19's scoping. The primary basis for "J1850
has no `CP_P3Min`" is SAE J2534-1 v04.04's own scoping of native `P3_MIN`
to the ISO 9141 and ISO 14230 protocol IDs (a J2534, not ISO 22900-2, citation, and this
workspace has no newer J2534-1 edition either) plus Table B.19's listing,
which is less exposed to the Annex text's own revision risk than a
clause-level reading would be — but this has not been re-checked against
2022 text, since none is available.

**VERIFIED against the 2022 edition (2026-08-11): confirmed unchanged, no
code change.** The 2022 text's own Table B.10 (Application layer ComParam
summary table, `vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:5774`)
still leaves the `SAE_J1850_VPW`/`SAE_J1850_PWM` columns blank for the
`CP_P3Min` row, the identical shape the 2009(E) Table B.10 reading above
was based on — only `ISO_14230_4`/`ISO_9141_2`/`ISO_14230_3` (the K-line
family) carry an `S` there. This ADR's `is_kwp_family()`/`is_j1850_family()`
branch split stands as written; SAE J2534-1 v04.04's own `P3_MIN` scoping
(no newer edition available in this workspace either) was not re-checked
since Table B.10 alone already settles it.

## Decision

`RcHandlingConfig::from_params` applies TWO DIFFERENT rules depending on
protocol family, matching Annex I.1.4.3 and the Context section's Table
B.10/B.19/J2534-1 findings, instead of substituting a hardcoded 25 ms
default whenever the stored value is `0`:

- **K-line** (`ChannelProtocol::is_kwp_family()`): the full
  `Max(CP_P3Min, CP_RC2xRequestTime)` floor from step 2 ("the greater
  value ... is used") applies.
- **J1850** (`ChannelProtocol::is_j1850_family()`, a new helper —
  `protocol.rs`): `CP_P3Min` has no defined value at all, so the explicit
  request time is used verbatim, with no floor term.
- **CAN**: neither rule applies — this codebase's CAN presets seed
  `CP_RC23RequestTime = 0` as their own default with `CP_RC23Handling`
  runtime-enable-able (Round 1), so presence there does not imply the
  client explicitly chose `0` the way it does on J1850 (no J1850 preset in
  this codebase ever seeds `0` — `j1850_common` and its callers all seed
  `200_000`). CAN keeps the plain 25 ms fallback unconditionally.

Both rules additionally require `CP_RC2xRequestTime` itself to be present
in the `ComParamSet`, distinguishing a genuinely *unconfigured* CLL from
one explicitly configured to `0` (Round 4):

```rust
pub(super) fn from_params(params: &ComParamSet, protocol: ChannelProtocol) -> Self {
    ...
    let is_kwp = protocol.is_kwp_family();
    let is_j1850 = protocol.is_j1850_family();
    // CP_P3Min only has a defined value on K-line -- force it to 0 (a
    // no-op floor) everywhere else, rather than reading whatever
    // (possibly inert) value happens to be in the raw map.
    let p3_min_ms = if is_kwp {
        params
            .unum32
            .get(&ComParamId(j2534_0404::P3_MIN))
            .copied()
            .unwrap_or(0)
            .div_ceil(1000)
    } else {
        0
    };
    let rc21_request_time_configured =
        (is_kwp || is_j1850) && params.unum32.contains_key(&PARAM_RC21_REQUEST_TIME);
    let rc23_request_time_configured =
        (is_kwp || is_j1850) && params.unum32.contains_key(&PARAM_RC23_REQUEST_TIME);
    ...
    rc21_request_time_ms: if rc21_request_time_configured {
        get_us_as_ms(PARAM_RC21_REQUEST_TIME, 0).max(p3_min_ms)
    } else {
        get_us_as_ms(PARAM_RC21_REQUEST_TIME, 25)
    },
    rc23_request_time_ms: if rc23_request_time_configured {
        get_us_as_ms(PARAM_RC23_REQUEST_TIME, 0).max(p3_min_ms)
    } else {
        get_us_as_ms(PARAM_RC23_REQUEST_TIME, 25)
    },
```

The field-assignment expressions themselves are unchanged from Round 4 —
`.max(p3_min_ms)` is always present, but forcing `p3_min_ms` to `0` on the
J1850 branch makes it a mathematical no-op there (`Max(0, x) = x`),
achieving "used verbatim" without a separate code path. This keeps the
K-line and J1850 branches sharing one expression instead of duplicating the
`get_us_as_ms`/`else` structure per branch.

`from_params` gained a `protocol: ChannelProtocol` parameter (previously
just `&ComParamSet`) so the gate has the caller's real protocol
classification to key on. All three call sites already had it in scope
without a new lookup: `events.rs::handle_send_recv` already captures
`protocol_id` (`link.hw_protocol_id`) into its own `SendRecvCycle`;
`rpc_primitive.rs`'s `CoptStartcomm` and `CoptStopcomm` arms both already
read `link.hw_protocol_id` a few lines above their own `from_params` call
for their own `resolve_send_recv_tx`/`tx_message_size_range` needs. All
three now pass `ChannelProtocol::from_raw(that same hw_protocol_id)`.
`hw_protocol_id`, not `link.protocol`, is deliberately what's threaded
through — the same distinction ADR-070's PR-review fix already established
for `build_tx_message`'s own protocol resolution (`link.protocol` is a
fixed *initial candidate* for the bus-agnostic `SAE_J1850`/`SAE_J2610`
protocols; `hw_protocol_id` is what the connection actually landed on).

The `contains_key` check on `PARAM_RC21_REQUEST_TIME`/`PARAM_RC23_REQUEST_TIME`
themselves does not repeat Round 3's mistake of reading the raw
`ComParamSet`: unlike `CP_P3Min`, these two ComParams are allowed on CAN,
KWP, and both J1850 arms (`is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/
`is_j1850vpw_param` in `comparam_support.rs` all include them — only
`is_sci_param` omits them, and SCI is excluded by every branch above
regardless), so no protocol this gate can ever be true for makes one of
these two keys' presence a false signal the way `CP_P3Min`'s
protocol-restricted accessibility did.

**This design went through five Codex/adversarial-review rounds on PR #136
before merge, each catching a real pre-existing-behavior regression before
landing (Codex itself hit its usage limit after round 4; round 5 was found
by an `edge-case-hunter` adversarial pass explicitly run as its substitute,
per CLAUDE.md's escalation norms, then resolved by a `design-advisor`
consult on the genuine spec-interpretation question it raised):**

- **Round 1** shipped the floor completely ungated — unconditional
  `get_us_as_ms(id, 0).max(p3_min_ms)` — reasoning that because no CAN
  preset seeds `CP_P3Min`, `Max(0, x) = x` made the change a no-op for CAN
  by construction. That reasoning missed that `CP_RC23RequestTime` is
  *also* seeded `0` by two CAN presets (`iso15765_4_common`,
  `iso_15765_3_on_iso_15765_2` — Table B.19's spec-correct value there too,
  per A2-16), each with `CP_RC23Handling` disabled *by default* but
  runtime-enable-able via `SetComParam` like any other ComParam. For such a
  client, the ungated floor silently substituted `Max(0, 0) = 0` for the
  prior 25 ms fallback — an unintended immediate-retry loop, not a no-op.
  Fixed by gating on `p3_min_ms > 0`.
- **Round 2** found that value-based gate (`p3_min_ms > 0`) itself
  conflated "absent" with "present and explicitly `0`" — a K-line client
  that `SetComParam(CP_P3Min, 0)`s (legal; nothing rejects a literal `0`
  write) would incorrectly fall through to the CAN-side 25 ms fallback
  instead of the spec-correct `Max(0, CP_RC2xRequestTime)`. Fixed by
  switching the gate to `contains_key` (presence).
- **Round 3** found presence itself was the wrong signal: resource
  `ISO_14230_3_on_ISO_15765_2` (see Context) is CAN-family but its preset
  seeds `CP_P3Min` anyway, so `contains_key` wrongly applied the K-line
  floor to a CAN CLL where `CP_RC2xRequestTime = 0` should retain the 25 ms
  fallback (or a smaller explicit value) rather than being floored up to
  the inert, client-invisible 55 ms. Fixed by threading the caller's actual
  `ChannelProtocol` through `from_params` and gating on
  `is_kwp_family()` instead of anything read from the raw `ComParamSet`.
- **Round 4** found the protocol gate alone was still not sufficient: a
  legacy/raw K-line CLL created via a bare numeric `resource_id`/
  `protocol_id` matching no resources-table row
  (`rpc_create_com_logical_link`'s `resource_row = None` fallback — the
  same legacy resolution path B22/ADR-069 document) gets a completely
  empty `ComParamSet`: no preset ever ran,
  so `CP_RC2xRequestTime` was never seeded, unlike every real preset in
  this codebase. A K-line-family `protocol` alone would still read that
  true absence as the Annex I.1.4.3 explicit-`0` case and compute
  `Max(0, 0) = 0`, an unintended immediate retry where the pre-ADR-125
  25 ms "nothing configured at all" fallback must still apply. Fixed by
  additionally requiring `CP_RC2xRequestTime`'s own presence in the
  `ComParamSet` (see Decision's note on why this specific presence check,
  unlike Round 2/3's `CP_P3Min` one, is sound).
- **Round 5** (an `edge-case-hunter` adversarial pass, run in place of a
  fifth Codex round after Codex's usage limit was hit) found the
  K-line-only protocol gate itself was too narrow: Annex I.1.4's own
  section title is "Additional RC23/RC21 handling description for **SAE
  J1850 VPW and ISO 14230 protocols**", and `CP_RC21RequestTime`/
  `CP_RC23RequestTime` are legally `SetComParam`-able on J1850VPW/PWM
  (`is_j1850vpw_param`/`is_j1850pwm_param`, `comparam_support.rs`) — a
  client explicitly setting `CP_RC23RequestTime = 0` there was still
  silently coerced to the hardcoded `25` ms literal, the exact defect this
  whole ADR exists to eliminate, just unreached by the K-line-only fix.
  Whether the fix should be "widen the K-line floor to J1850" or something
  narrower was a genuine spec-interpretation question (does `Max(P3Min,
  x)` really extend to a protocol Table B.10/B.19/J2534-1 never define
  `CP_P3Min` for at all?), so it was escalated to a `design-advisor`
  consult rather than resolved by pattern-matching against Rounds 1-4.
  The verdict (see Context and Decision above): J1850 gets its own branch
  — explicit values used verbatim, no P3Min floor, since none exists to
  apply.

## Consequences

- An ISO 14230/ISO 9141 (K-line) CLL with `CP_RC21Handling`/
  `CP_RC23Handling` enabled and the Table-B.19-seeded `CP_RC2xRequestTime =
  0` now waits `CP_P3Min` (55 ms for every K-line preset in this codebase)
  before an RC21/RC23 re-request, instead of a hardcoded 25 ms with no
  spec citation.
- A K-line CLL that explicitly configures `CP_RC2xRequestTime` to a value
  below its own `CP_P3Min` is now floored up to `CP_P3Min` too (Annex
  I.1.4.3 step 2's "the greater value ... is used" is a `Max`, not a
  substitution only for the `0` case).
- A K-line CLL with `CP_P3Min` explicitly set to `0` now correctly gets an
  immediate (`0` ms) RC21/RC23 re-request when `CP_RC2xRequestTime` is also
  `0` — distinct from a K-line CLL relying on the preset default (`CP_P3Min`
  seeded at `55 000` µs).
- A SAE J1850 VPW/PWM CLL with `CP_RC21Handling`/`CP_RC23Handling` enabled
  and an explicit `CP_RC2xRequestTime` (including `0`) now uses that value
  verbatim, with no P3Min floor (none exists for J1850) — an explicit `0`
  no longer gets silently coerced to the hardcoded 25 ms.
- No behavior change for any CAN/ISO15765 CLL, **including
  `ISO_14230_3_on_ISO_15765_2`, whose preset's `CP_P3Min` value is now
  correctly ignored regardless of it being present in the raw ComParamSet**
  — the protocol-based gate never engages there — or for any K-line CLL
  with `CP_RC2xRequestTime` already configured above its `CP_P3Min`.
- No behavior change for a legacy/raw K-line *or* J1850 CLL created with an
  empty `ComParamSet` (no resources-table row matched) — both keep the
  original 25 ms fallback exactly as before this ADR, instead of an
  unintended immediate (`0` ms) retry.
- **Accepted residual (Round 5):** no J1850 preset in this codebase today
  seeds `CP_RC21RequestTime`/`CP_RC23RequestTime = 0` (`j1850_common` and
  every caller seed `200_000` — verified as part of the Round 5 review), so
  the J1850 branch is currently reachable only via an explicit client
  `SetComParam(..., 0)` call, never a preset default. If a future preset
  change ever seeds `0` for a J1850 protocol with `CP_RC2xHandling` also
  enabled or runtime-enable-able, this branch would activate for every
  client relying on that default (the same shape as Round 1's CAN
  regression) — worth re-checking `comparam_defaults.rs`'s J1850 presets
  specifically if this ever changes.
- ADR-060's own accepted scope (RC21/23 re-request retransmissions are not
  gap-*tracked* against `CP_P3Func`/`CP_P3Phys`) is unchanged — this ADR
  fixes only the request-*time* value fed into the existing, already-gapped
  wait, not whether that wait feeds back into the CAN gap-tracking state
  ADR-060 built.
- Unit test coverage: `service.rs`'s `RcHandlingConfig` tests —
  `rc_handling_config_uses_p3_min_when_rc2x_request_time_is_explicit_zero`,
  `rc_handling_config_rc2x_request_time_floored_to_p3_min_when_smaller`,
  `rc_handling_config_can_preset_zero_request_time_keeps_25ms_fallback_without_p3_min`
  (pins Round 1's regression), `rc_handling_config_explicit_zero_p3_min_is_distinguished_from_absent`
  (pins Round 2's regression), `rc_handling_config_can_family_ignores_inert_p3_min_from_preset`
  (pins Round 3's regression, reproducing only the specific trait of
  `ISO_14230_3_on_ISO_15765_2` that matters here — `CP_P3Min` present at
  `55_000` on a CAN-family protocol; its `CP_RC21RequestTime`/
  `CP_RC23RequestTime` values are deliberately synthetic (`0`/`10_000`),
  not the real preset's own `200_000`/`200_000` — using the real values
  would pass identically whether the CAN-family gate were correct or
  broken, since `Max(55, 200) = 200` either way, so the test needs
  different values to actually discriminate),
  `rc_handling_config_kwp_family_with_no_preset_keeps_25ms_fallback` (pins
  Round 4's regression — a K-line-family protocol with a totally empty
  `ComParamSet`), `rc_handling_config_j1850_explicit_zero_request_time_used_verbatim`,
  `rc_handling_config_j1850_family_with_no_preset_keeps_25ms_fallback`, and
  `rc_handling_config_j1850_ignores_p3_min_even_if_present` (pin Round 5's
  fix and its own inert-`CP_P3Min` variant), and the pre-existing
  `rc_handling_config_defaults_rc21_rc23_timings_when_unset` (now
  explicitly passes a CAN-family protocol, since the gate is no longer
  ComParamSet-shape-dependent).
- `iso22900-2-conformance-audit.md`'s B11 row is updated to record this fix;
  ADR-060's own "Accept the documented gap-check scope limitation" triage
  for the broader RC21/23 gap-tracking deviation stands unchanged.
