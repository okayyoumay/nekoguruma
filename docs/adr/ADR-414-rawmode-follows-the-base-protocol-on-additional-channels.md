# ADR-414: RawMode Follows the Base Protocol on Additional Channels

**Date:** 2026-11-03
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_create_com_logical_link`'s
             RawMode allowlist and its refusal text),
             `j2534-0404-service/tests/grpc_mock/raw_mode.rs`,
             `j2534-0404-service/tests/grpc_mock/t0_loopback_check.rs`,
             `ngr-hw-checks/src/guard/handle.rs` (`GuardedHandle::open_link`),
             `ngr-hw-checks/src/profile.rs` (`check_endpoint`),
             `ngr-hw-checks/docs/hardware-profile.md`, `docs/j2534-0404-architecture.md`,
             `docs/j2534-2-support-plan.md`, `docs/adr/ADR-196-cll-create-flag-raw-mode-checksum-mode-phase1.md`
             (Status line), `docs/adr/ADR-200-cll-create-flag-raw-mode-phase3.md` (Status line)

## Context

ADR-196 Decision item 1 limited RawMode to a list of protocols, and ADR-198 and ADR-200 extended
the list. The list is tested against the link's hardware protocol id. An SAE J2534-2 clause 7
additional channel (`_CHx`) has an id of its own, so the check refused RawMode on every
`_CHx` link, although the same protocol on its base channel accepts it. ADR-196 and ADR-200 left
additional channels out of those phases on purpose; they did not weigh them as a case of their own.

The T0 loopback check of the hardware harness (design 13.5) runs between two channels of one
module, and two links of one protocol on one module need two channels. Without RawMode on an
additional channel, that preferred loopback could not run on `j2534-0404-service`.

## Decision

1. An additional channel is allowed RawMode exactly when its base protocol is, under the same
   conditions. The service derives the base with `resources::chx_base_protocol_id`, the mapping
   it already uses for every other protocol-family decision (ADR-157, ADR-156), and applies the
   existing list to it. There is no second list.
2. The conditions carry over unchanged: an ISO15765 link must not be software ISO-TP (an
   additional channel never is, ADR-157, so the condition holds by construction), a J1850 link
   needs ChecksumMode ON (ADR-200), and TP2.0 stays excluded on every channel.
3. A family whose `_CHx` block extends its own `_PS` id, not a base id the list names, stays
   refused, as the same family's `_PS` link is: GM UART, UART Echo Byte, Honda DIAG-H, J1708,
   TP2.0, Fault-Tolerant CAN and ISO15765, Single Wire CAN and ISO15765, and CAN FD and
   ISO15765-on-CAN-FD (the FD ids cannot be named directly either, ADR-158). Widening any of them
   is a decision of its own for that family.
4. Allowed channel families: `CAN_CHx`, `ISO15765_CHx`, `ISO9141_CHx`, `ISO14230_CHx`,
   `J1850VPW_CHx`, `J1850PWM_CHx`, `J1939_CHx`, `J2610_CHx`. The refusal text describes the rule.
5. The harness creates a link of a profile endpoint that names a `channel` from resource data
   carrying that name, as SAE J2534-2 clause 7 describes a channel's compound name, instead of
   through `GetResourceIds`, which lists base protocol names only. The harness branches on the
   profile, not on the worker (design principle 6). The guard compares the data with the
   profile's link as it does a learned resource (ADR-333 item 2), and is no looser.
6. The profile refuses a `channel` on a D-PDU API VCI: a D-PDU library has no compound channel
   name to create a link by.

## Consequences

- RawMode on `CAN_CHx` and `ISO15765_CHx` is covered by tests that send and receive raw frames
  against the mock, and the T0 loopback check passes between two channels of one module there.
- A new family with a `_CHx` block takes the rule of the base the mapping names, so adding one
  needs no change here, only a decision whether its base belongs on the list.
- The wire shape of RawMode on `J1939_CHx`, `J2610_CHx` and the K-line and J1850 channels is
  the shape of their base protocol; only their creation is tested on a channel here.
