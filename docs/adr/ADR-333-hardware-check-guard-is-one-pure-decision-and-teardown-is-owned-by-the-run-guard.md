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
   flag may carry RawMode (D.2.3) and nothing else, and RawMode only on T0: on T1, T2 and
   T3 a RawMode link is refused (item 3). On T0 a link is created in RawMode and in no other
   way: T0 exchanges raw frames only, and a protocol link would let the J2534 preset arm a cyclic
   tester-present frame. There is one link per profile endpoint: one,
   or two for T0, and a link is created for the endpoint whose own identity the resource names:
   the protocol name of its `channel` (SAE J2534-2 clause 7 additional channel, a `_CH<n>` name
   read as the worker reads it) when the endpoint has one, else its protocol. A SocketCAN end has
   no worker link. The grant binds that endpoint, not the first free one. A link is not
   connected with the worker's defaults: the guard keeps, per bound link, which pinned ComParams
   have been set to the profile's value (`Grant::SetsComParams`, applied by the caller after the
   worker accepted the request) and refuses `ConnectComLogicalLink` until all are: the baud rate,
   the physical request and response identifiers, the functional identifier if the profile has
   one, the CAN ID formats of the physical request, the USDT response and, with a functional
   identifier, the functional request, on every link (the D-PDU API leaves a link's defaults to
   the vendor library, so even an 11-bit normal profile sets them to its encoding), the address byte
   when the addressing has one, the request and tester-present address modes on every link (the J2534 preset of
   `ISO_14229_3_on_ISO_15765_2` sends tester present functionally to its own 0x7DF with handling
   on, so a profile without a functional identifier sets both to physical), the filler handling
   on every link (the preset pads, so an unpadded profile sets it to 0; a padded one sets it to 1
   and sets the filler byte), the tester-present message on T1 to T3 (the preset arms a cyclic
   one and a D-PDU library's default is unknown, so a link starts communication only after the
   message is set and has passed the TesterPresent-only rule; on T0 the ComParam is refused), and
   every `[link.comparams]` entry. The other preset defaults of a ComParam with a rule (formats 0x05,
   address 0, ids 0x7E0, 0x7E8 and 0x7DF) are pinned above or equal the profile's whenever the
   profile does not need the ComParam; the D-PDU API side has library-specific defaults the guard
   cannot know, so the same set is required there too. The format ComParams' padding bits (Table B.13, bits 5 and 4) cannot
   contradict the pinned handling: an overwrite against the profile's padding is refused. Both workers replace the response table with the one a request carries, so a
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
   a TesterPresent request (`3E 00` or `3E 80`) on T1, T2 and T3, nothing else, session control
   included (a cyclic session change would need teardown state, also on the simulated ECU); T0
   refuses it, and a ComParam the profile's `[link.comparams]` fixes keeps the
   profile's value. A ComParam no rule covers passes only if it is on the short list of protocol
   and timing ComParams (session timing, ISO 15765-2 frame timing and flow control, response
   pending handling, tester-present timing; ISO 22900-2 (2022) Annex B) or the profile sets it.
   The short list holds only ComParams the J2534 worker's `SetComParam` accepts on CAN and ISO
   15765 (checked against `comparam_support.rs`): `CP_TesterPresentImmed`, which is on no
   real-protocol allow-list there, was removed, so it can be neither set nor pinned (the guard
   does not pass what the worker would refuse). Unknown, vendor and other physical-layer names
   are refused. The profile accepts in `[link.comparams]` only what the guard can enforce: a numeric
   ComParam of the short list above (byte fields, the ComParams the typed fields give, physical
   names and unknown or vendor names are refused at parse, naming the key and the typed field to
   use), so every pinned entry is compared with its value and required before connect. The
   profile check compares
   physical-layer names canonically too, so an alias such as `CP_Bit_Sample_Point` is refused
   like `CP_BitSamplePoint`. The profile check refuses two
   `[link.comparams]` keys of one canonical name (an alias or another case), whatever their
   values, so the profile has one value per ComParam; the alias table is shared by the profile
   check and the guard (`comparam`). `COPT_RESTORE_PARAM` is refused on
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
   count above one, or -1). RawMode is a T0 tool: a RawMode link is refused on T1, T2 and T3, at creation. A worker passes
   RawMode data through, with the identifier chosen by the client and the bytes after the header
   read as the UDS payload, and a native ISO 15765 channel segments it itself, so bytes that look
   like a flow control frame can be a routine request on the wire (`31 01 FF 00` after the
   identifier), and on T1 it would reach any identifier and could enter a session unseen. A
   raw-bound handle that appears in the context on T1, T2 or T3 anyway is refused,
   never decoded, so no frame decoder exists. On T0 a raw frame passes only to the sending
   endpoint's complete header: its request identifier, and under extended or mixed addressing its
   profile address byte as the fifth byte, with a payload long enough to hold it. Its content
   is not read; T0 has no communication to start,
   so `COPT_STARTCOMM` and `COPT_STOPCOMM` are refused there. On a RawMode link (T0 only) the worker
   takes the frame's format from the flags alone, so the three wire-format flags
   (`TX_FLAG_CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`, `ISO15765_FRAME_PAD`) must each equal the
   profile's, set or not, as named bits or as a raw TxFlag of four bytes with named bits only. T1
   passes ordinary requests undecoded. On every class and link mode, TxFlags that would
   override the profile are refused: frame padding when the profile has none (the J2534 worker
   turns padding on from the flag, apart from the ComParam), the 29-bit flag on 11-bit
   identifiers, and the extended addressing flag on normal addressing. The agent's list is kept apart on
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
   joined, and the remaining steps run. `Drop` runs the rest and never panics. `RunGuard::teardown()` gives the check a
   handle that registers steps and sets the timeout but cannot run them; running goes through the
   guard, whose `run_teardown()` does early cleanup and keeps the failures, so a failure is never
   returned to the check and lost. The guard's drop runs the rest; the guard's `Drop` runs it before anything else, whatever the check did, and when
   a step failed (early or at drop) it turns a recorded pass into a failure naming the steps (and appends them to
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
- The guarded handle (`guard::GuardedHandle`) uses the decision: it keeps the link table and the
  learned ids, passes every call of a check through `decide`, binds and unbinds links on the
  grants, and registers the teardown steps (the device, each link, the return to the default
  session before a session-entering request is sent). The seam that routes the agent's job runner
  through `decide` is separate work; until it exists, a job the agent runs is not mediated.
- The handle creates the profile's links itself, through the guard, with every ComParam
  `required()` lists at the profile's value (the table's identifiers, formats and address in the
  one-entry response table, the rest with `SetComParam`), because the agent's own link setup does
  not set them. A link that entered another session is returned to the default one before the
  handle destroys it (the destroy is refused unless the ECU answers `50 01`), and the staged response timings take
  effect at the connect or after a finished ComParam update. Power is not registered by the handle: a check that switches power registers its
  off step first.
- RawMode is refused on T1, T2 and T3: the frame form is settled against the workers, but a
  worker passes the data through, the identifier is the client's and the bytes after the header
  are UDS data, so on those classes the guard would need a frame decoder. On T0 the form is the
  literal frame: `j2534-0404-service` transmits `cop_data` unchanged as the message data (the
  CAN identifier in four bytes, the address byte when the `ISO15765_ADDR_TYPE` flag is set, then
  the payload), and the hardware ISO 15765 channel it requires for RawMode segments the payload,
  so the payload carries no ISO 15765-2 (2024) protocol control information. `iso22900-service`
  forwards the data and flags to the D-PDU library, which defines the form (ISO 22900-2 (2022)
  D.2.1). The T0 check (identifier, then the address byte, content unread) matches both.
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
