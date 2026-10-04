# ADR-197: ISO15765 RX Extended-Addressing Detection Uses RxStatus Bit 7 Too

**Date:** 2026-08-28
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`header_footer_len`,
             `poll_rx_inner`, the K-line fast-init synthetic-frame call site),
             `j2534-0404-mock/src/lib.rs` (ECU-response injection, if it
             cannot yet set RxStatus bit 7 on an RX delivery),
             `docs/adr/ADR-051-iso15765-resultdata-header-in-extra-info.md`

## Context

`events.rs`'s `header_footer_len` decides, for an ISO15765 CLL, whether a
received frame carries a plain 4-byte CAN-ID prefix or an extended 5-byte
prefix (CAN ID plus one Address Extension byte). Its only signal today is a
static lookup against `can_addressing_by_id` -- built from this CLL's own
`SetUniqueRespIdTable` entries -- checking whether the frame's CAN ID is
marked `Addressing::Extended`. A CLL with no matching table entry (a
"no-table wildcard" CLL, one that does not rely on `SetUniqueRespIdTable`
for addressing) always misses this lookup and falls back to 4 bytes, even
when the frame genuinely carries an Address Extension byte.

SAE J2534-1 §8.7.1 (Figure 43, the `RxStatus` bit table) defines bit 7,
`ISO15765_ADDR_TYPE`, with the identical meaning for RX as its TxFlags
counterpart (§8.7.3 Figure 45, bit 7): unset means no extended address, set
means the byte immediately after the CAN ID is the Address Extension. This
is a genuine per-message native signal -- `j2534_0404::ISO15765_ADDR_TYPE_STATUS`
(re-exported from `RX_FLAG_ISO15765_ADDR_TYPE`, present in every `*-sys`
binding target) -- that this crate has never consulted (confirmed by grep:
zero references anywhere in `j2534-0404-service/src/` before this ADR). It
sits outside `RX_STATUS_FLAGS_MASK`'s 5 low bits, so it is read from
`msg.rx_status()` directly rather than through the existing
`rx_status_flags: u8` byte.

This was found while auditing ADR-196 Decision item 3b (RawMode Phase 1's
internal negative-response anchors), which reuses `header_footer_len`'s
prefix-width computation for its new `raw_prefix`/`tx_prefix` mechanism --
but the underlying gap predates ADR-196 and is not RawMode-specific.
`header_footer_len`'s `header_len` output already feeds ADR-051's
RawMode=OFF header/footer split (`data_bytes`/`extra_info`), so a no-table
wildcard ISO15765 CLL using extended addressing already misclassifies its
own negative responses today, independent of RawMode: the Address
Extension byte lands at `data_bytes[0]` instead of being stripped, shifting
the true `0x7F`/SID-echo bytes one position to the right of where every
internal detector (and any client reading `data_bytes` directly) expects
them.

A concretely reachable case (verified against Figure 43, not merely
theoretical): two CLLs share one physical ISO15765 channel. CLL-A installs
an extended-addressing flow-control filter via its own `UniqueRespIdTable`.
CLL-B has no table entry for that CAN ID at all (ADR-048's "what siblings
cause the adapter to forward" sharing model) but still receives the
resulting extended-addressed frame -- with RxStatus bit 7 correctly set by
the interface -- and mis-splits it purely because its own table has no
opinion.

## Decision

**1. Extended addressing is detected as `raw && (rx_ext_addr ||
<the existing can_addressing_by_id lookup>)`** -- the two signals are
combined with OR, not one replacing the other, because they cover disjoint
delivery paths:

- The RxStatus bit is only meaningful on a delivery that actually passed
  through the native ISO15765 layer. UUDT/raw-CAN-read deliveries --
  dual-channel companion-channel reception (ADR-046), the native-mixed
  `PASS_FILTER` arm, and software-ISO-TP UUDT/raw deliveries -- are
  received as plain CAN frames with no ISO15765 layer to ever set bit 7,
  even though some of those frames are genuinely extended-addressed per
  their CLL's own `UniqueRespIdTable`. For these, the table lookup is the
  *only* correct signal, and the fix must not regress it.
- The RxStatus bit is the *only* correct signal for a no-table wildcard CLL
  (this ADR's motivating case above), where the table lookup structurally
  cannot help.

Neither signal is authoritative on its own; OR covers both regimes without
regressing either.

**2. Accepted residual, not chased further:** a CLL whose table declares a
CAN ID `Addressing::Extended` will still over-strip 5 bytes from a
genuinely normal-addressed frame on that same ID (e.g. a sibling CLL's
plain flow-control filter reusing the ID) if RxStatus bit 7 is clear on
that particular frame -- OR lets the table win. This is pre-existing
behavior (the table already made this call before this ADR) and is kept
deliberately: flipping the table to advisory-only in this direction would
make correctness depend entirely on an unaudited interface's RxStatus
reporting, the same class of untrusted-FFI-value distrust ADR-171 already
established for `ExtraDataIndex`. Revisit only if this proves reachable in
practice.

**3. Plumbing:** `poll_rx_inner` reads
`msg.rx_status() & j2534_0404::ISO15765_ADDR_TYPE_STATUS != 0` once per
native message (alongside its existing `rx_status_flags` extraction) and
passes it into `header_footer_len` as a new `rx_ext_addr: bool` parameter,
applied only inside the existing `raw` (ISO15765) arm's `extended`
computation -- `header_footer_len`'s two call sites both cover: the normal
per-frame delivery loop passes the real value; the K-line fast-init
synthetic-frame call site (where no ISO15765 frame is ever reachable)
passes `false`, mirroring that call site's own existing "no real values
available" convention. No change to `ConcatFrameMeta`/`deliveries`' shape
or ADR-196 Decision item 3b's `raw_prefix` derivation -- it reuses
`header_footer_len`'s result unchanged and inherits this fix automatically,
exactly as that Decision item's own "one already-audited computation, never
diverge" rationale intended.

**4. Scope: fixes both consumers, not just RawMode's.** The RawMode=OFF
split (ADR-051) and ADR-196 Decision item 3b's `raw_prefix` are the same
expression; fixing only the RawMode path would mean deliberately
preserving a known Figure 43 violation on the pre-existing, more heavily
used non-RawMode path for no reason other than narrower PR scope.

## Consequences

- A no-table wildcard ISO15765 CLL using extended addressing now correctly
  strips the Address Extension byte on both RawMode=OFF (ADR-051's
  `extra_info` split) and RawMode=ON (ADR-196 Decision item 3b's internal
  anchors) -- closing a pre-existing negative-response misclassification
  risk this fix's audit found, not one ADR-196 introduced.
- `j2534-0404-mock`'s existing RX-injection test backdoor
  (`inject_rx_with_status`, `tests/grpc_mock/harness.rs`) already round-trips
  arbitrary `RxStatus` bits, including bit 7 -- no mock change was needed.
- Accepted residual: a table-declared-Extended CAN ID with a genuinely
  normal-addressed frame on it (RxStatus bit 7 clear) still over-strips one
  byte -- unchanged, pre-existing behavior (Decision item 2 above).
- Accepted residual, test-coverage gap (`edge-case-hunter` pre-merge review,
  low-risk, confirmed by code inspection rather than a live behavioral
  branch): no test pins that `header_footer_len`'s plain `j2534_0404::CAN`
  arm structurally ignores `rx_ext_addr` (it does -- that match arm never
  references the parameter), and no test exercises the software-ISO-TP
  "deliver raw, not yet reassembled" arm with `rx_ext_addr = true` (it
  shares the identical `header_footer_len` call site as every other raw
  delivery, so this is coverage, not a suspected divergence). Revisit only
  if a future change to either arm makes the shared-call-site assumption
  stop holding.
- `docs/adr/ADR-051-iso15765-resultdata-header-in-extra-info.md`'s own text
  is checked for any claim that the `UniqueRespIdTable` lookup is the sole
  extended-addressing signal, and corrected if so.
