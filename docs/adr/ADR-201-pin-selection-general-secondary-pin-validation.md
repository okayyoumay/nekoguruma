# ADR-201: General Clause 6 Pin Selection — Per-Bus Secondary-Pin Validation

**Date:** 2026-08-29
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/resources.rs`, `names.rs`

## Context

ADR-168's Phase 6 "Seventh correction" closed a pin-selection completeness
gap for the FT-CAN arm of `names.rs`'s `resolve_pin_selection`: SAE J2534-2
clause 20.2.1 enumerates FT-CAN (ISO 11898-3) as connectable on exactly two
documented pin-pairs, a closed set, so that arm now rejects anything else
outright. That same correction's own "Accepted residual" explicitly left the
general SAE J2534-2 clause 6 Pin Selection fallback path (serving
`CAN_PS`/`ISO15765_PS`/`J1850VPW_PS`/`J1850PWM_PS`/`ISO9141_PS`/
`ISO14230_PS`/`J2610_PS` — the seven ADR-156 Decision 2 in-scope base
protocols) unaddressed, noting that a correct general fix needs a real
per-bus "secondary pin required" predicate rather than a uniform pin-count
rule, since K-line's own secondary pin is genuinely optional. This residual
was tracked as a P2 entry in
`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.

Clause 6.3.3.2 Table 3's own framing leaves "is a secondary pin required for
this protocol" as a per-protocol fact the spec does not itself enumerate in
one place — unlike clause 20.2.1's closed FT-CAN pin-pair list. This is why
porting the FT-CAN closed-set-check pattern to the general path is the wrong
shape: an ordinary bus's pin choice (e.g. `CAN_PS`) is not restricted to a
small enumerated set of pairs the way FT-CAN is (clause 6.3.1's own worked
example uses an arbitrary pin pair, not one of a short enumerated list). A
real per-bus predicate, not a closed-set check, is what the general path
needs.

## Decision

Add a three-way predicate, `resources::secondary_pin_requirement(hw_protocol_id:
u32) -> Option<SecondaryPinRequirement>`, covering exactly the same ten
hardware ids `ps_protocol_id` covers (`J1850VPW`, `J1850PWM`, `ISO9141`,
`ISO14230`, `CAN`, `ISO15765`, `SCI_A_ENGINE`, `SCI_A_TRANS`, `SCI_B_ENGINE`,
`SCI_B_TRANS`), `None` for everything else:

- **`Required`** (a 1-pin selection is rejected): `CAN`/`ISO15765` (standard
  differential CAN HI/LOW — single-wire CAN is a structurally distinct
  hardware id family, `SW_CAN_PS`/`SW_ISO15765_PS`, already handled by the
  separate SW clause-9 arm elsewhere in `resolve_pin_selection`, so plain
  `CAN_PS`/`ISO15765_PS` has no legitimate 1-pin form), `J1850PWM` (Ford
  SCP, differential PLUS/MINUS), and all four SCI ids (SAE J2610 needs both
  Tx and Rx to function as a bidirectional diagnostic link — no
  listen-only/one-directional SCI shape is defined in either spec, and a
  J2534 channel is bidirectional by construction).
- **`Optional`** (1 or 2 pins both valid): `ISO9141`/`ISO14230` (K-line; the
  L-line secondary is genuinely optional for a real, common K-only
  diagnostic configuration).
- **`NeverPresent`** (a 2-pin selection, or any secondary-typed pin, is
  rejected): `J1850VPW` (GM/Chrysler Class 2) — genuinely single-wire, no
  secondary signal exists for this bus at all, unlike `J1850PWM`.

`resolve_pin_selection` (`names.rs`) enforces this immediately after
computing `pin_select` via `compute_pin_select`, testing the packed value's
low byte (the SS/secondary byte): `Required` rejects when that byte is
zero, `NeverPresent` rejects when it is nonzero, `Optional` adds no further
check. This is sound because `compute_pin_select` already rejects wildcard
pin numbers (`dlc_pin_number == 0`) on every entry before packing, so a
packed SS of `0` unambiguously means "no secondary pin was in the input,"
never a legitimate wildcard secondary.

If `ps_protocol_id(hw_protocol_id)` is `Some` but `secondary_pin_requirement`
returns `None` for the same id, that is a domain-invariant break (e.g. a
future protocol added to one match but not the other) — handled as a loud
`Status::internal` failure at the call site, never a silent skip of the new
validation, matching this file's established anti-silent-canonicalization
stance elsewhere in `resolve_pin_selection`.

## Alternatives rejected

- **Porting the FT-CAN closed-set-check pattern to the general path.** Wrong
  shape: FT-CAN's two documented pin-pairs are a closed enumeration clause
  20.2.1 states outright; the general clause 6 buses are not restricted to
  a small enumerated set of pairs, so there is no finite list to check
  against.
- **A uniform "always require 2 pins" rule.** Would wrongly reject a valid
  K-line-only connect (`ISO9141_PS`/`ISO14230_PS`) — the exact hazard the
  original backlog entry named as the reason a naive pin-count rule is
  insufficient.

## Consequences

- Closes the P2 backlog entry (and ADR-168's Seventh correction/Accepted
  residual it was cross-referenced from) — see ADR-168's Status line
  annotation below.
- A K-line single-pin connect (`ISO9141_PS`/`ISO14230_PS` with only the K
  pin supplied) now produces a `pin_select` distinct from the default K+L
  connect. This is correct, not a bug: an L-disabled connect is physically
  distinct from a K+L connect.
- `FD_CAN_PS`/`FD_ISO15765_PS` remain entirely unreachable through this
  path — the existing unconditional guard rejecting any directly-named CAN
  FD `_PS` hardware protocol id, near the top of `resolve_pin_selection`,
  precedes everything this ADR touches. Noted here for completeness, since
  an earlier version of the backlog entry this ADR closes incorrectly
  implied FD ids were in scope of the original gap; that inaccuracy was
  corrected when the entry was deleted (this PR).
- ADR-168's `**Status:**` line is annotated (not replaced) per this repo's
  partial-supersession convention: its Seventh correction/Accepted
  residual on the general clause 6 dual-wire pin-pair-completeness gap is
  resolved by this ADR, while the rest of ADR-168 remains in force
  unchanged.
