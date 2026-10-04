# ADR-181: CP_W1-W4 Min/Max ComParam Storage-Key Collision Fix

**Date:** 2026-08-18
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/service_params.rs`, `j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/comparam_defaults.rs`, `j2534-0404-service/src/service/comparam_support.rs`

## Context

`names.rs::map_comparam_name_native` resolved both the Max and Min
shortname of each of `CP_W1`, `CP_W2`, `CP_W3`, and `CP_W4` to the same
native `ComParamId` -- and that resolved `ComParamId` is the literal
storage key in a link's Working/Active `unum32` map (a
`HashMap<ComParamId, u32>`). Concretely, `cp_w1max`/`cp_w1min` both
resolved to native `W1`, `cp_w2max`/`cp_w2min` both to native `W2`,
`cp_w3max`/`cp_w3min` both to native `W3`, and `cp_w4max`/`cp_w4min` both
to native `W4`. The reverse (id-to-canonical-name) direction had the same
collision: `ComParamId(W4)`, for instance, was claimed by both `CP_W4Max`
and `CP_W4Min`.

The practical effect: `SetComParam(CP_W1Max, x)` followed by
`SetComParam(CP_W1Min, y)` silently overwrote the same HashMap entry --
`GetComParam(CP_W1Min)` afterward incorrectly reported `x` (or `y`,
depending on call order), and whichever value was staged last is what
reached native `PassThruIoctl SET_CONFIG`, not a value either the client
or this codebase's own design chose deliberately.

SAE J2534-1's Figure 30 (DEC2004, v04.04) defines native `W1`/`W2`/`W3` as
single MAXIMUM-only timers and native `W4` as a single MINIMUM-only timer
-- there is no native MIN-side register for `W1`-`W3` and no native
MAX-side register for `W4`. ISO 22900-2:2009(E) Table A.3 accordingly maps
only `CP_W1Max->W1`, `CP_W2Max->W2`, `CP_W3Max->W3`, and `CP_W4Min->W4`.
`CP_W1Min`, `CP_W2Min`, `CP_W3Min`, and `CP_W4Max` have no native mapping
in the spec at all -- but each is still a legitimate, independently
defined D-PDU ComParam (ISO 22900-2:2009(E), each with its own
description, range, and per-protocol defaults, applicable to the ISO9141-2/
ISO14230-2/ISO14230-4 family), so a client legitimately wants both bounds
of a timing window settable and independently readable.

A second, related bug surfaced during the same review: `comparam_defaults.rs`
seeded the native `W4` slot (both K-line preset-seeding sites,
`kwp_on_kline_common` and `kwp_on_9141_common`) with `50_000` us, labeled
`// CP_W4Max`. Since Table A.3 makes the native `W4` register `CP_W4Min`'s,
not `CP_W4Max`'s, whatever value sits in that slot is what actually reaches
hardware as native `W4` -- so the seeded default was silently forwarding
the wrong ComParam's value. The correct default for that slot is
`CP_W4Min`'s own ISO default, `25_000` us.

## Decision

**Mint four new project-chosen `ComParamId`s**, in the `0x8000`-range
convention `service_params.rs` already uses for service-level/project ids
(the highest previously allocated id in that file was `0x80C4`):

| Constant | Hex | D-PDU name |
|---|---|---|
| `PARAM_W1_MIN` | `0x80C5` | `CP_W1Min` |
| `PARAM_W2_MIN` | `0x80C6` | `CP_W2Min` |
| `PARAM_W3_MIN` | `0x80C7` | `CP_W3Min` |
| `PARAM_W4_MAX` | `0x80C8` | `CP_W4Max` |

**Bijective `names.rs` mapping.** `CP_W1Max`/`CP_W2Max`/`CP_W3Max`/
`CP_W4Min` keep resolving to native `W1`-`W4` exactly as before -- these
are the sides Table A.3 actually gives a native register to.
`CP_W1Min`/`CP_W2Min`/`CP_W3Min`/`CP_W4Max` -- the sides with no native
counterpart -- now resolve to their own new project id instead of
re-using the native one. The reverse direction (the shortname-resolution
test table) is updated to match: each id now has exactly one canonical
name, so `ComParamId(W4)`'s canonical name is unambiguously `CP_W4Min`.

**`to_j2534_config_id` needs no new arm.** The function's per-id `match`
(`comparam_id.rs`) already ends in a catch-all `_ => false` for any id it
does not explicitly recognize, so the four new project ids naturally
never translate to a native `SET_CONFIG` id and are never forwarded to
hardware -- confirmed directly against the function's source, not assumed.
This is the same store-only shape this codebase already uses for other
project-invented service-level ComParams with no native counterpart (e.g.
`CP_BlockSizeOverride`'s sibling params on a link where they have no
translation target).

**`comparam_defaults.rs` reseed.** Both K-line preset-seeding call sites
(`kwp_on_kline_common`, `kwp_on_9141_common`) now seed the native `W4`
slot with `25_000` us (`CP_W4Min`'s own ISO default), correcting the
mislabeled/miscoded `50_000` us. The four new project ids are seeded
alongside it with their own independent ISO defaults: `CP_W1Min = 60_000`,
`CP_W2Min = 5_000`, `CP_W3Min = 0`, `CP_W4Max = 50_000` (all microseconds,
this file's existing convention). `W1`/`W2`/`W3`'s own Max-side seeds are
unchanged.

**No `expand_tidle`-style derivation.** ADR-072's `expand_tidle` fans a
single D-PDU ComParam (`CP_TIdle`) out to whichever native register
(`W0`/`W5`) the connected protocol actually uses, because in that case
both native targets exist and the ambiguity is purely "which protocol are
we on." That shape does not apply here: Table A.3 defines no native
fan-out target for `CP_W1Min`/`CP_W2Min`/`CP_W3Min`/`CP_W4Max` on any
protocol, so there is nothing to derive into. Attempting it anyway would
actively corrupt timing -- writing a minimum-bound value into a
maximum-timeout register (or vice versa) is not a rounding error, it
inverts the field's meaning. These four ids are therefore store-only by
design, not by an accidental gap.

**`CP_W0Max`/`CP_W5Max` residual.** These two names are a separate,
project-invented direct-native-alias pattern (`names.rs`), unrelated to
this fix -- ISO 22900-2 defines no `CP_W0*`/`CP_W5*` ComParam at all, only
`CP_TIdle`. A short code comment documents this as an accepted residual
where it's defined; this ADR does not otherwise touch it.

## Consequences

- The whole `CP_W1`-`CP_W4` Min/Max family is now order-independent in
  storage: setting one side of a pair never clobbers the other, and
  `GetComParam` faithfully reads back whichever value was actually staged
  for that specific shortname, regardless of call order.
- **Wire-visible behavior change, intended:** a fresh K-line preset
  (ISO9141 or ISO14230, no client `SetComParam` override) now forwards a
  native `W4` of `25` (ms) at connect time, not the old, incorrect `50`.
  Any caller relying on the old (wrong) `50 ms` fresh-preset value sees a
  different timeout after this change; the new value is the one Table
  A.3's default actually specifies for the register that's really there.
- No unit-conversion arm is needed for the four new ids in
  `to_j2534_config_value` -- they're never forwarded, so no native-unit
  conversion ever applies to them.
- `CP_W1Min`/`CP_W2Min`/`CP_W3Min`/`CP_W4Max` are added to the KWP-family
  ComParam allowlist (`comparam_support::is_kwp_param`) alongside the rest
  of the W-timer group, so `SetComParam`/`GetComParam` accept them on
  ISO9141/ISO14230 links exactly as they do the native-mapped sides.
