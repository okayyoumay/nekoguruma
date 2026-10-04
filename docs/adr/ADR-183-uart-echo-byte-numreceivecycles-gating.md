# ADR-183: UART Echo Byte's Legacy 5-Baud-Init Path Gains NumReceiveCycles Gating

**Date:** 2026-08-18
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`CoptStartcomm`
handler's legacy 5-baud-init heuristic branch), `j2534-0404-service/src/service.rs`
(`FiveBaudInit` doc comment), `docs/rpc-api-guide.md`,
`docs/j2534-0404-architecture.md`

## Context

ISO 22900-2 §8.2.6.3.2 c) (both the 2009(E) edition,
`iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:690`, and the 2022
edition, `iso22900-2-2022/ISO_22900-2_2022(en).md:702-707`) frames
`NumReceiveCycles` as the client's key-byte-delivery switch for a 5-baud
`PDU_COPT_STARTCOMM` generically — the requirement is not scoped to any
particular K-line protocol family. [ADR-076](ADR-076-five-baud-init-spec-contract.md)
implemented this generic contract only on its "spec-mandated" path (explicit
`CP_InitializationSettings == 1`), and froze the pre-existing "legacy
heuristic" path (the ComParam absent from the bound set) as delivering key
bytes unconditionally regardless of `NumReceiveCycles`. ADR-076's own stated
reason for that freeze, its Consequences section's "Behavioral asymmetry, by
design" bullet, was compatibility: the legacy path coexists with the new
spec-mandated path on ISO9141/ISO14230 links, and extending the gate to it
would retroactively change the contract for pre-existing legacy clients that
had no reason to expect it.

[ADR-170](ADR-170-j2534-2-uart-echo-byte-phase9.md) (SAE J2534-2 clause 12,
UART Echo Byte) added `PROTOCOL_UART_ECHO_BYTE_PS`/`_CHx` afterward. Decision
3 deliberately excludes `CP_InitializationSettings` from this protocol's
ComParam allowlist — not because clause 12.3.4.1's closed native-`SET_CONFIG`
parameter list forces the exclusion (this ComParam has no native form), but
as a policy choice: `FiveBaud` is the only spec-valid init outcome for this
protocol, so the ComParam would carry no real information. Decision 8 follows
from that: because the spec-mandated `== Some(1)` branch can never fire for
this protocol, the legacy single-byte heuristic is its *only* reachable init
path, and Decision 8's own text notes the generic five-baud branch in
`run_protocol_init` needed no change to keep serving it.

SAE J2534-2 clause 12.3.4.2 (`j2534-2-0404/J2534-2_202012 - Optional
Pass-Thru Features.md:1363-1367`) leaves the native `FIVE_BAUD_INIT` IOCTL's
interface unchanged for this protocol and says nothing about D-PDU-layer
`NumReceiveCycles` semantics, so nothing in clause 12 overrides or narrows
ISO 22900-2's generic contract for this protocol.

Put together: ADR-076's asymmetry reasoning doesn't reach UART Echo Byte.
That reasoning assumed the legacy path exists alongside a spec-mandated
alternate path, and preserved the legacy path's old behavior to avoid
breaking clients who had a choice of which path to use and had already
settled on the legacy one before ADR-076 shipped. UART Echo Byte has no
alternate path (Decision 8) and no pre-ADR-170 legacy clients to grandfather,
since the protocol did not exist before ADR-170 introduced it — after
ADR-076. The premise that motivated leaving this protocol's only init path
ungated was never actually evaluated against it; it inherited the freeze only
because it happens to route through the same branch of code ISO9141/ISO14230
also use.

## Decision

For `PROTOCOL_UART_ECHO_BYTE_PS` specifically, the legacy-heuristic branch of
`CoptStartcomm`'s handler now applies the same `NumReceiveCycles` contract the
spec-mandated branch already applies:

- `num_receive_cycles` (`cop_ctrl_data.as_ref().map(|c| c.num_receive_cycles).unwrap_or(0)`,
  mirroring the spec-mandated branch's own default) must be `0` or `1`; any
  other value is a synchronous `Status::invalid_argument`, mirroring the
  spec-mandated branch's own validation.
- `deliver_keybytes = (num_receive_cycles == 1)`; `0` (or an absent
  `cop_ctrl_data`) still runs the init but suppresses delivery of the
  `[KB1, KB2]` result.

This is a deliberate **hybrid**, not a full switch onto the spec-mandated
branch's contract: the target address still comes from `cop_data[0]`, and
`cop_data` remains mandatory-exactly-one-byte for this protocol
(ADR-170 Decision 8, unchanged by this ADR). The spec-mandated branch's
"no optional message / address from ComParams" shape is not adopted here,
because ADR-170 Decision 3's ComParam allowlist leaves
`CP_5BaudAddressPhys`/`CP_5BaudAddressFunc` unsettable for this protocol —
`cop_data[0]` is the only address carrier this protocol has, and nothing in
this ADR revisits that allowlist decision.

ISO9141/ISO14230's own use of the legacy branch, and fast-init
([ADR-075](ADR-075-fast-init-header-construction-and-response-parsing.md)), are unchanged
byte-for-byte: ADR-076's original decision for those two protocols remains
in force exactly as written. This ADR narrows ADR-076's scope to exclude
`PROTOCOL_UART_ECHO_BYTE_PS`; it does not reopen or revisit the decision for
the protocols ADR-076 actually reasoned about.

## Consequences

- **Intentional breaking change** for UART Echo Byte clients that relied on
  the prior default (key bytes always delivered regardless of
  `NumReceiveCycles`): they must now set `NumReceiveCycles = 1` explicitly to
  keep receiving them. This is the same migration stance ADR-076 itself
  already established for its own breaking change on the spec-mandated path.
- **Narrows ADR-076's Consequences**: the "Behavioral asymmetry, by design"
  bullet (legacy path and fast-init deliver responses unconditionally, only
  the spec-mandated path gates) no longer covers `PROTOCOL_UART_ECHO_BYTE_PS`.
  ADR-076's Status line is annotated in place to record this; the rest of
  ADR-076, and its treatment of ISO9141/ISO14230/fast-init, is unaffected.
- **Amends ADR-170 Decision 8**: the legacy single-byte heuristic, being this
  protocol's sole init path, is no longer a byte-for-byte pass-through of the
  pre-ADR-076 legacy contract — it now also carries the spec-mandated
  branch's `NumReceiveCycles` validation and gating.
- **General precedent**: a protocol whose only reachable init path happens to
  be the branch of code labeled "legacy heuristic" still gets ISO 22900-2's
  generic ctrl-data contract, wherever that branch's own per-protocol
  constraints (here, the ComParam allowlist forcing the address to come from
  `cop_data`) don't prevent it. This asymmetry between UART Echo Byte and
  ISO9141/ISO14230 on the same code path is driven by which paths are
  actually reachable for each protocol, not a blanket exemption for anything
  routed through the legacy branch.
