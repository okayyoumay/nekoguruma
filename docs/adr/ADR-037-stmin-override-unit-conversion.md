# ADR-037: CP_StMinOverride ↔ J2534 STMIN_TX Unit Conversion

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/events.rs`

## Context

`STMIN_TX` (`0x23`) is one of the ~35 ComParam IDs `ComParamId::to_j2534_config_id`
(ADR-027/ADR-028) forwards to hardware unchanged, on the assumption that the
D-PDU ComParam value and the native J2534 `SET_CONFIG` value share the same
units and encoding. That assumption does not hold for `CP_StMinOverride`
(ISO 22900-2), which the service maps to `STMIN_TX`:

- **ISO 22900-2 `CP_StMinOverride`**: a plain microsecond value, 1 us
  resolution. `0xFFFFFFFF` is the sentinel meaning the ECU's own advertised value is used.
- **SAE J2534-1 `STMIN_TX`**: a two-range encoding per the ISO 15765-2 STmin
  byte —
  - `0x00`-`0x7F`: 0-127 ms, 1 ms resolution.
  - `0xF1`-`0xF9`: 100-900 us, 100 us resolution.
  - `0xFFFF` is the sentinel meaning the ECU's own advertised value is used.

Forwarding a `CP_StMinOverride` microsecond value straight through as the raw
`STMIN_TX` `SET_CONFIG` value (the previous behavior) produced a value in the
wrong units and, for most inputs, not even a value `STMIN_TX` can represent —
e.g. a client requesting 500 us would have set the adapter to interpret
`STMIN_TX = 500` as 500 ms (out of the valid `0x00`-`0x7F` range entirely).

`CP_StMinOverride`'s 1 us resolution is finer than either `STMIN_TX` range,
so most microsecond values do not land exactly on a representable step, and
values above 127 ms have no `STMIN_TX` representation at all. Both cases
needed an explicit policy.

## Decision

Added `stmin_override_to_stmin_tx(value_us: u32) -> u32` in `comparam_id.rs`,
called through a new `to_j2534_config_value(config_id: u32, value: u32) ->
u32` hook that both hardware-forwarding call sites
(`rpc_link.rs::apply_j2534_params`, `events.rs::apply_params_to_hardware`)
now run every value through, immediately after `to_j2534_config_id`
translates the param ID. `to_j2534_config_value` is a passthrough for every
config ID except `STMIN_TX`; it is the single point where a ComParam value
may need reinterpretation before reaching `PassThruIoctl SET_CONFIG`, mirroring
how `to_j2534_config_id` is the single point for ID translation.

Conversion rules for `stmin_override_to_stmin_tx`:

- `0xFFFFFFFF` (ISO 22900-2 sentinel) maps to `0xFFFF` (J2534 sentinel).
- Otherwise, the nearest representable step in each of the two `STMIN_TX`
  ranges is computed (100 us steps in `[100, 900]` us, 1 ms steps in
  `[0, 127000]` us), and whichever of the two is numerically closer to the
  input is used. A tie (input equidistant from both) favors the larger of
  the two candidates.
- Because both candidates are pre-clamped to their valid range (`900` us and
  `127000` us respectively), an input above 127 ms naturally resolves to the
  1 ms candidate at its clamped maximum (`0x7F`) — there is no separate
  clamping step.

This makes the conversion a straightforward "nearest valid `STMIN_TX`
encoding" function with no silent truncation: every `u32` input (other than
the sentinel) produces a defined, in-range `STMIN_TX` byte value.

## Consequences

- `GetComParam` still reads the in-memory Working `ComParamSet` (unconverted,
  microsecond-domain) and is unaffected — the conversion only happens at the
  hardware-forwarding boundary, so a client reading back `CP_StMinOverride`
  after `SetComParam` sees the value it set, not the lossy `STMIN_TX`
  encoding actually applied to hardware.
- Non-representable inputs lose precision by design (e.g. 150 us rounds to
  200 us, 950 us rounds to 1 ms) — this is inherent to `STMIN_TX`'s coarser
  encoding, not a bug; ADR captures the rounding/tie-break policy so it is a
  documented, tested behavior rather than an implicit one.
- If a future ComParam needs the same kind of unit conversion,
  `to_j2534_config_value` is the place to add it — a `match` on `config_id`
  alongside the `STMIN_TX` case, not a new call-site-specific hook.
