# ADR-072: K-Line/SCI Timing ComParam Unit Conversion and `CP_TIdle` W0/W5 Fan-Out

**Date:** 2026-07-09
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/rpc_link.rs` (`apply_j2534_params`), `j2534-0404-service/src/service/events.rs` (`apply_params_to_hardware`), `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/names.rs`

## Context

`ComParamId::to_j2534_config_id` (ADR-027/ADR-028) forwards the ~35
native-equivalent ComParam IDs to `PassThruIoctl SET_CONFIG` unchanged,
on the assumption that the D-PDU ComParam value and the native J2534
`SET_CONFIG` value share the same units. ADR-037 already documented one
exception (`STMIN_TX`). Seventeen more of these IDs -- the K-line/KWP
timing family (`P1_MAX`, `P3_MIN`, `P4_MIN`, `W0`-`W5`, `TIDLE`, `TINIL`,
`TWUP`) and the SCI hardware timers (`T1_MAX`-`T5_MAX`) -- have the same
mismatch:

- ISO 22900-2 `CP_*` ComParam space represents all of these at 1 us
  resolution (confirmed by `comparam_defaults.rs`'s existing seeded
  presets, e.g. `P1_MAX = 20_000` for a 20 ms default, `TIDLE = 300_000`
  for 300 ms -- these presets already assumed microsecond ComParam-space
  values even before this ADR's conversion existed).
- SAE J2534-1 (DEC2004) native `SET_CONFIG` encodes `P1_MAX`/`P3_MIN`/
  `P4_MIN` at 0.5 ms resolution and `W0`-`W5`/`TIDLE`/`TINIL`/`TWUP`/
  `T1_MAX`-`T5_MAX` at 1 ms resolution (per the adapter's native units,
  distinct from the ComParam encoding).

Before this change, `to_j2534_config_value` passed these 17 IDs straight
through (only `STMIN_TX` converted, per its doc comment), so a client
supplying a microsecond `CP_*` value (matching the seeded defaults'
convention) ended up writing a native config value 500x-1000x too large --
e.g. a 20 ms `P1_MAX` (`20_000` us) would have been forwarded as native
`P1_MAX = 20_000` (10 seconds), not `40` (20 ms at 0.5 ms resolution).

Separately, ISO 22900-2's `CP_TIdle` has no direct 1:1 native counterpart:
J2534 v04.04 splits the K-line idle timer into `W0` (ISO9141) and `W5`
(ISO14230) -- two different `SET_CONFIG` IDs, gated to mutually exclusive
protocols (ADR-028). A client setting only `CP_TIdle` (the ID that
happens to numerically overlap the native `TIDLE` register) had no way to
also populate whichever of `W0`/`W5` the connected protocol actually uses,
short of separately calling `SetComParam` on that native ID too.

## Decision

**Unit conversion.** Extended `to_j2534_config_value` (a `match` on
`config_id`, replacing the previous `if config_id == STMIN_TX` check) to
convert these 17 IDs, via two new helpers alongside
`stmin_override_to_stmin_tx`:

- `us_to_half_ms(value_us) -> value_us.saturating_add(250) / 500` for
  `P1_MAX`, `P3_MIN`, `P4_MIN` (0.5 ms native resolution).
- `us_to_ms(value_us) -> value_us.saturating_add(500) / 1000` for `W0`-`W5`,
  `TIDLE`, `TINIL`, `TWUP`, `T1_MAX`-`T5_MAX` (1 ms native resolution).

Both use the same round-to-nearest-via-saturating-add policy as
`stmin_override_to_stmin_tx` (ADR-037): a value exactly halfway between two
native steps rounds up (an artifact of integer division on the
`saturating_add`-offset value, not a distinct tie-break rule -- unlike
ADR-037's STMIN_TX, which has an explicit favor-the-larger-candidate rule
because it must choose between two *different* encodings, not just round
within one). `saturating_add` means no input (including `u32::MAX`) can
panic.

**`CP_TIdle` -> `W0`/`W5` fan-out.** Added `expand_tidle(configs, j2534_protocol_id)`
in `comparam_id.rs`, mirroring `expand_uart_config`'s shape (ADR-071): given
the `(config_id, value)` list already produced by `to_j2534_config_id`, if a
`TIDLE` entry is present and the protocol is ISO9141, a `W0` entry with the
same (still-unconverted, microsecond) value is appended; on ISO14230, a `W5`
entry is appended instead; on any other protocol, nothing is added. If
`configs` already contains an explicit `W0`/`W5` entry (the client called
`SetComParam` on that native-overlapping ID directly), that explicit entry
wins and no derived entry is appended -- deterministic, independent of
`ComParamSet.unum32`'s `HashMap` iteration order, the same precedence rule
`expand_uart_config` uses for `CP_Parity` vs. `CP_UartConfig`-derived parity.

**Working-set storage: `CP_TIdle` writes only its own ID.** `rpc_set_com_param`
stores a `SetComParam(CP_TIdle, ...)` under `ComParamId(TIDLE)` in
`link.working.unum32` only -- it does **not** also insert into the `W0`/`W5`
entries of the same map. `expand_tidle` is the *only* place a `TIDLE` value
becomes a `W0`/`W5` value, and it runs once, at the hardware-forwarding
boundary, over the Working (or Active) snapshot already bound for that
call/COP -- never against the persistently-stored `ComParamSet` itself. This
mirrors ADR-071's `CP_UartConfig`: it is stored under its own ID only, and
`PARITY` derivation happens at forwarding time via `expand_uart_config`, not
at `SetComParam` time. This is a deliberate design choice, not an
implementation detail: mutating `W0`/`W5`'s *stored* Working entries at
`SetComParam(CP_TIdle, ...)` time (as an earlier draft of this change did,
before review) would make the explicit-entry-wins precedence above
order-dependent -- `SetComParam(W0/W5, x)` followed by a *later*
`SetComParam(TIDLE, y)` would silently overwrite the explicit `x` in the
Working set with `y`, and `expand_tidle` would then see the overwritten `W0`/
`W5` entry as "explicit" and forward `y`, not `x`. Storing `TIDLE` under its
own ID only, and deriving `W0`/`W5` fresh at every forwarding call, makes the
precedence rule hold regardless of `SetComParam` call order.

**Call-site pipeline ordering.** Both hardware-forwarding call sites
(`rpc_link.rs::apply_j2534_params`, `events.rs::apply_params_to_hardware`)
previously ran `to_j2534_config_value` per-entry inside the `filter_map`
that produces the `(config_id, value)` list, before `expand_uart_config`.
This ADR reorders that pipeline: `to_j2534_config_id` mapping now produces
still-unconverted `(config_id, value)` pairs; `expand_uart_config` and
`expand_tidle` both run on that unconverted list (in either order --
they touch disjoint config ID sets); `to_j2534_config_value` now runs last,
mapped once over every entry in the final expanded list, including any
`W0`/`W5` entry `expand_tidle` derived. This guarantees a derived entry is
unit-converted exactly once, like every other entry, rather than zero times
(if conversion ran before expansion, a derived `W0`/`W5` copy of an
already-converted `TIDLE` value would happen to still be numerically
correct here only because `TIDLE`/`W0`/`W5` all share the same 1 ms
resolution -- but that coincidence should not be load-bearing) or twice.

## Consequences

- **Breaking change in these 17 IDs' ComParam-space semantics.** A caller
  that previously relied on setting e.g. `P1_MAX`/`TIDLE`/`T1_MAX` to an
  already-native-scale value (as the pre-fix integration tests in
  `comparam_tx.rs`/`startcomm_comparam.rs` did) must switch to microsecond
  values -- this is the intended fix (the prior passthrough did not match
  ISO 22900-2's documented 1 us `CP_*` resolution, and directly contradicted
  the microsecond values already seeded by `comparam_defaults.rs`), but it
  is wire-visible for any existing client setting these IDs directly by
  native-scale numeric value rather than by name+documented unit.
- **`GetComParam` is unaffected**, per the same rule as ADR-037: it reads
  the in-memory Working `ComParamSet` (unconverted, microsecond-domain)
  and never queries hardware, so a client reading back e.g. `CP_TIdle`
  after `SetComParam` sees the value it set, not the converted value
  written to hardware -- and, per this ADR, not the `W0`/`W5` value derived
  from it either (`expand_tidle` runs only in the hardware-forwarding
  pipeline, not against the stored `ComParamSet`; there is no companion
  read-side derivation for `W0`/`W5` the way ADR-071 added one for
  `CP_Parity`, since a client reading `W0`/`W5` back without ever having set
  them directly gets `0`, matching every other unset Unum32 ComParam).
- **The stored Working `ComParamSet` never gains a `W0`/`W5` entry as a side
  effect of `SetComParam(CP_TIdle, ...)` either** -- not just `GetComParam`'s
  externally-visible behavior, but the `HashMap` storage underneath it
  (`link.working.unum32`/`link.active.unum32`). This is what makes the
  explicit-entry-wins precedence order-independent (see "Working-set
  storage" in Decision, above): `SetComParam(W0/W5, ...)` and
  `SetComParam(CP_TIdle, ...)` may arrive in either order and the result is
  the same -- the explicit `W0`/`W5` value always reaches hardware, because
  `expand_tidle` (not stored-map mutation) is the only fan-out mechanism,
  and it re-evaluates "is there an explicit entry?" fresh against the
  snapshot at every forwarding call.
- **Rounding loses precision by design** for non-exact-multiple inputs
  (e.g. 20_249 us rounds down to 40, 20_250 us rounds up to 41 for a
  0.5 ms-resolution param) -- inherent to the native encoding's coarser
  step size, not a bug, and now a documented, tested behavior (mirrors
  ADR-037's STMIN_TX precision-loss note).
- **If a future ComParam needs the same kind of conversion**,
  `to_j2534_config_value`'s `match` is the place to add it, per ADR-037's
  existing guidance -- this ADR does not change that guidance, only
  exercises it for a larger set of IDs.

See ADR-027 (D-PDU ComParam ID / native config ID overlap), ADR-028 (the
per-protocol `SET_CONFIG` support matrix these IDs are gated by,
unchanged by this ADR), ADR-037 (the `STMIN_TX` conversion and
`to_j2534_config_value` hook this ADR extends), and ADR-071 (the
`expand_uart_config`/explicit-entry-wins pattern `expand_tidle` mirrors).
