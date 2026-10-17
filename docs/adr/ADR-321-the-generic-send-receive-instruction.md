# ADR-321: The Generic Send-Receive Instruction

**Date:** 2026-10-17
**Status:** Accepted (Decision item 5, and the Consequences on serving the instruction and on what the static checks see, superseded by ADR-322)
**Affects:** `diag-ir` (`Op::SendReceive`, `DiagHost::send_receive`, `VmError::UnknownService`,
`check_declarations`, `schema/ir.fbs`: `Protocol.application_layer`, `Protocol.negative_shape`,
`NegativeShape`), `agent` (`policy::check_program`, `host.rs`, `runner::check_job`), design
8.2.4; ADR-233, ADR-309, ADR-316

## Context

ADR-309 item 4 replaces `ReadDtc`, whose request the agent's host builds as UDS, with one
generic send-receive. That instruction carries the complete request bytes, which the lowering
builds through the application-layer profile. It also names the declared service whose response
it expects. The host matches the answer against that service's positive response and against
the negative-response shape that the IR declaration carries for the link's protocol, so the IR
needs a place for that shape and for the application layer. ADR-233 lets the instruction set
grow only by appending `Op` variants, and every change to the encoding raises
`IR_SCHEMA_VERSION`.

Three choices are open:
- where the request bytes come from;
- how the service is named;
- where the application layer and its negative-response shape live in the declaration part.

There is also a question of what the agent does with the instruction until its host can match
answers against the declarations.

## Decision

1. **`Op::SendReceive { service }` sends the bytes on top of the stack.**
   - The operand is the complete request, starting with its service identifier. The host
     sends it as it is and builds no request bytes.
   - The answer replaces the operand, as it does for `ServiceRequest` and `RoutineControl`.
   - The request comes from the stack, not from the constant pool, so a `diag.routine` on an
     application layer other than UDS can carry `params` known only at run time (ADR-309
     item 4) with the instruction the constant requests use.
   - `service` is the declared service's `Service.id`, the id `DecodeField` uses (ADR-316),
     so the answer can be decoded through the same declarations.
   - The instruction is a diagnostic primitive, appended after `DecodeField`. A host error
     leaves the state on it, as for every primitive (ADR-233).
2. **`DiagHost::send_receive(service, request)`.** The host matches the answer against the
   positive response of `service` and against the negative-response shape of the link's
   protocol, and returns the answer's bytes. The VM does not read the declarations for this
   instruction; the host does.
3. **The declarations are checked before the program runs.**
   - `diag_ir::check_declarations` (the former `check_decode_fields`) also refuses a
     `SendReceive` whose service the variant does not declare with a positive response
     (`VmError::UnknownService`), since the host could not match an answer to it.
   - The agent's `check_job` runs it, so a job naming such a service is refused before its
     link opens (ADR-318).
4. **The protocol carries its application layer and negative-response shape.**
   - `Protocol` gains `application_layer`, the layer's name ("UDS"), and `negative_shape`.
   - `NegativeShape` holds the marker byte, whether the request's service identifier
     follows it, the position of the code, and the codes that mean the answer is still
     pending. These are the fields of the frontend profile's shape (ADR-314), which the
     workers limit to the `7F` form and a subset of the codes 0x78, 0x21 and 0x23 (ADR-309
     item 5).
   - The shape is placed on the protocol, not on the variant, because ADR-309 item 4 matches
     answers against the shape of the link's protocol. ADR-309 item 3 still requires every
     protocol of a variant to name the same layer.
   - A document whose definition names no layer leaves both fields absent.
5. **The agent refuses the instruction until its host serves it.**
   - `policy::check_program` refuses `SendReceive` whatever the permission
     (`HostError::Unsupported`), and the host's method returns the same error.
   - The release agent's read-only rule (ADR-235 item 8) is kept per application layer only
     once the host matches answers against the declarations (ADR-309 item 4). Until then a
     job that would send it fails before anything is sent.
6. **`ReadDtc` stays in the encoding.** Removing a variant would change the encoding of every
   later one (ADR-233). The frontend never emits it, and the agent keeps serving it for
   programs that already use it.

## Consequences

- Schema version 12. A runtime of version 11 cannot decode a program that uses
  `SendReceive`, and documents of version 11 carry no application layer.
- The frontend writes both fields: each protocol's application layer (`csv-tables.md` 4.3),
  and its profile's negative-response shape and pending codes.
- The agent can serve the instruction only after its host matches answers against the
  declared positive response and the protocol's negative-response shape, stops building
  request bytes for DTC reads, and keys its read-only list by application layer (ADR-309
  item 4). Until then a procedure that lowers to it cannot run on the agent.
- A send-receive's service identifier is the first byte of its run-time operand, so neither
  `Program::validate`'s flash-plan rules (ADR-245, ADR-250) nor the agent's static checks can
  see it. The host that serves the instruction applies those rules to the request it sends,
  and refuses an empty request.
- The VM tests run the instruction against a scripted host; the declaration tests round-trip
  the new protocol fields.
