# ADR-071: CP_UartConfig Decodes to DATA_BITS + PARITY

**Date:** 2026-07-09
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/rpc_link.rs` (`apply_j2534_params`, `rpc_set_com_param`, `rpc_get_com_param`), `j2534-0404-service/src/service/events.rs`, `j2534-0404-service/src/service/comparam_support.rs`

## Context

`CP_UartConfig` (ComParam ID `0x20`) is a `PDU_PC_BUSTYPE`-class ComParam
required (`S`) on ISO 9141-2, ISO 14230-1, SAE J2610, and SAE J1708 (UART
family). ADR-027's numeric overlap between D-PDU ComParam IDs and native
J2534 config IDs means `0x20` is also the native `DATA_BITS` `SET_CONFIG`
parameter ID.

Before this change, `j2534-0404-service` treated the value stored under ID
`0x20` as an already-native `DATA_BITS` value and forwarded it unchanged to
`PassThruIoctl SET_CONFIG` on ISO9141/ISO14230 channels. This was wrong: per
ISO 22900-2, `CP_UartConfig` is a *combined* encoding of data bits, parity,
and stop bits (`0..=17`) — not a `DATA_BITS`-shaped value at all. Native
J2534 v04.04 has no single equivalent parameter; it exposes `DATA_BITS`
(`0x20`, 7 vs. 8 data bits only) and `PARITY` (`0x16`, N/O/E) as two
independent `SET_CONFIG` registers, with no stop-bit parameter (the spec
implies 1 stop bit). This mismatch was previously documented as a known
limitation (`comparam-protocol-support.md`'s "⚠️ CP_UartConfig" note): the
parity portion of a client's `CP_UartConfig` value was silently lost unless
the client separately set the J2534-specific `CP_Parity` alias.

Additionally, the seeded UART/K-line `comparam_defaults.rs` presets already
stored `6` under `ComParamId(DATA_BITS)` — an out-of-range value for the old
"raw `DATA_BITS`" interpretation (valid native range is `0..=1`), but exactly
the `CP_UartConfig` encoding for `8N1` (8 data bits, no parity, 1 stop bit),
the correct default for K-line. This preset value's numeric coincidence with
the new encoding did not need changing (see Consequences).

## Decision

Reinterpret the value stored under ComParam ID `0x20` (`CP_UartConfig`,
still name-aliased as `data_bits` in `names.rs` for backward compatibility)
as the ISO 22900-2 `CP_UartConfig` combined encoding, not a raw `DATA_BITS`
value. Working-set storage and `GetComParam` are symmetric with this: they
always read/write the `CP_UartConfig` encoding, regardless of alias name
used.

**Accepted value set.** `CP_UartConfig`'s `0..=17` range groups into 6
`(data bits, stop bits)` combinations of 3 parity values each. J2534 v04.04
cannot represent 2-stop-bit configurations (no stop-bit `SET_CONFIG` param
at all) or 9-data-bit configurations (`DATA_BITS` only distinguishes 7 vs.
8). Only the two 1-stop-bit groups are therefore representable: `0,1,2`
(7N1/7O1/7E1) and `6,7,8` (8N1/8O1/8E1). `SetComParam` rejects every other
`CP_UartConfig` value (`3,4,5` / `9,10,11` / `12..=17` / anything `>17`) with
`Status::invalid_argument`, rather than silently truncating it — matching
this service's existing convention of failing closed on unsupported
ComParam/protocol/value combinations (`comparam_support::check_param_allowed`).

**Forwarding split.** At both hardware-forwarding call sites
(`rpc_link.rs::apply_j2534_params`, called at `ConnectComLogicalLink`; and
`events.rs::apply_params_to_hardware`, called at `CoptUpdateparam`), a new
`comparam_id::expand_uart_config` step runs on the `(config_id, value)` list
already produced by `to_j2534_config_id`/`to_j2534_config_value`. It rewrites
a `DATA_BITS` entry that originated from `CP_UartConfig`'s numeric overlap
into **two** entries: `DATA_BITS` (the decoded data-bits value) and `PARITY`
(the decoded parity value), via two small decode helpers,
`uart_config_to_data_bits`/`uart_config_to_parity`. The existing
`to_j2534_config_id` protocol gate (ADR-028) is unchanged: `DATA_BITS`/
`PARITY` are still only forwarded for ISO9141/ISO14230 channels; on SCI
channels `CP_UartConfig` is accepted and range-checked the same way but
remains stored-only, matching prior behavior.

**Explicit-`CP_Parity`-wins precedence.** If the same param collection also
contains an explicit `PARITY` (`0x16`, `CP_Parity`) entry — set independently
via `SetComParam`, not derived from `CP_UartConfig` — that explicit value is
kept and the `CP_UartConfig`-derived parity is dropped instead of appended.
This is a deterministic precedence rule rather than "last write wins," which
would depend on `ComParamSet.unum32`'s `HashMap` iteration order and produce
a nondeterministic result depending on hash state. Explicit `CP_Parity` is
the more specific of the two (a client that bothers to set it directly is
making an explicit override), so it wins unconditionally.

**`CP_Parity` (`0x16`) range validation.** `SetComParam` also now rejects an
explicit `CP_Parity` value outside `0..=2` (the native `PARITY` range;
default `0`) with `Status::invalid_argument`, closing the same class of gap
`CP_UartConfig` had — previously an out-of-range `CP_Parity` value would have
been forwarded to hardware unchecked.

**`GetComParam(0x16)` derives from `CP_UartConfig` when unset (roundtrip
idempotence).** `rpc_get_com_param` originally returned `0` for any Unum32
ComParam with no explicit Working entry, including `CP_Parity`. Combined
with the write-side "explicit `CP_Parity` wins" precedence above, this made
a save/restore roundtrip lossy: a client reading back both `0x20` and `0x16`
via `GetComParam` (to persist a config) and later replaying both through
`SetComParam` (to restore it) would write an explicit `CP_Parity = 0` that
had never actually been set, which then silently overrode the
`CP_UartConfig`-derived parity on restore — e.g. `CP_UartConfig = 8` (8E1)
would come back as 8N1 after a roundtrip. `rpc_get_com_param` now special-
cases `PARITY` (`0x16`): when there is no explicit `0x16` entry in Working,
it derives the value from the Working `CP_UartConfig` (`0x20`) entry via
`uart_config_to_parity`, falling back to `0` only when neither is set. This
makes `GetComParam(0x16)` always report the currently *effective* parity,
and makes the save/restore roundtrip idempotent. This is a read-side-only
change — the write/forward-side "explicit `0x16` wins" precedence in
`expand_uart_config` is unchanged.

## Consequences

- **Breaking change in `0x20`'s semantics.** A caller that previously relied
  on setting `0x20` to a raw `DATA_BITS` value (`0` = 8 bits, `1` = 7 bits)
  must switch to the `CP_UartConfig` encoding (`6` for 8 bits, `0` for 7
  bits, both implying no parity). This is the intended fix — the prior
  behavior did not match ISO 22900-2 at all — but it is a wire-visible
  behavior change for any existing client setting `0x20` directly.
- **Seeded defaults needed no change.** `comparam_defaults.rs`'s UART/K-line
  presets (`iso_14230_1_uart`, `iso_9141_2_uart`, `sae_j1708_uart`,
  `sae_j2610_uart`) already stored `6` under `ComParamId(DATA_BITS)`. Under
  the old raw-`DATA_BITS` interpretation this was an out-of-range value that
  happened to never be validated; under the new `CP_UartConfig` encoding it
  is exactly `8N1`, the correct K-line default. No preset values were
  changed by this ADR — a latent inconsistency in the old code is
  incidentally resolved as a side effect.
- **`GetComParam(0x20)` is unaffected in shape.** It continues to read the
  raw Working-set value for ID `0x20` — now consistently the `CP_UartConfig`
  encoding on both the read and write sides — without needing a companion
  decode step of its own. `GetComParam(0x16)`, however, is no longer a plain
  Working-set lookup; see the derivation rule above.
- **Known cross-encoding disagreement between `0x20` and `0x16` read-back.**
  If a client sets an explicit `CP_Parity` (`0x16`) that disagrees with the
  parity implied by the Working `CP_UartConfig` (`0x20`) value — e.g.
  `CP_UartConfig = 8` (8E1, implying `PARITY = 2`) plus an explicit
  `CP_Parity = 1` (odd) — the explicit `0x16` value wins on the hardware-
  forwarding side (actual hardware ends up running 8O1), but
  `GetComParam(0x20)` still returns `8`: `CP_UartConfig`'s stored encoding is
  never rewritten to reflect an explicit `CP_Parity` override. A caller that
  wants to know the parity actually in force must read it via `0x16`, not
  infer it from `0x20`'s stored value.
- **This ADR amends, but does not supersede, ADR-067's `PDU_PC_BUSTYPE`
  classification note.** ADR-067 documented `CP_Parity` as excluded from
  `BUSTYPE_UNUM32` because "D-PDU folds parity into `CP_UartConfig`, which
  this service maps to `DATA_BITS` alone." That mapping description is now
  outdated (`CP_UartConfig` maps to `DATA_BITS` **and** `PARITY`), but the
  exclusion itself is unchanged: `CP_Parity` remains a J2534-specific alias
  with no distinct D-PDU `CP_*` name, and its `PDU_PC_BUSTYPE` membership
  remains genuinely ambiguous, so `comparam_support::BUSTYPE_UNUM32` still
  does not include it.
- **No stop-bit or 9-data-bit support is added.** Callers needing those
  configurations still cannot represent them through this service; there is
  no J2534 v04.04 native equivalent to forward them to.
