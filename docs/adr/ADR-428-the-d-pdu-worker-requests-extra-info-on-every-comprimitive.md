# ADR-428: The D-PDU Worker Requests Extra Info on Every ComPrimitive, and the Mock Attaches the Table's Response Identifier

**Date:** 2026-11-07
**Status:** Accepted
**Affects:** `crates/iso22900-service` (`service/convert.rs` `with_extra_info`, `control_for_start`;
`service/rpc_primitive.rs` `rpc_start_com_primitive`; `tests/grpc_mock.rs`),
`crates/iso22900-mock` (`PDUSetUniqueRespIdTable`, `PDUStartComPrimitive`, `begin_next`,
`make_event_item`), `crates/iso22900-mock/docs/testing-guide.md`, `docs/rpc-api-guide.md`,
`docs/system-architecture.md` 8.11, `crates/iso22900-service/docs/implementation-notes.md`;
ADR-051, ADR-116, ADR-299, ADR-427

## Context

ISO 22900-2:2022 10.1.4.11.4 and D.2.1 (the 2009 edition has the same flag) give a result item
header bytes, and footer bytes, only when the TxFlag of the ComPrimitive that produced it sets
ENABLE_EXTRA_INFO (byte 0, bit 5 of Table D.4). The agent sends no TxFlag on its D-PDU link
(design principle 6: the agent does not branch on the worker). On a real library the results
therefore had no identifier, `unattached` items named none, and every identifier the agent used
rested on its fallback to the unique response identifier table.

The mock ignored the TxFlag. It attached a header only to the indication items it queued unbound
under `SimEcuCommand::IndicationBinding`, from that command's identifiers (ADR-427), so no test
showed the agent reading a header from a response item.

## Decision

1. **The worker requests extra info on every ComPrimitive.** `iso22900-service` ORs ENABLE_EXTRA_INFO
   into byte 0 of the TxFlag after it converted the caller's control data, once, in
   `rpc_start_com_primitive`, so every writer (named bits, raw bytes, no `tx_flag`, no
   `cop_ctrl_data`) goes through it. Every other bit the caller set is kept. The flag is padded
   with zeros to the default length of four bytes (D.2); a longer raw flag keeps its length. The
   agent is unchanged.
2. **The mock gives result items a header when asked.** `PDUSetUniqueRespIdTable` records the
   `CP_CanRespUSDTId` value (a `PDU_PT_UNUM32` ComParam; all ones means not used) of the entry
   whose `UniqueRespIdentifier` is 1, the identifier its ECU always answers as. Like the other
   ComParams it goes to a working buffer, which `PDUConnect` and the start of an UPDATEPARAM make
   active (ISO 22900-2:2022 8.4.28.1, 8.4.15.6), and a ComPrimitive keeps the id active when it
   was started (8.4.17.2). A null table is refused (8.4.28.2). The link's reset clears both
   buffers. `PDUStartComPrimitive` notes whether byte 0 of the TxFlag has the bit, reading the
   flag only when it has a byte to read. When the bit was set and the table gave an identifier,
   each result item of the attached ECU (responses and response pendings) carries a four-byte
   big-endian header, no footer, and its data bytes are unchanged. There is no header without the
   bit, without a table identifier, on the synthetic completion item (no ECU answers), on bound
   indications, or on status and error items. `PDUDestroyItem` already frees a result's extra
   info and its header bytes once.
3. **Unbound indications keep ADR-427's control.** The mock then has two sources for a header:
   the control for the unbound indications, the table for the responses.
4. **Related ADRs are annotated.** ADR-299 is extended by item 2; ADR-427's Consequences on the
   agent's D-PDU link are settled by item 1.

## Consequences

- On a library, results carry the identifier they came on when the library honours the bit, so
  `unattached` names it and `response_id` prefers it over the table. A header of a length other
  than four bytes (for example a CAN identifier with an extended address) is not read as an
  identifier and falls back to the table, as before; a hardware run has to show which layout a
  library gives (backlog).
- Every StartComPrimitive now has at least four flag bytes, so a hardware check's raw flags are
  harmless but are widened. J2534 is unaffected: its TxFlags are the native word (ADR-116) and
  its worker splits the CAN identifier itself (ADR-051).
- On the mock the header equals the table fallback, so an agent end-to-end test cannot tell the
  two apart; the discriminating tests are the mock's unit tests and the service's gRPC test.
- Hardware runs must confirm that a library accepts the bit on every ComPrimitive type.
