# ADR-130: `ISO_11898_3_DWFTCAN` Default `CP_Baudrate` Corrected to 125k; Spec-Undefined `CP_CanBaudrateRecord` Default Removed

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_defaults.rs`, `j2534-0404-service/src/service/comparam_support.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/docs/comparam-protocol-support.md`

## Context

Conformance-audit finding A2-13 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`):
`iso_11898_3_dwftcan`'s (`comparam_defaults.rs`) `CP_Baudrate` (`DATA_RATE`)
default was `500_000`, contradicted by the function's own doc comment ("up to
125 kbit/s") and by ISO 22900-2:2009(E) Table B.21 (line 6075), which lists
`ISO_11898_3_DWFTCAN = 125k` for `CP_Baudrate` — the bus's own physical
ceiling, not the 500 kbit/s DW-CAN rate.

While reading Table B.21 in full to fix the cited default (per this
document's own guidance to verify the whole cited section, not just the
summarized defect), the same function's `CP_CanBaudrateRecord`
(`PARAM_CAN_BAUDRATE_RECORD`) default of `250_000` turned out to have no
basis in spec either: Table B.21's `CP_CanBaudrateRecord` row (line 6078)
defines a default only for `ISO_11898_2_DWCAN` (`{500000, 250000}`) and
`SAE_J1939_11_DWCAN` (`{250000}`) — the two bus types that use this record
for OBD CAN-speed auto-detection during initialization. `ISO_11898_3_DWFTCAN`
has no row entry at all. The pre-existing `250_000` value was thus both
unauthorized by spec and, independently, in violation of the same 125 kbit/s
physical limit that motivated this finding in the first place — a `250_000`
list entry advertises a rate the bus cannot achieve, the identical defect
class as the `DATA_RATE` bug, just on a different ComParam.

`j2534-0404-service/docs/comparam-protocol-support.md`'s "Bus Type and
Protocol Name Default ComParams" table had already drifted independently: it
listed `ISO_11898_3_DWFTCAN`'s `CP_Baudrate` default as `250 000` — neither
the code's `500_000` nor spec's `125_000` — so no reader could have used it
to catch either defect.

## Decision

- `iso_11898_3_dwftcan`'s `DATA_RATE` default changes from `500_000` to
  `125_000`, matching Table B.21's `CP_Baudrate` row for this bus type.
- `iso_11898_3_dwftcan` no longer populates a `PARAM_CAN_BAUDRATE_RECORD`
  default at all, matching Table B.21's `CP_CanBaudrateRecord` row, which
  defines no default for this bus type. `SetComParam` still accepts the
  param post-creation (`is_can_param` treats it as valid for the whole CAN
  protocol family, protocol ID `0x05`, per `comparam-protocol-support.md`);
  only the unauthored default is removed.
- `comparam-protocol-support.md`'s default-ComParams table is corrected to
  `125 000` for `ISO_11898_3_DWFTCAN`, matching the fixed code.

## Alternatives Considered

1. **Keep a `CP_CanBaudrateRecord` default for FT-CAN, lowered to `125_000`
   to stay within the physical limit.** Rejected: Table B.21 defines no
   default for this bus type at all, so inventing any value — even one
   inside the physical envelope — is not a spec-conformant default; a client
   relying on `GetComParam` returning a spec default would get a fabricated
   one.
2. **Fix only the cited `DATA_RATE` default, leave `CP_CanBaudrateRecord`
   untouched.** Rejected: this would leave a freshly-fixed function in an
   internally contradictory state (a 125k physical bus advertising a 250k
   entry in its own baud-rate record) discovered in the course of fixing the
   very defect this ADR addresses — deferring it would just re-surface the
   same finding under a new ID.

## Consequences

- A client that creates a CLL on `ISO_11898_3_DWFTCAN` and calls
  `GetComParam(CP_CanBaudrateRecord)` before ever calling `SetComParam` for
  it now gets an empty Bytefield (see Amendment below for why this needed a
  second fix), not a fabricated `{250000}` entry. Table B.21 gives no
  guidance for this case since it defines no default, so this is the
  conformant behavior, not a regression.
- Spec-edition caveat (per this repository's standing policy): the clause
  and table citations above are ISO 22900-2:2009(E). A 2022 revision of
  Table B.21 is possible and has not been checked against this decision.

## Amendment (Codex review, PR #142): `rpc_get_com_param` reported the wrong oneof variant for the now-absent default

The Decision section above assumed that removing `iso_11898_3_dwftcan`'s
`CP_CanBaudrateRecord` entry would simply leave `GetComParam` reporting an
"empty/absent" value. A post-open Codex review round found this was wrong:
`rpc_get_com_param` (`rpc_link.rs`) falls through to a generic
`Unum32(0)` response for *any* param absent from both the `structfield` and
`bytes` Working maps, with no awareness that `CP_CanBaudrateRecord` is
declared a Bytefield. Before this ADR, `ISO_11898_3_DWFTCAN` never hit this
path (it always had a `bytes` entry, if a wrong one); this ADR's own fix
newly exposed it for that bus type -- and it already latently affected
`SAE_J2411_SWCAN`, which has never populated a `CP_CanBaudrateRecord`
default at all (Table B.21 defines none for it either). A client reading
`CP_CanBaudrateRecord` on either bus type before ever calling `SetComParam`
would get the wrong `ParamData` oneof variant (`Unum32(0)` instead of
`Bytefield`), breaking any client that type-checks the response before
decoding it.

**Fix:** `comparam_support.rs` gains `is_bustype_bytes_param`, a `pub(super)`
accessor over the existing `BUSTYPE_BYTES` registry (already used by
`bustype_params_differ`/`apply_bustype_lock`/`strip_bustype_keys` for the
same "which physical-layer ComParams are Bytefield-typed" question).
`rpc_get_com_param`'s fallback chain now checks this before defaulting to
`Unum32`: a `BUSTYPE_BYTES` member absent from the Working `bytes` map
reports an empty `Bytefield`, not `Unum32(0)`. This is a general fix at the
registry level, not a per-bus-type special case, so it also closes the
pre-existing `SAE_J2411_SWCAN` gap without a separate change. Regression
test: `j2534-0404-service/tests/grpc_mock/resources.rs::create_com_logical_link_a2_13_ftcan_comparam_defaults`.

## Amendment 2 (Codex review, PR #142): the getter fix could mask an accepted mismatched write

A second review round on the fix above found it introduced a new failure
mode. `rpc_set_com_param`'s `Unum32` branch (`rpc_link.rs`) has never
validated that `param_id` is actually declared `Unum32`-typed -- it only
runs the hardware-flavor allowlist (`check_param_allowed`) plus a handful of
param-specific range checks (`CP_UartConfig`, `CP_Parity`, etc.), then
stores unconditionally into `working.unum32`. So
`SetComParam(CP_CanBaudrateRecord, Unum32(_))` was always silently
accepted into the wrong map. Before Amendment 1's fix this was harmless in
practice: `GetComParam`'s final fallback reads `working.unum32` for
anything not found in `structfield`/`bytes`, so it would read back exactly
the value the client set -- type-blind, but self-consistent. Amendment 1's
new `is_bustype_bytes_param` branch runs *before* that final fallback,
so for a `BUSTYPE_BYTES` member it now unconditionally returns an empty
`Bytefield` regardless of whether `working.unum32` actually holds a
client-supplied value -- silently discarding an accepted write instead of
echoing it back.

**Fix:** `rpc_set_com_param`'s `Unum32` branch now rejects
`comparam_support::is_bustype_bytes_param(param_id)` with
`InvalidArgument` before ever reaching `working.unum32.insert`, mirroring
the existing type/range checks in the same branch (`CP_UartConfig`,
`CP_Parity`, `CP_InitializationSettings`, the two 5-baud address params).
This closes the mismatch at its actual source (the setter accepting a
value of the wrong declared type) rather than papering over it at the
getter, and removes the race Amendment 1's fix depended on not existing.
Regression test (extended): the same
`create_com_logical_link_a2_13_ftcan_comparam_defaults` test now also
asserts `SetComParam(CP_CanBaudrateRecord, Unum32(_))` is rejected with
`InvalidArgument`.

## Amendment 3 (Codex review, PR #142): the empty-Bytefield representation itself broke `temp_param_update`

A third review round found a further consequence of Amendment 1's
`GetComParam` fix. A client that reads the new empty-`Bytefield` response
and, per a common naive save/restore pattern, writes it straight back via
`SetComParam` now stores an explicit empty entry in `working.bytes` --
`rpc_set_com_param`'s `Bytefield` branch accepted it unconditionally
(Amendment 2 only added a check for the mismatched-*type* case, not this
same-type-empty-value case). `active.bytes` never gained a matching entry
(nothing ever promoted it there), so `bustype_params_differ`'s direct
`working.bytes.get(&id) != active.bytes.get(&id)` comparison now saw
`Some(&vec![])` on one side and `None` on the other -- a real difference by
that comparison's logic, even though the client changed nothing relative to
what `GetComParam` had just told them. This would reject a subsequent
`temp_param_update=1` COP with `PDU_ERR_TEMPPARAM_NOT_ALLOWED` for a client
that did nothing wrong.

**Fix:** `rpc_set_com_param`'s `Bytefield` branch now normalizes an empty
write to "absent" specifically for `is_bustype_bytes_param` members: an
empty `bytes` payload removes the `working.bytes` entry instead of
inserting an empty `Vec`. This keeps "no value" and "explicit empty list"
identical for `CP_CanBaudrateRecord`, which Table B.21 never distinguishes
in the first place. The normalization is intentionally scoped to
`is_bustype_bytes_param` rather than applied to every Bytefield param:
several `comparam_defaults.rs` presets (e.g. `CP_TesterPresentMessage`) use
an explicit empty `Bytefield` as a meaningful, distinct default value, and
those params are not `BUSTYPE_BYTES`-class (`bustype_params_differ` never
inspects them), so widening the normalization would silently change
unrelated behavior for no benefit. Regression test:
`j2534-0404-service/tests/grpc_mock/resources.rs::set_com_param_empty_baudrate_record_round_trip_stays_temp_param_update_safe`.
