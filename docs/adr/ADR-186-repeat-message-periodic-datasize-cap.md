# ADR-186: SAE J2534-1 §7.2.7 Periodic-Message DataSize Cap for Repeat Messaging

**Date:** 2026-08-22
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs`, `service/rpc_link.rs`, `service/discovery.rs`, `j2534-0404-mock/src/lib.rs`, `j2534-0404-service/tests/grpc_mock/{j1939,honda_diagh}.rs`, `docs/adr/ADR-165-j2534-2-repeat-messaging-phase12.md` (Status line annotation), `docs/adr/INDEX.md`, `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`, `j2534-0404-mock/docs/testing-guide.md`

## Context

SAE J2534-1 v04.04 §7.2.7 caps every periodic message, of any protocol, at
a single frame — `<DataSize>` at most 12 bytes, including the message
header/ID. SAE J2534-2 clause 14.2.1 (Repeat Messaging) ties the
`RepeatMsgData[0]` message a repeat slot autonomously retransmits to this
same periodic-message cap. Clause 21.2.2(h) restates the 12-byte cap for
CAN FD specifically, and clause 22.2.2(h) restates an 11-byte cap for
FD-ISO15765 — critically, in both cases the cap is *not* relaxed by the
`TX_FD_CAN_FORMAT` TxFlag, unlike clause 21.4.4's cap for an *ordinary*
(non-periodic) TX message, which the same flag widens up to 68 bytes.

Before this fix, `ioctl_start_repeat_message`
(`j2534-0404-service/src/service/rpc_misc.rs`) validated `RepeatMsgData[0]`
(`setup.repeat_msg_data`) against the wrong size ranges: the same
ordinary-TX ranges (`fd_can_tx_message_size_range`,
`fd_iso15765_tx_message_size_range`, `protocol.tx_message_size_range`) used
for an ordinary `CoptSendrecv` TX message. For the raw `FD_CAN_PS` family
specifically, a padding block additionally grew a payload up to 64 bytes
(68-byte DataSize) whenever the client had staged a large
`CP_CANFDTxMaxDataLength`, composing and sending a periodic message far
past the 12-byte cap. This was nonconformant for every protocol family this
function supports, not just FD — classic (non-FD) ISO15765 repeat messages
were equally allowed past their own periodic ceiling. On a real conforming
adapter, an oversized repeat-message start draws `ERR_INVALID_MSG` (clause
14.2.2.1); this service instead composed and sent it, and the mock's own
lack of DataSize validation on this path masked the bug end-to-end.

Separately, `install_client_message_filters` (`rpc_link.rs`) never set
`TX_FD_CAN_FORMAT` on a client-installed filter's mask/pattern template for
an FD-connected link. `ioctl_start_repeat_message`'s own round-15 fix
(ADR-165 PR #42) already sets this flag on its own mask/pattern templates,
for the same clause 21.4.4 reason: a `PASSTHRU_MSG`'s `DataSize` is capped
at 12 bytes unless `TX_FD_CAN_FORMAT` is set, in which case Table 91 allows
up to 68 — a long filter template on an FD-connected link needed the same
treatment for template *validity*, even though clause 21.2.2(g) requires
FD-capable-channel filtering itself to ignore CAN message format entirely
(matching is on address+data only, never on classic-vs-FD wire encoding).

## Decision

1. `ioctl_start_repeat_message`'s size-range check for
   `setup.repeat_msg_data` (`RepeatMsgData[0]`, the message this slot
   actually *transmits*) is replaced with a periodic-message-capped range,
   covering every protocol family this function supports, not just FD:
   - Raw CAN (FD or classic): `4..=12` bytes — the periodic cap for this
     family is not relaxed by `TX_FD_CAN_FORMAT`, so both the FD and
     classic case collapse to the same fixed range (no longer tracking the
     link's staged `CP_CANFDTxMaxDataLength`).
   - ISO15765 (FD or classic): `4..=11` bytes (normal addressing) /
     `5..=11` bytes (extended addressing), per clause 22.2.2(h)'s
     FD-ISO15765 restatement, applied uniformly to classic ISO15765 too
     under §7.2.7's flat, protocol-independent cap. This check now runs
     ahead of, and (per a second design-advisor consult, confirmed
     independently rather than taking the first pass's own claim at face
     value) fully subsumes, the pre-existing functionally-addressed Single
     Frame check (ADR-055/ADR-169) for every ISO15765 flavor, classic and
     FD alike — see the Consequences section for the subsumption argument.
     That inlined check is therefore deleted outright, not merely
     superseded, replaced by a comment recording why.
   - SAE J1939: `5..=12` bytes, via the same generic `.min(12)` treatment
     as every other non-CAN-FD/non-ISO15765 protocol below (not a
     dedicated arm). Per a third-round design-advisor consult, SAE
     J2534's `<DataSize>` field never carries a "wire bytes transmitted"
     carve-out anywhere in the spec family — clause 21.2.2(h)/22.2.2(h)'s
     own CAN-FD periodic-message cap counts the 4-byte CAN ID toward
     `<DataSize>` even though it is never a wire "data" byte either, and
     clause 16's Table 62 gives a 0-data-byte J1939 message a minimum
     `<DataSize>` of 5, despite that message sitting squarely inside
     clause 16.4.4's own don't-care regime for the destination byte. Both
     confirm the spec's own accounting counts the don't-care byte toward
     `<DataSize>` regardless of what happens on the wire, so a "5-byte
     header + 8-byte payload transmits as a 12-byte wire frame, therefore
     13 is the boundary" reading (the prior round's position) does not
     hold: SAE J2534-2 clause 16 never restates a periodic-message cap for
     J1939, so it inherits SAE J2534-1 §7.2.7's flat 12-byte `<DataSize>`
     cap literally, the same as every other protocol without its own
     clause 21.2.2(h)/22.2.2(h)-style restatement.
   - Every other protocol this function supports (KWP/ISO14230, ISO9141,
     J1850PWM/VPW, SCI, J1939 — see the J1939 bullet just above for why it
     falls through to this same generic treatment rather than getting its
     own dedicated arm — and any other protocol reaching this check): the
     existing per-protocol range's own upper bound is simply capped at 12,
     preserving whatever lower bound already applied.

   An oversized `repeat_msg_data` is now rejected with `invalid_argument`
   before composition, citing SAE J2534-1 §7.2.7 and SAE J2534-2 clause
   14.2.1. The now-unreachable `FD_CAN_PS` padding block (which used to
   grow a payload up to 64 bytes) is deleted; the raw CAN family's fixed
   `4..=12` range guarantees every payload reaching that former padding
   step is already a wire-legal CAN FD DLC (0–8 bytes), so a
   `debug_assert!` stands in its place as an invariant guard.

   Repeat-message mask/pattern *templates* (`RepeatMsgData[1]`/`[2]`,
   `setup.mask_data`/`setup.pattern_data`) are deliberately **exempt** from
   this cap and left entirely untouched. These were widened in round 17/18
   (ADR-165 PR #42) specifically to let a slot's stop-matching discriminate
   deep into a long ECU response (`Condition == 0`'s match evaluation);
   applying the periodic cap to them here would silently revert that fix,
   since a template is a comparison basis evaluated against incoming
   traffic, never itself transmitted, so the periodic-message constraint
   (which bounds what a device autonomously *retransmits*) does not apply
   to it.

2. `install_client_message_filters` now ORs `TX_FD_CAN_FORMAT` into the
   `TxFlags` used for both the mask and pattern `PassThruMessage`s it
   installs, unconditionally, whenever the connected link is FD
   (`resources::is_fd_protocol_id`) — mirroring `ioctl_start_repeat_message`'s
   own round-15 precedent. This is deliberately *not* length-gated: a short
   filter template stays valid either way, so there is no benefit to
   narrowing the condition further. `TX_FD_CAN_BRS` is deliberately **not**
   set — it is a transmission bit-timing property within an FD frame
   (Table 93), not a classic/FD format discriminator, with no analogous
   DataSize-validity coupling on a never-transmitted template.
   `can_filter_tx_flags` itself is untouched (its own contract is
   ID-width-variant selection, orthogonal to this flag) — the bit is ORed
   in at the two `PassThruMessage::new` call sites instead.
   `install_pass_all_filter`'s own pass-all templates are deliberately
   **excluded** from this fix — its 4-byte templates are already valid
   under the 12-byte periodic/ordinary-TX cap regardless of
   `TX_FD_CAN_FORMAT`, so there is nothing for this flag to fix there.

## Consequences

A repeat-message start that previously succeeded with an out-of-spec,
oversized FD or classic-ISO15765 payload now fails fast with a
clause-citing `invalid_argument` error. This is a genuine, deliberate
breaking behavior change for any caller currently relying on the old,
nonconformant acceptance — accepted, since a real conforming J2534-2
adapter would already reject such a start with `ERR_INVALID_MSG` (clause
14.2.2.1); this fix only brings the service's own behavior in line with
what the hardware it fronts already requires.

For ISO15765 (classic and FD alike), the new periodic cap fully subsumes
the pre-existing functionally-addressed Single Frame check (ADR-055/
ADR-169) at this call site: the cap admits at most 7 payload bytes (normal
addressing) or 6 (extended addressing), which never exceeds the Single
Frame limit — exactly 7/6 for classic CAN and for FD at the minimum staged
`CP_CANFDTxMaxDataLength` of 8, and strictly less than the FD-widened limit
at any larger staged value — and the periodic cap is checked first. The
inlined Single Frame check in `ioctl_start_repeat_message` was therefore
unreachable for every ISO15765 flavor and has been deleted, replaced by a
comment recording this subsumption; any future relaxation of the periodic
cap must reinstate an equivalent check. The shared
`isotp::max_sf_payload`/`fd_max_sf_payload` helpers remain live for
`rpc_primitive.rs`'s ordinary `CoptSendrecv` TX path, which still enforces
the Single Frame limit there.

Accepted residual: a maximally strict reading of clause 14.2.2.1 could
extend the periodic-message cap to the mask/pattern templates
(`RepeatMsgData[1]`/`[2]`) as well, not just the transmitted message. This
is deliberately **not** done here — it would silently revert round 17/18's
own widening of those templates, which exists to support discriminating a
match deep into a long ECU response (`Condition == 0`'s stop-matching).
