# ADR-414: RawMode Follows the Base Protocol on `_PS` and `_CHx` Links

**Date:** 2026-11-03
**Status:** Accepted (revises ADR-196, ADR-198 and ADR-200 on one point: their protocol allowlist
             is applied to the protocol a `_PS` or `_CHx` link stands for)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_create_com_logical_link`'s
             RawMode allowlist and its refusal text),
             `j2534-0404-service/tests/grpc_mock/raw_mode.rs`,
             `j2534-0404-service/tests/grpc_mock/j1939_raw_mode.rs`,
             `j2534-0404-service/tests/grpc_mock/t0_loopback_check.rs`,
             `j2534-0404-service/docs/comparam-protocol-support.md`,
             `ngr-hw-checks/src/guard/handle.rs` (`GuardedHandle::open_link`),
             `ngr-hw-checks/src/guard/link.rs`, `ngr-hw-checks/src/profile.rs`,
             `ngr-hw-checks/docs/hardware-profile.md`, `docs/j2534-0404-architecture.md`,
             `docs/j2534-2-support-plan.md`,
             `docs/adr/ADR-196-cll-create-flag-raw-mode-checksum-mode-phase1.md`,
             `docs/adr/ADR-198-cll-create-flag-raw-mode-checksum-mode-phase2.md`,
             `docs/adr/ADR-200-cll-create-flag-raw-mode-phase3.md` (Status lines)

## Context

ADR-196 Decision item 1 limited RawMode to a list of protocols, and ADR-198 and ADR-200 extended
the list. The check compared the link's hardware protocol id with each family's base id exactly,
so a link made with SAE J2534-2 pin selection (clause 6, `_PS`) or as an additional channel
(clause 7, `_CHx`) was refused RawMode although the same protocol on its base channel accepts it.
Those ADRs scoped such links out for their phase; none weighed them as a case of their own.

A pin or channel selection changes where the physical layer is connected, not the protocol. The
send path (`tx_header`) already keys on the link's protocol family, and the receive path
(`events`, `CllRxEntry::header_protocol`) and the TX flag derivation (`compute_j2534_tx_flags`)
on `resources::base_protocol_id`, so the allowlist was the only thing in the way.

The T0 loopback of the hardware harness (design 13.5) runs between two channels of one module,
and two links of one protocol on one module need two channels.

## Decision

1. A `_PS` or `_CHx` link gets the RawMode answer of the protocol it stands for. The service
   resolves the link with `resources::base_protocol_id`, which recurses through
   `chx_base_protocol_id`, and applies the existing list and its conditions to the result. There
   is no second list.
2. The conditions carry over unchanged: an ISO15765 link must not be software ISO-TP (a `_PS` or
   `_CHx` link never is, ADR-157), a J1850 link needs ChecksumMode ON (ADR-200), and a K-line
   link keeps ChecksumMode's meaning (ADR-198).
3. Allowed: CAN, ISO15765, ISO9141, ISO14230, J1850VPW, J1850PWM, SAE J2610 SCI and SAE J1939 on
   their base, `_PS` and `_CHx` ids, and the Single Wire, Fault-Tolerant and CAN FD families,
   which resolve to CAN or ISO15765. Refused in every form: TP2.0 (the permanent exclusion of
   ADR-200), UART Echo Byte, Honda DIAG-H, GM UART and SAE J1708.
4. CAN FD. An FD link cannot be named by a direct id; ordinary CAN or ISO15765 links become FD at
   connect when FD ComParams are staged (ADR-158, ADR-213), so a RawMode `CAN_CHn` or `CAN_PS`
   link promoted to FD keeps its RawMode flag. This is intended under the rule above: the
   promoted link is still CAN. In `rpc_primitive.rs` the FD format and rate-switch flags are
   set from the link's state, never from the client's message, and `tx_header` passes a RawMode
   message through unchanged (the identifier and the data stay the client's, with no padding to
   a DLC length), so a raw FD frame reaches the wire as the client composed it.
5. The refusal text describes the rule.
6. The hardware harness creates a link of a profile endpoint that names a `channel` from resource
   data carrying that name (SAE J2534-2 clause 7 describes the compound name), not through
   `GetResourceIds`, which lists base protocol names only. The harness branches on the profile,
   not on the worker (design principle 6). The guard compares the data with the profile's link
   as it does a learned resource (ADR-333 item 2). The profile and the guard require a
   `channel` to be of the link's own framing (raw CAN or ISO 15765), the only place a channel
   is named (no `_CHn` suffix on the link's protocol), free of surrounding whitespace (the worker
   does not trim), and different from the other end by physical channel: `CAN_CHn` and
   `ISO15765_CHn` with one `n` are taken, conservatively, for one hardware channel (a raw CAN and
   an ISO 15765 link can share one) and are refused as two ends.
7. The profile refuses a `channel` on a D-PDU API VCI: a D-PDU library has no compound channel
   name to create a link by.

## Consequences

- The mock tests send and receive a raw frame on a `_PS` and a `_CHx` link of each allowed family
  with a mock exchange (CAN, ISO15765, ISO9141, ISO14230, J1850VPW, J1850PWM, SAE J2610 SCI,
  Single Wire and Fault-Tolerant CAN and ISO15765, J1939 through its destination-address shim),
  and the FD promotion of a RawMode `CAN_CH1` and `CAN_PS` link. Each excluded family fails on
  its `_PS` and its `_CHx` id. The T0 loopback check passes between two channels of one module.
- A new family with a `_PS` or `_CHx` block takes the rule of the base the mapping names, so
  adding one needs a decision only whether its base belongs on the list.
