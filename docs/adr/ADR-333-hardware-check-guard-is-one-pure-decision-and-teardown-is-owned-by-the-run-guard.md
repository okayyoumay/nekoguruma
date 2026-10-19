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
   flag may carry RawMode (D.2.3) and nothing else, and RawMode only on T0 and T1: on T2 and T3
   a RawMode link is refused (item 3). There is one link per profile endpoint: one,
   or two for T0, and a link is created for the endpoint whose own identity the resource names:
   the protocol name of its `channel` (SAE J2534-2 clause 7 additional channel, a `_CH<n>` name
   read as the worker reads it) when the endpoint has one, else its protocol. A SocketCAN end has
   no worker link. The grant binds that endpoint, not the first free one. A link is not
   connected with the worker's defaults: the guard keeps, per bound link, which pinned ComParams
   have been set to the profile's value (`Grant::SetsComParams`, applied by the caller after the
   worker accepted the request) and refuses `ConnectComLogicalLink` until all are: the baud rate,
   the physical request and response identifiers, the functional identifier if the profile has
   one, the CAN ID formats when the width or addressing is not 11-bit normal, the address byte
   when the addressing has one, the filler when the profile pads, and every `[link.comparams]`
   entry. Both workers replace the response table with the one a request carries, so a
   `SetUniqueRespIdTable` must itself carry every pinned ComParam a table can hold, each at the
   profile's value, in exactly one entry (the link addresses one ECU, and the J2534 worker takes
   the physical addressing from the first entry and pairs a response with its request only within
   an entry): the physical request id, format and address, and
   the USDT response id, format and address, as far as the profile needs them (the UNIQUE_ID
   class of ISO 22900-2 (2022) Annex B; the functional request ComParams are of the COM class and
   the J2534 worker reads them from the ComParam set, so they are set with `SetComParam`, not
   required in the table). An empty or partial table is refused, also after a complete one.
   The guard does not track a replacement; it refuses a table that would leave the link short.
   `Context` names the profile VCI its worker fronts, and a link is created only for an endpoint
   on that VCI, so on a T0 loopback of two VCIs each worker's context binds its own end whatever
   the order of creation (two channels of one VCI are both that VCI's). ComParam rules are an allow-list over canonical names. A name (or the name a learned id
   stands for) is first reduced to its canonical form: ASCII case folded and the aliases
   `j2534-0404-service` accepts for one ComParam resolved (`DATA_RATE` is `CP_Baudrate`, and so on),
   so a rule cannot be skipped by another spelling; `iso22900-service` resolves names in the
   D-PDU library, so a name the guard does not know is refused. The request's class field never
   admits a ComParam; it can only add a refusal. Then the rules apply: the
   baud rate must equal the profile's, every other physical-layer ComParam is refused (the list
   of `profile`, with the ComParams of class `PDU_PC_BUSTYPE`), the identifiers must equal the
   profile's (the unsegmented response identifier is refused, the functional one needs a
   functional identifier in the profile), the CAN ID format ComParams must encode the profile's
   id width and addressing (bits of Table B.13 and B.14, verified against ISO 22900-2 (2022);
   the same bits `j2534-0404-service` decodes), the address and filler ComParams must agree with
   the profile's address and padding, the request and tester-present address modes may be
   functional only with a functional identifier, a tester-present message (which the worker sends cyclically) is
   a TesterPresent request (`3E 00` or `3E 80`) on T2 and T3, nothing else, session control
   included; T1 passes it and T0 refuses it, and a ComParam the profile's `[link.comparams]` fixes keeps the
   profile's value. A ComParam no rule covers passes only if it is on the short list of protocol
   and timing ComParams (session timing, ISO 15765-2 frame timing and flow control, response
   pending handling, tester-present timing; ISO 22900-2 (2022) Annex B) or the profile sets it.
   Unknown, vendor and other physical-layer names are refused. `COPT_RESTORE_PARAM` is refused on
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
   rule applies to a request sent directly and to one repeated by a periodic primitive (a send
   count above one, or -1). A RawMode link is refused on T2 and T3, at creation: a worker passes
   RawMode data through, and a native ISO 15765 channel segments it itself, so bytes that look
   like a flow control frame can be a routine request on the wire (`31 01 FF 00` after the
   identifier). A raw-bound handle that appears in the context on T2 or T3 anyway is refused,
   never decoded, so no frame decoder exists. On T0 a raw frame passes only to the sending
   endpoint's request identifier, with the profile's flags (`TX_FLAG_CAN_29BIT_ID`,
   `ISO15765_ADDR_TYPE`) and a raw TxFlag of four bytes with named bits only; its content is not
   read. T1 passes raw frames undecoded. The agent's list is kept apart on
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
   returned. An async step runs on a detached OS thread with a fresh current-thread runtime, and
   the caller waits for it on a channel with a timeout, so `run` works from inside a runtime's
   thread (blocking on the runtime there would panic) and a step that blocks inside `poll`, where
   no runtime timer fires, times out as well: it is reported, its thread is left behind and never
   joined, and the remaining steps run. `Drop` runs the rest and never panics. `RunGuard::teardown()` gives the run's
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

## Consequences

- A check that needs a service or IO control outside the lists must extend them by a reviewed
  change to the guard (and, for a state-changing service on T2, wait for the authorized-job path
  that design 13.5 describes). Nothing in a profile widens them.
- The decision is not the whole handle. The seam that routes direct checks and the agent's job
  runner through `decide`, keeps the link table, records learned ids and registers teardown
  steps for grants is separate work. Until it exists, nothing enforces the decision on a live
  worker.
- RawMode is refused on T2 and T3 until the seam settles, against the workers, what a raw
  payload sends: `j2534-0404-service` passes it through unchanged, so the form (with or without
  transport PCI) depends on the worker. Checks on those classes use ordinary links and
  requests. Only then can raw frames be classified there.
- `SetUniqueRespIdTable` entries are judged by their ComParams only. The
  `unique_resp_identifier` of an entry is the D-PDU unique response identifier, a tag the client
  assigns and that expected-response data refers to; it carries no CAN identifier, so it is not
  compared with the profile.
- `ModuleDisconnect` without a handle is refused, like `ModuleConnect`: it would disconnect the
  module the harness holds.
- A profile's protocol name must be the ISO 22900-2 (2022) short name the agent sends
  (`ISO_14229_3_on_ISO_15765_2`); another spelling of the same protocol is refused.
- The agent's link setup (`open` in `crates/agent/src/link.rs`) sets the baud rate, P2, P2*, the
  0x78 handling and the declared ComParams, and the two identifiers in the response table, and
  then connects. It never sets the functional identifier, the CAN ID formats, the address byte,
  the filler or a profile's `[link.comparams]`. Through the guard, a profile that has any of
  these has its connect refused until the seam sets them (it must set them itself before or
  while handing the agent its link, or the agent's connect fails); the worker's defaults are not
  assumed to equal the profile's (the J2534 CAN preset is 500 kbit/s whatever the profile says).
- ADR-275 items 4 and 6 are unchanged; this ADR gives the rules their concrete form.
