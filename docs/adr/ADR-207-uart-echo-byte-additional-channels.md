# ADR-207: SAE J2534-2 Additional Channels (_CHx) for UART Echo Byte

**Date:** 2026-09-01
**Status:** Accepted
**Affects:** `j2534-0404/src/lib.rs`, `j2534-0404-service/src/service/resources.rs`,
`j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_misc.rs`,
`j2534-0404-mock/src/lib.rs`, `j2534-0404-service/tests/grpc_mock/uart_echo_byte.rs`,
`docs/j2534-2-support-plan.md`, `j2534-0404-service/docs/implementation-notes.md`

## Context

[ADR-206](ADR-206-j1939-additional-channels.md) closed SAE J1939's own
`_CHx` gap and split the remaining nine out-of-scope protocol families into
two groups: four "ready now, no design pass needed" (UART Echo Byte, Honda
DIAG-H, SAE J1708, TP2.0 — each a standalone `_PS`-only hardware id with no
cross-family collapse, the same shape GM UART/J1939 already proved out) and
five "needs a dedicated design/audit pass first" (the CAN-family-collapse
group, where `is_fd_protocol_id`/`is_sw_protocol_id`/`is_ft_protocol_id`
would need a call-site audit before `_CHx` support is safe). This ADR picks
UART Echo Byte (SAE J2534-2 clause 12, Phase 9/[ADR-170](ADR-170-j2534-2-uart-echo-byte-phase9.md))
as the next family from the first group — the same "one family per PR"
scoping ADR-206 Decision item 6 established, and Phase 9's own earliest
position among the three simplest remaining standalone protocols (Honda
DIAG-H/Phase 10, SAE J1708/Phase 11 remain equally ready; TP2.0/Phase 7 is
deliberately skipped as the most structurally complex of the four, spanning
three stages/ADRs).

ADR-206's own Decision item 4 flagged a specific risk for any future
session picking one of these four families: J1939's own `names.rs` arm had
been deliberately widened (Codex review, PR #72 round 13) to intercept and
reject a raw `_CHx` id, since `chx_block_base` didn't yet resolve one —
closing J1939's gap required narrowing that arm back to an exact `_PS`-id
match (mirroring GM UART's own arm shape) before `_CHx` could work
end-to-end. Investigated directly against the actual code before writing
this ADR's first draft: UART Echo Byte's own arm (`names.rs`'s
`resolve_pin_selection`, the `if resources::is_uart_echo_byte_protocol_id
(raw_hw_protocol_id)` gate) already uses the exact-`_PS`-match shape, never
widened to the `_CH1..128` range the way J1939's own predicate was — so the
specific J1939-shaped risk ADR-206 flagged does not recur here.

**That first-draft investigation was incomplete, however — a second,
different `names.rs` gap surfaced during implementation and is fixed by
this ADR too (Decision item 8 below).** GM UART's and J1939's own arms
each have a `requested_index.is_some()` bypass at the top of the arm,
returning `Ok(None)` (with a pins-mutual-exclusion rejection first, if
`dlc_pin_data` is also non-empty) so `resolve_channel_selection`'s own
generic `_CHx` handling can run for the compound-name connect route.
UART Echo Byte's arm had no such bypass at all — checked for the
*existence* of an over-broad outer gate (the J1939-shaped risk) but not for
the *absence* of this narrower, independent bypass, which is a different
failure mode with the same root cause (this arm was written before
`_CHx` was in scope for this family, so it never had to account for a
compound-name resolution reaching it). Without it, a compound name like
`"...CH1"` reaches this arm with `raw_hw_protocol_id ==
PROTOCOL_UART_ECHO_BYTE_PS` (a matched `_PS` row) and empty
`dlc_pin_data`, falls into the "apply the row's own default pin" branch,
and returns `Ok(Some((.., 0x0000_0700)))` instead of `Ok(None)` —
`resolve_channel_selection`'s own clause-6/clause-7 mutual-exclusion check
then sees a non-`None` `pin_selection` and rejects the connect outright.
The raw-numeric-id route is unaffected (a directly-named
`PROTOCOL_ECHO_BYTE_CH1` id never matches this arm's own
`raw_hw_protocol_id == PROTOCOL_UART_ECHO_BYTE_PS` guard in the first
place, so it already fell through correctly). Caught by the implementer's
own second new test (`connecting_via_compound_chx_name_succeeds`) failing
during verification — exactly the kind of genuinely new, unanticipated
finding this repo's own convention says to stop and report rather than
guess at, which is what happened; this ADR was revised with the fix
before proceeding, not patched around it silently.

**A further, independent gap surfaced after the Decision-item-8 fix landed,
during the standard post-implementation `edge-case-hunter` verification
pass, and is fixed by this ADR too (Decision item 10 below).**
`resources.rs`'s
`connect_discovery_check` — the ADR-185 Stage-1 Discovery fail-fast
pre-check consulted before native `PassThruConnect` — had a bare
exact-match arm for `PROTOCOL_UART_ECHO_BYTE_PS`, unlike GM UART's own arm
(`id if is_gm_uart_protocol_id(id) => ...`), which is deliberately
range-inclusive because GM UART was already "the first Stage-1 DeviceFlag
family with in-scope `_CHx` Additional Channels" (that arm's own comment).
UART Echo Byte's arm was never widened to match when this ADR's own item 2
made it a `_CHx` family too, so a `_CHx` connect silently skipped the
Discovery-based fail-fast rejection the `_PS` route still enforces —
directly contradicting this ADR's own first-two-drafts' claim, in Decision
item 4, that "no other `resources.rs` function changes" were needed. A
follow-up sweep of every other raw-`hw_protocol_id` consumer of
`is_uart_echo_byte_protocol_id` (as opposed to a normalized
`base_hw_protocol_id()`/`ChannelProtocol`-keyed consumer, inherently safe
since `ChannelProtocol` has no separate `_CHx` variant) found three more
call sites with the identical gap: `bustype_default_name_for_hw_protocol_id`
(`resources.rs`), and both the START and QUERY/STOP Repeat Messaging
clause-12.3.3.1 rejections in `rpc_misc.rs`. All four are fixed the same
way, by a single new range-inclusive predicate
(`is_uart_echo_byte_family_protocol_id`) — see Decision item 10. This is
the third round in a row this ADR's own investigation was incomplete on
first pass (Decision item 8's `names.rs` bypass being the second); each was
caught before merge by the layered process this repo's convention already
mandates (a failing self-written test, then `edge-case-hunter`, then a
deliberate follow-up sweep triggered by the second finding), not by luck.

**A second risk ADR-206 hit (CAN FD's `apply_fd_mode` interaction) was
checked directly and does not apply here either.** UART Echo Byte has no
CAN-family relationship at all (clause 12 defines its own standalone
framing), no `hw_protocol_override`/`base_protocol_id` collapse arm, and no
downstream state machine (mirroring `apply_fd_mode`'s own shape) keyed on
its exact `_PS` id instead of composing through `base_protocol_id` — the
same conclusion GM UART's and J1939's own additions already reached for
their own, structurally identical families.

**One genuine gap this investigation found, distinct from anything ADR-206
hit:** `PROTOCOL_UART_ECHO_BYTE_CH1`, the constant a naive search for that
exact name would expect, does not exist — but this is a naming
inconsistency in the vendor header, not a missing binding. The header
(`j2534-0404-sys/src/bindings/j2534_v0404.h`) already defines the full
`_CH1..128` block for this family under a *different* prefix,
`PROTOCOL_ECHO_BYTE_CH1`..`PROTOCOL_ECHO_BYTE_CH128` (`0x00009600`..
`0x0000967F`), while the `_PS` id alone uses the full
`PROTOCOL_UART_ECHO_BYTE_PS` name — confirmed present, unchanged, in all
five pre-committed target bindings (`x86_64-unknown-linux-gnu.rs`,
`x86_64-pc-windows-msvc.rs`, `x86_64-pc-windows-gnullvm.rs`,
`i686-pc-windows-gnullvm.rs`, `armv5te-unknown-linux-gnueabi.rs`). **No
header edit and no bindgen regeneration are needed** — the constant this
ADR's code uses is `PROTOCOL_ECHO_BYTE_CH1`, not a name that would need
minting. It is, however, not yet re-exported from `j2534-0404/src/lib.rs`
(unlike `PROTOCOL_J1939_CH1`/`PROTOCOL_GM_UART_CH1`, which already are) —
that re-export gap is real and part of this ADR's own scope.

## Decision

1. **`j2534-0404/src/lib.rs` re-exports `PROTOCOL_ECHO_BYTE_CH1`**, mirroring
   the existing `PROTOCOL_J1939_CH1`/`PROTOCOL_GM_UART_CH1` re-exports
   (same file, same shape) — this is the one piece of new FFI surface this
   ADR needs; `PROTOCOL_UART_ECHO_BYTE_PS` is already re-exported.
2. **`resources.rs`'s `chx_block_base` gains a tenth entry**:
   `j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS =>
   Some(j2534_0404::PROTOCOL_ECHO_BYTE_CH1)` — keyed by its own `_PS`-only
   id, the same shape GM UART's and J1939's own entries use (clause 12
   defines no unqualified base id either).
3. **`chx_base_protocol_id`'s `BLOCKS` array gains a tenth entry**:
   `(j2534_0404::PROTOCOL_ECHO_BYTE_CH1, j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS)`,
   widening `[(u32, u32); 9]` to `[(u32, u32); 10]`.
4. **No changes to `chx_protocol_id`/`is_chx_protocol_id`/
   `is_uart_echo_byte_protocol_id` themselves** — `chx_protocol_id` derives
   from `chx_block_base` unchanged; `is_chx_protocol_id` already covers the
   full 18-family/19-block region; `is_uart_echo_byte_protocol_id` (Context,
   above) keeps the correct exact-match shape it already had, unlike
   J1939's own predicate before ADR-206 — it stays deliberately narrow for
   `names.rs`'s own arm-gate purpose (see Decision item 10 for the
   *different* function this ADR adds for `resources.rs`'s other,
   family-wide callers).
5. **`j2534-0404-mock/src/lib.rs`'s own crate-local duplicate tables**
   (`BASE_PROTOCOL_IDS`/`CHX_BLOCK_BASE_IDS`) get the matching tenth entry
   — the same ADR-157 propagation duty ADR-206 already paid down for
   J1939.
6. **No split of the mock's `is_uart_echo_byte_protocol` predicate is
   needed** (unlike ADR-206's `is_j1939_protocol`/`is_j1939_family_protocol`
   split for J1939's own `ERR_ADDRESS_NOT_CLAIMED` enforcement) — UART Echo
   Byte has no family-wide behavioral enforcement anywhere in the mock
   beyond pin-gating (`ChannelState::new`'s `pins_assigned` computation,
   which stays keyed on the existing exact-match predicate, correctly
   excluding a `_CHx` id the same way GM UART's own `protocol_id !=
   PROTOCOL_GM_UART_PS` check already does). Investigated directly: no
   `PassThruWriteMsgs`/`PassThruIoctl` call site in the mock consults any
   UART-Echo-Byte-specific predicate besides this one pin-gating check.
7. **The mock's existing `DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED` Discovery
   arm is left as-is** (the flat `0x0000_0001` shape, not `chx_capacity`-
   packed) — this mirrors GM UART's own identical choice, not a gap: only
   the original seven CAN-family `_SUPPORTED` params pack `chx_capacity`
   into their value; GM UART's own dedicated arm (needed for its
   `gm_uart_unsupported` test backdoor) already uses the same flat shape
   this family's arm does, so UART Echo Byte joining that precedent is
   consistent, not newly deficient. Unlike J1939 (ADR-206's own accepted
   limitation: no Discovery arm existed for it at all), UART Echo Byte's
   Discovery advertisement already correctly reports it supported —
   `_CHx` clients gain nothing new here, but lose nothing either.
8. **`names.rs`'s UART Echo Byte arm in `resolve_pin_selection` gains a
   `requested_index.is_some()` bypass** at the top of the arm (before the
   `!j2534_2_opted_in` check), mirroring GM UART's own bypass verbatim in
   shape (`names.rs:1361-1372` before this ADR's own edits): reject if
   `dlc_pin_data` is also non-empty (clause 7 channels have no J1962 pin
   concept, so the two are mutually exclusive), otherwise `return
   Ok(None)` so `resolve_channel_selection`'s own generic `_CHx` handling
   runs for the compound-name route instead. This is a NEW gap this ADR's
   own first-draft investigation missed (see Context's own account of
   finding it during implementation) — distinct from, but the same root
   cause as, the J1939-shaped risk ADR-206 already taught this session to
   check for.
9. **Scope this PR to UART Echo Byte alone**, not the other three
   "ready now" families in the same change — mirrors ADR-206 Decision
   item 6's own single-family-per-PR precedent.
10. **`resources.rs` gains a new `is_uart_echo_byte_family_protocol_id`
    predicate** (`_PS` id OR the full `ECHO_BYTE_CH1..128` range, mirroring
    `is_gm_uart_protocol_id`'s exact shape) for the four call sites that
    must treat a `_CHx` link identically to its `_PS` sibling for
    family-wide behavioral checks unrelated to pin selection —
    `connect_discovery_check`'s Discovery-gating arm (widened from a bare
    exact-match to `id if is_uart_echo_byte_family_protocol_id(id) => ...`,
    mirroring GM UART's own arm), `bustype_default_name_for_hw_protocol_id`,
    and both the START and QUERY/STOP Repeat Messaging clause-12.3.3.1
    rejections in `rpc_misc.rs`. The narrower, arm-gate-only
    `is_uart_echo_byte_protocol_id` predicate (Decision item 4) is
    deliberately left unchanged and untouched at every one of its own call
    sites — widening it instead of adding a second predicate would have
    broken `names.rs`'s own arm-gate purpose, which must NOT match a
    `_CHx` id (that's precisely what lets a `_CHx` connect fall through to
    `resolve_channel_selection`'s generic handling in the first place).
    Found by an `edge-case-hunter` verification pass (the
    `connect_discovery_check` gap) followed by a targeted sweep of every
    other raw-`hw_protocol_id` consumer of `is_uart_echo_byte_protocol_id`
    for the same failure shape (the other three call sites) — see Context.

## Alternatives rejected

- **Picking TP2.0 instead**, since it is technically also in the "ready
  now" group. Rejected — TP2.0 is the most structurally complex of the
  four (three stages/ADRs: active connections, passive connections,
  broadcast/periodic re-trigger), a materially higher-risk starting point
  than the three single-ADR standalone protocols share; picking the
  simplest of the four first keeps this ADR's own risk profile close to
  GM UART's/J1939's already-verified shape.
- **Minting a new `PROTOCOL_UART_ECHO_BYTE_CH1` constant** instead of using
  the header's own existing `PROTOCOL_ECHO_BYTE_CH1` name. Rejected —
  `_CH1..128` already exists under that name in the vendor header and every
  pre-committed target binding; inventing a same-valued alias would
  duplicate an existing FFI symbol for no reason and diverge from the
  header's own naming, which this codebase treats as authoritative
  (`*-sys` crates re-export bindgen's own generated names verbatim).

## Consequences

- UART Echo Byte `GetResourceIds`/`_CHx` clients are unlocked: a
  directly-named `PROTOCOL_ECHO_BYTE_CH1`..`_CH128` id (or the equivalent
  compound `"...CH1"`-suffixed name) now resolves and connects, mirroring
  GM UART's and J1939's own client-visible capability.
- `_CHx` Additional Channels now covers 10 of 18 protocol families.
- The backlog's remaining `_CHx` item (`implementation-notes.md`'s
  ADR-206-citing P2) is updated: UART Echo Byte moves out of the
  "ready now" list (now three families: Honda DIAG-H, SAE J1708, TP2.0)
  into the closed set, alongside GM UART and J1939.
- Unlike ADR-206, this ADR closes with **no new accepted-limitation
  residuals** of its own — every gap found across all three rounds of
  investigation (the compound-name `names.rs` bypass, Decision item 8; the
  `connect_discovery_check` Discovery-gating gap and its three sibling
  raw-`hw_protocol_id` call sites, Decision item 10) is fixed here, not
  deferred. The specific J1939-shaped outer-gate risk ADR-206 flagged, and
  the Discovery-advertisement gap ADR-206 had to document as an
  out-of-scope limitation for J1939, both turned out not to apply to UART
  Echo Byte — but this ADR's own first-draft claims that "no other
  `resources.rs` function changes" were needed, and that "`names.rs`
  needed no change at all," both turned out wrong, in two different ways
  (Context's own account of both). The corrected version, reflected here,
  is that `names.rs` needed one small bypass fix (Decision item 8) and
  `resources.rs` needed one new family-wide predicate applied at four call
  sites (Decision item 10) — a materially larger surface than either draft
  claimed, found only because each round's fix was independently verified
  (a self-written test, then `edge-case-hunter`, then a deliberate
  follow-up sweep) rather than trusted on the strength of the prior
  investigation alone. This ADR's own history is itself the concrete
  argument for continuing to run that full verification sequence on future
  `_CHx` extensions, not a shortcut to skip because GM UART's/J1939's own
  additions went more smoothly. The cross-cutting RawMode-allowlist gap
  ADR-206 already recorded (applies identically to all `_CHx`-supported
  families, UART Echo Byte included, since it was never in that allowlist
  to begin with) is not re-recorded here — it is already tracked once,
  generically, in `implementation-notes.md`'s existing P3.
