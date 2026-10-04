# ADR-096: ISO-TP Tester-Present Suppression Keys on Wire Target, Not CLL Identity

**Date:** 2026-07-16
**Status:** Accepted
**Affects:** `j2534-0404-service` events (`isotp_send`, `dispatch_due_tester_present`)

## Context

ADR-095 added `exclude_cll: Option<u32>` to `dispatch_due_tester_present`, used by `isotp_send`'s
FlowControl-wait and STmin/ConsecutiveFrame-block loops to keep the in-flight CLL's own mode-0
tester-present from firing mid-transfer: a same-CAN-ID SingleFrame tester-present spliced into the
middle of a FirstFrame/ConsecutiveFrame sequence looks like an unexpected SF to the receiving ECU
(ISO 15765-2), aborting its in-progress reassembly.

A Codex review finding on this same PR (#97) caught that this only excludes by CLL *identity*, not
by *wire target*: two different CLLs can share one physical channel (`SharedChannel`, same
protocol+baud) while independently configured to address the same target ECU (same
`CP_CanPhysReqId`) — nothing in this codebase's CLL/addressing model prevents that configuration.
If a sibling CLL's mode-0 tester-present targets the same physical address as another CLL's
in-flight software-ISO-TP transfer, `exclude_cll` (which only matches the transferring CLL's own
handle) does not exclude it, and the exact splice failure ADR-095 protects against for the
same-CLL case remains reachable for a different CLL targeting the same ECU.

## Decision

### Suppress by frozen TX wire identity, not `resolved.target_can_ids`

The natural first instinct — reuse `ResolvedTesterPresent::target_can_ids` (ADR-088's
`CP_CanRespUSDTId`/`CP_CanRespUUDTId`) — is wrong: that field is the ECU→tester *response* address,
which never appears in a sibling's own *transmitted* tester-present frame. Comparing it would miss
real collisions (same `CP_CanPhysReqId`, different response IDs) and false-positive on the reverse.
The actual collision is TX-side: does the candidate sibling's own tester-present frame, if sent right
now, carry the same CAN ID onto the wire as the in-flight transfer's own target?

A new, `Copy` struct captures that:
```rust
struct InFlightIsoTpTarget { can_id: [u8; 4], can_29bit: bool }
```
passed as a new, independent `exclude_isotp_target: Option<InFlightIsoTpTarget>` parameter on
`dispatch_due_tester_present`, alongside — not replacing — `exclude_cll`. `isotp_send` builds this
once from its own already-available outgoing CAN ID bytes and `tx_flags` (the same locals it already
uses to build the frame it's sending), and passes it at both of its own call sites. Every other call
site (`handle_delay`, `wait_for_expected_response`, the RC21/23 wait, `run_due_tick_duties`, and the
pre-init top-up added for the round-5 fix) passes `None` — none of them have a transfer in flight to
protect.

### The comparison is against the candidate's own frozen `framed_data`, not a live re-resolution

The due-snapshot filter skips a candidate `Armed` CLL when its own `framed_data[..4]` (the CAN ID
bytes its tester-present would actually transmit — already frozen at that CLL's own arm/re-arm time)
equals the in-flight target's `can_id`, and the CAN 29-bit-ID flag bit in `resolved.tx_flags` matches
`can_29bit` (so an 11-bit `0x7E0` doesn't alias a 29-bit `0x000007E0`). This reuses the *pattern*
ADR-088's amendment established after three review rounds of getting addressing comparisons wrong on
this exact subsystem (frozen raw identity, not a live table lookup or a re-labelable field) — but not
its specific field, since the field that actually collides on the wire here is different (TX bytes,
not the response-side `target_can_ids`). Comparing the frozen `framed_data` the candidate would
literally transmit is stronger than ADR-088's own "frozen at arm time" argument: there is no drift to
worry about, because the compared value *is* the value that would go on the wire if this candidate
fired right now — not a cached description of it.

This is a point-in-time skip decision, re-evaluated fresh from a fresh snapshot on every FC-wait tick
and before every ConsecutiveFrame, not a value cached across multiple future uses — so the "must not
go stale" concern that drove ADR-088's frozen-vs-live discipline does not apply the same way here; a
re-arm that retargets a sibling mid-transfer takes effect on the very next dispatch call regardless.

### Applies through the shared due-snapshot filter, covering both modes

Placing the check in `dispatch_due_tester_present`'s shared filter (rather than duplicating it at
each call site) automatically covers both tester-present modes: a mode-0 sibling is directly
protected by the exclusion; a mode-1 sibling is *mostly* already deferred by the transfer's own
`last_bus_activity` stamps, but the FlowControl-wait itself writes nothing to the bus (a default N_Bs
of 1000ms with no traffic), so a short-interval mode-1 sibling targeting the same ECU could still
come due mid-wait without this same filter catching it too.

## Consequences

- A same-target sibling CLL's tester-present is deferred for up to the in-flight transfer's own
  duration (bounded — the same order of magnitude as the same-CLL exclusion ADR-095 already
  accepted), self-correcting on the very next dispatch opportunity once the transfer completes.
- **Accepted residual — extended addressing.** With ISO 15765-2 extended addressing, the N_AI
  includes an extension byte (`framed_data[4]`) this CAN-ID-only comparison does not check —
  over-suppression only (a same-CAN-ID-but-different-extension-byte sibling is deferred
  unnecessarily, never under-suppressed), and bounded to the transfer's own duration. Not closed now;
  revisit only if a future finding shows this actually matters in practice.
- Functionally-addressed tester-present and physically-addressed siblings targeting a genuinely
  different CAN ID are unaffected — proven by a companion regression test alongside the same-target
  suppression test.
- `exclude_cll` and `exclude_isotp_target` are independent, orthogonal exclusion mechanisms on the
  same function; keeping them separate (rather than merging into one struct parameter) keeps every
  non-`isotp_send` call site's diff to "pass `None`" instead of re-justifying a merged parameter's
  shape at every site.
