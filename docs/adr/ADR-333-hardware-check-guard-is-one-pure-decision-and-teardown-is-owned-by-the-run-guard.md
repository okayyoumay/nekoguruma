# ADR-333: The Hardware-Check Guard Is One Pure Decision over the Profile, and Teardown Is Owned by the Run Guard

**Date:** 2026-10-19
**Status:** Accepted
**Affects:** `crates/ngr-hw-checks` (`guard`, `teardown`, `report::RunGuard`, `docs/hardware-profile.md`), design 13.5 ("Guarded VCI access", "Cleanup"); ADR-275 items 4 and 6; `crates/agent` `policy` (related, unchanged)

## Context

ADR-275 item 4 says a check reaches the VCI only through a guarded handle, and item 6 says
teardown steps run in reverse, once, on pass, failure and panic. Both stayed prose. The handle has
two parts: a decision (is this worker request allowed?) and a seam that routes a direct check or
the agent's job runner through it and keeps the state the decision needs. The decision is what
the class rules rest on, and it needs to be testable without a worker or a device.

Three facts shape it. The worker creates links by resource id and sets ComParams by id, and
the agent resolves those ids with `GetResourceIds` and `GetObjectId` first, so a request often
carries a number whose meaning only an earlier answer tells. The handle of a new link is known
only from the response. And the agent's own read-only list (`policy::READ_ONLY_SERVICES`, ReadDTCInformation
and ReadDataByIdentifier) is for production jobs, which own no session and send no tester present,
whereas design 13.5's class table lets a T3 check use the default and extended session and tester
present.

## Decision

1. **One pure function over explicit state.** `guard::decide(&Context, &Request)` returns a
   `Grant` or a `GuardRefusal`. `Request` has one variant per RPC of `service VciService` and
   `decide` matches it without a catch-all arm, so a new RPC does not compile until it is
   classified (a test also compares the variants with `service.proto`). `Context` holds the
   profile, the module handle, the links bound so far (`cll_handle` to profile endpoint and
   RawMode) and what the harness learned from worker answers (resource id to resource, object
   type and id to short name). `decide` sends nothing and keeps nothing: state changes only
   through a `Grant` (`CreatesLink` names the endpoint to bind the response's handle to,
   `DestroysLink`, `EntersSession` for the return-to-default step) or through a response the
   caller records. Anything unknown is refused: an id not learned, a handle not bound, a handle
   of another module, a request that cannot be decoded. `ModuleConnect` is refused, since the
   harness opens the device before the guard exists and another module is another device.
2. **Link identity is pinned to the profile.** A link is created only from a resource that names
   the profile link's protocol (ASCII case ignored, no alias table), the dual-wire CAN bus type
   `ISO_11898_2_DWCAN` the agent uses, and exactly two pins: the profile's first pin as `HI` and
   its second as `LOW` (ISO 22900-2 (2022) Annex B). A resource by name is refused. The create
   flag may carry RawMode (D.2.3) and nothing else. There is one link per profile endpoint: one,
   or two for T0. ComParams are compared by name (or by the name a learned id stands for): the
   baud rate must equal the profile's, every other physical-layer ComParam is refused (the list
   of `profile`, with the ComParams of class `PDU_PC_BUSTYPE`), the identifiers must equal the
   profile's (the unsegmented response identifier is refused, the functional one needs a
   functional identifier in the profile), the CAN ID format ComParams must encode the profile's
   id width and addressing (bits of Table B.13 and B.14, verified against ISO 22900-2 (2022);
   the same bits `j2534-0404-service` decodes), the address and filler ComParams must agree with
   the profile's address and padding, the request and tester-present address modes may be
   functional only with a functional identifier, a tester-present message is judged as a UDS
   request by the class list, and a ComParam the profile's `[link.comparams]` fixes keeps the
   profile's value. Timing and the other ComParams pass. `COPT_RESTORE_PARAM` is refused on
   every class, since it would put back ComParams the guard compared with the profile.
3. **UDS requests and frames.** T1 passes every request undecoded (the ECU is simulated; a
   session change is still reported so it can be undone). T0 sends no ECU-addressed request: only
   raw frames to the sending endpoint's own request identifier pass, and their content is not
   read; a non-raw payload is refused. T2 and T3 pass a request only from this list, and until an
   authorized job exists (design 13.5 "Writes on a real ECU") T2 is held to the same list:
   DiagnosticSessionControl to the default or extended session (two bytes), ReadDataByIdentifier
   (one or more identifiers), ReadDTCInformation with a report type ISO 14229-1 (2026) 11.3
   defines (at least two bytes) and TesterPresent with sub-function zero (two bytes). The
   suppress-response bit is ignored when the sub-function is read. Everything else is refused:
   an empty request, an undefined service, a response service, a wrong length. DTC clearing,
   security access, writes, routines, resets and the services that read memory by address
   (0x23) or periodically (0x2A) or change a dynamic identifier (0x2C) are not on it. The same
   rule applies to a request sent directly, to one repeated by a periodic primitive (a send count
   above one, or -1) and to one inside a raw frame. A raw frame is `[4-byte CAN id][address byte
   for extended or mixed addressing][PCI and data]`: it must go to the profile's request or
   functional identifier, carry the profile's address byte, and its `TX_FLAG_CAN_29BIT_ID` and
   `ISO15765_ADDR_TYPE` flags must agree with the profile; a raw TxFlag must be four bytes
   with only named bits. The PCI byte (ISO 15765-2 (2024) 9.6) says what follows: a single
   frame, including the escape length form, is decoded; a first frame is decoded from its leading
   bytes and its declared length; a flow control frame passes; a consecutive frame, any other
   PCI type and a frame too short for its PCI are refused. The agent's list is kept apart on
   purpose: it is narrower (no session, no tester present) because a production job owns no
   session, while a T3 check may; the guard's list also looks at sub-function and length. The
   crate documents the relation next to its list; it does not reuse `policy`, so a change to the
   job runner's rule does not silently change what hardware checks may send.
4. **IO controls.** An IOCTL is named by its `PDU_IOCTL_*` short name (ASCII case ignored) or by a
   learned IO control id; anything else is refused. On every class these pass: `READ_VBATT` (the
   battery voltage, the one VCI read design 13.5 always allows) and the housekeeping that touches
   only the VCI's own queues, buffers and receive filters (clear, suspend and resume of the
   transmit queue, clear of the receive queue, buffer size, start, stop and clear of message
   filters, event queue properties). These are refused on every class, also when a profile lists
   a hardware control: programming voltage, `GENERIC` and every vendor or unknown name or id,
   `SEND_BREAK`, the software-CAN switches, the repeat-message controls, device configuration
   and poll response, `BECOME_MASTER`, `RESET`, and every other read (programming voltage,
   ignition sense, pin voltage, cable id, device configuration, NDIS adapter information).
   Letting a profile's control through on T1 and T2 is a later change with its own review.
5. **Teardown is owned by `RunGuard`.** `teardown::Teardown` takes steps as resources are
   acquired (`register` for a closure, `register_async` for a future). `run` pops them in
   reverse, so each runs at most once, a second `run` or `Drop` finds none, each step runs under
   `catch_unwind`, a failing or panicking step does not stop the rest, and the failures are
   returned. An async step runs on a scoped OS thread with a fresh current-thread runtime and a
   timeout, so `run` works from inside a runtime's thread, where blocking on the runtime would
   panic. `Drop` runs the rest and never panics. `RunGuard::teardown()` gives the run's
   `Teardown`; the guard's `Drop` runs it before anything else, whatever the check did, and when
   a step failed it turns a recorded pass into a failure naming the steps (and appends them to
   a recorded failure), then writes the report as before.

Alternatives rejected:

- **Reusing `agent::policy`.** Its list is deliberately narrower (item 3), it has no notion of
  frames, sub-functions, lengths or classes, and sharing it would tie the hardware checks' limit
  to changes in the job runner's.
- **A guard that calls the worker to resolve ids.** It would make the decision depend on, and
  send traffic to, the device it protects, and it could not be tested without a worker. The
  harness records `GetResourceIds` and `GetObjectId` answers instead, and an id it has not seen
  is refused.
- **Teardown as a separate local that relies on drop order.** The order of locals is easy to get
  wrong, a step cannot be registered from code that does not own the local, and a failed step
  could not reach the report. One owner, the run's guard, fixes the order and the outcome.
- **Decoding consecutive frames with state across frames.** It would make the decision stateful
  and bring reassembly timing into the guard. A consecutive frame is refused, so a multi-frame
  request through a raw link is limited to what its first frame shows and cannot be completed
  through the guard.

## Consequences

- A check that needs a service or IO control outside the lists must extend them by a reviewed
  change to the guard (and, for a state-changing service on T2, wait for the authorized-job path
  that design 13.5 describes). Nothing in a profile widens them.
- The decision is not the whole handle. The seam that routes direct checks and the agent's job
  runner through `decide`, keeps the link table, records learned ids and registers teardown
  steps for grants is separate work. Until it exists, nothing enforces the decision on a live
  worker.
- The raw-frame form is the one design 13.5 and the proto describe: the PCI byte follows the
  identifier and address byte. A worker whose raw mode takes the unsegmented message instead
  (a native J2534 ISO 15765 channel) reads the same bytes differently, which the seam's check
  against a real worker must settle before checks send raw frames on T2 or T3.
- A profile's protocol name must be the ISO 22900-2 (2022) short name the agent sends
  (`ISO_14229_3_on_ISO_15765_2`); another spelling of the same protocol is refused.
- ADR-275 items 4 and 6 are unchanged; this ADR gives the rules their concrete form.
