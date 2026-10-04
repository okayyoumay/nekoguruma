# ADR-056: CP_P2Star Drives RC78 Handling; CP_P3Phys/CP_P3Func Reachable for CAN

**Date:** 2026-07-04
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`RcHandlingConfig`),
             `j2534-0404-service/src/service/comparam_defaults.rs`,
             `j2534-0404-service/src/service/comparam_support.rs`,
             `j2534-0404-service/src/service/names.rs`,
             `j2534-0404-service/src/service/rpc_link.rs`

## Context

An audit of ISO15765 P2/P3 timing ComParam handling turned up two unrelated
gaps:

**1. `CP_P2Star` was dead.** `PARAM_P2_STAR` (`0x8011`) was defined,
name-mapped, allowlisted, and pre-seeded with real per-protocol values in
`comparam_defaults.rs` — but never read by any code. Its own doc comment
already stated its intended purpose: *"the extended timeout after receiving
a ResponsePending indication (e.g. RC 0x78)"* — exactly what ADR-018's RC78
auto-handling needs. ADR-018 instead used a separate, service-specific
`CP_RC78CompletionTimeout` (`0x8026`) for this, leaving the D-PDU-standard
`CP_P2Star` unused. A client that (correctly, per the D-PDU/ISO 14229
convention) sets `CP_P2Star` to control the post-0x78 wait window had no
effect on this service's actual behavior.

Both ComParams carry real, but *different*, values in every existing
preset (`CP_P2Star` ~5,000,000 vs `CP_RC78CompletionTimeout`
25,000,000–30,000,000, both in ms per their own doc comments) — so simply
switching the read source to `CP_P2Star` as originally seeded would have
cut the RC78 wait window by 5–6x for every existing preset with RC78
handling enabled by default (e.g. `iso_15031_5_on_iso_14230_4`,
`iso_15031_5_on_iso_15765_4`).

**2. `CP_P3Phys`/`CP_P3Func` were unreachable for CAN/ISO15765.** The
service-level `PARAM_P3_FUNC`/`PARAM_P3_PHYS` (`0x80B3`/`0x80B4`) exist
specifically as the CAN-context equivalents of KWP's native
`P3_MIN`/`P3_MAX` forwarding (per `service_params.rs`'s own doc comment) and
are pre-seeded by three CAN-family presets — but:
- `is_can_param` (`comparam_support.rs`) never included them, so
  `SetComParam`/`GetComParam` rejected both with `invalid_argument`
  regardless of how they were addressed (by name or by raw numeric ID).
- `map_comparam_name` (`names.rs`) mapped the shortnames `"cp_p3phys"`/
  `"cp_p3func"` unconditionally to native `P3_MIN`, so even `GetComParam`
  by name on a CAN/ISO15765 channel returned the wrong (and, per
  `is_can_param`, unreachable-anyway) native ComParam instead of the
  intended service-level one.

(Investigated separately and found *not* to be a gap: whether a Single
Frame ISO15765 send requires a `FLOW_CONTROL_FILTER` configured first. It
doesn't — the only send-time requirement is the CAN ID needed to build the
header (`tx_header.rs`, unrelated to flow control), and RX-side filter
installation already has a pass-all fallback (ADR-048) for exactly this
case. No change was needed there.)

## Decision

### CP_P2Star becomes the sole authoritative source for the RC78 extension

`RcHandlingConfig::from_params` (`service.rs`) now reads `PARAM_P2_STAR`
instead of `PARAM_RC78_COMPLETION_TIMEOUT` for `rc78_completion_timeout_ms`.
`CP_RC78CompletionTimeout` remains a valid, get/settable ComParam (for
backward compatibility and inspection) but no longer drives any behavior.

To keep every existing preset's *runtime* RC78 wait window unchanged
despite the two ComParams' diverging pre-seeded values,
`protocol_default_params` (`comparam_defaults.rs`) now seeds `CP_P2Star`
from each preset's own `CP_RC78CompletionTimeout` value as a final step,
rather than using the presets' original (lower, and now irrelevant)
`CP_P2Star` literals — which are removed from the individual preset
functions to avoid confusing, immediately-overwritten dead values:

```rust
p.unum32.insert(PARAM_P2_STAR, p.unum32.get(&PARAM_RC78_COMPLETION_TIMEOUT).copied().unwrap_or(5_000_000));
```

This is a programmatic sync (reading the already-computed
`CP_RC78CompletionTimeout` value back out of the same `ComParamSet`)
rather than hand-transcribed literals, since manually copying numbers
across ten preset functions is exactly the kind of transcription that
already produced the mismatch this ADR fixes.

A caller that explicitly sets `CP_P2Star` via `SetComParam` after
`CreateComLogicalLink` now controls the RC78 wait window directly, matching
the D-PDU/ISO 14229 standard meaning of the parameter.

### CP_P3Phys/CP_P3Func made reachable for CAN/ISO15765

- `is_can_param` now includes `PARAM_P3_FUNC`/`PARAM_P3_PHYS`, so
  `SetComParam`/`GetComParam` accept them (by raw numeric ID) on CAN and
  ISO15765 channels, matching the values already pre-seeded there.
- A new `map_comparam_name_for_protocol(name, protocol)` in `names.rs`
  resolves `"cp_p3phys"`/`"cp_p3func"` to the CAN-context
  `PARAM_P3_PHYS`/`PARAM_P3_FUNC` when `protocol.is_can_family()`, and
  otherwise defers to the existing protocol-agnostic `map_comparam_name`
  (still resolving to native `P3_MIN` for KWP, where both names collapse
  onto the same single hardware register — there is no separate KWP
  physical/functional P3 CONFIG). `map_comparam_name` itself is
  deliberately kept protocol-agnostic (its own doc comment states
  shortnames are unique across categories) since it also backs
  `GetObjectId`'s `resolve_object_id`, which has no CLL/protocol context at
  all. Only `rpc_get_com_param` — which does have a resolved `link.protocol`
  — was switched to the new protocol-aware resolver; `SetComParam` has no
  by-name variant in the proto, so no change was needed there.

## Consequences

- Every existing preset's RC78 auto-handling wait window is numerically
  identical to before this change (verified by
  `protocol_default_params_syncs_p2_star_from_rc78_completion_timeout`).
  (No longer true as of ADR-102, which removes this sync entirely and
  renames the verifying test to
  `protocol_default_params_no_longer_syncs_p2_star_from_rc78_completion_timeout`
  — see ADR-102's own Consequences for the resulting behavior change.)
- A client that sets `CP_P2Star` (the standards-correct ComParam) now gets
  the behavior it should have always gotten; a client that sets
  `CP_RC78CompletionTimeout` instead no longer has any effect — this is a
  deliberate, documented behavior change for that specific (non-standard)
  ComParam, consistent with treating `CP_P2Star` as authoritative going
  forward.
  (Amended by ADR-102: `CP_RC78CompletionTimeout` is reinstated as an
  independent total-ceiling ComParam, distinct from `CP_P2Star`'s
  per-occurrence reload role — it is no longer inert.)
- `CP_P3Phys`/`CP_P3Func` are now get/settable on CAN/ISO15765 channels,
  both by raw numeric ID and by name. They were stored-only at the time of
  this ADR (no inter-request minimum-gap enforcement existed for any
  protocol, and adding new TX-scheduling behavior was out of scope for this
  fix) — **amended by ADR-060**, which implements the actual gap
  enforcement these two ComParams configure.
- `CP_P2Star_Ecu`/`CP_P2Max_Ecu` remain stored-only with no per-ECU
  override mechanism — also out of scope for this fix.
