# Hardware profile and check declarations

`ngr-hw-checks` is the harness of the real-hardware checks (design 13.5, ADR-275). This page
describes the hardware profile it reads, how a check declares what it needs, and how the two are
compared, how the device a worker reports is matched with the profile, and how the harness opens
it, how a run's report is written, and what the guard decides about each worker request and how a
run's teardown works (ADR-333). A check reaches the worker only through `guard::GuardedHandle`
(the "Guarded handle" section), which decides every call with the guard and registers the
teardown steps. A job the agent runs is mediated the same way, through `guard::GuardedClient`,
which implements the agent's worker-client trait over the same handle (ADR-334; the "Agent
jobs" part of the Guarded handle section).

## Running

```sh
# List the checks the profile permits, without running any (design 13.5, procedure step 2).
NGR_HW_PROFILE=/path/to/profile.toml cargo run -p ngr-hw-checks -- list

# Run them.
NGR_HW_PROFILE=/path/to/profile.toml cargo nextest run --profile hardware --run-ignored=only
```

The `hardware` profile in `.config/nextest.toml` selects the tests whose name starts with `hw_`
and runs them one at a time without retries. The `hw_` tests are `#[ignore]`d, and a nextest
profile cannot include ignored tests by itself, so the command needs `--run-ignored=only`. CI
never runs it. Without `NGR_HW_PROFILE` every hardware check fails.

## The profile

A TOML file outside the repository. Unknown keys are refused.

| Key | Meaning |
|---|---|
| `class` | `"T0"`, `"T1"`, `"T2"` or `"T3"` (design 13.5) |
| `[vci]` | T1 to T3: the VCI (below) |
| `[link]` | T1 to T3: the link (below) |
| `[[endpoint]]` | T0 only, exactly two: each names a `[endpoint.vci]`, with its `channel` (such as the protocol name of an SAE J2534-2 clause 7 additional channel) at both ends when both are on one VCI, or a `socketcan` interface (such as `"can0"`), and a `[endpoint.link]`. At least one end is a VCI channel, the two ends differ (two ends on one VCI by their `channel`), and their links are one bus: the same `baud_rate` and `id_bits`, each end's `request_id` the other's `response_id` |
| `allow` | Optional. Narrows the class to the listed operations: `"configuration-write"`, `"routine"`, `"reprogramming"`, `"fault-injection"`. Without it the profile keeps its class's operations |
| `report_full_identifiers` | Optional, `false` by default: `true` puts VINs and serial numbers in the run's report in full instead of masked (see "Reports") |
| `expendable_ecu` | T2 only: the ECU may be reprogrammed |
| `[power]` | T1 and T2 only: the commands that switch the supply `off` and `on`, each a program and its arguments |
| `[[hardware-control]]` | T1 and T2 only: a hardware control the wiring is safe for, with its complete parameters (below) |
| `[read]` | T1 to T3, optional: the definition the read check runs (see "The T2/T3 read check"). `definition` is a directory of CSV tables and Starlark files, and `procedure` the file name of the read procedure's Starlark file in it |

The operations a class allows by itself, beyond reads:

| Class | Operations |
|---|---|
| T0 | none (bus traffic between the two endpoints only) |
| T1 | configuration writes, routines, reprogramming, fault injection |
| T2 | configuration writes and routines, inside an authorized job; reprogramming only with `expendable_ecu` |
| T3 | none |

An `allow` entry outside the class's operations is refused, and so is every addition the class
does not take. A T3 profile can add nothing.

`[vci]`:

| Key | Meaning |
|---|---|
| `api` | `"j2534-0404"` or `"iso22900"` |
| `library` | The library name, resolved as in design 7.1 |
| `module` | D-PDU API: the device name or serial number the library reports (see "The module match"). J2534: a description only, not compared with the device |
| `label`, `pname` | J2534 only, and required there, not empty, and the label is not `j2534-0404` (the worker's default entry): the label and the device name of the configured `modules` entry (ADR-281) |
| `firmware_version`, `library_version` | What the device reports, compared after opening (see "The module match"); a D-PDU API profile writes each as `major.minor.revision`, each part 0 to 255 with no leading zero |
| `timing_margin_us` | Optional: the VCI's timing margin in microseconds (design 8.11 "Timing margin"), the key the agent reads too. The T2 timing check judges the latency the VCI measured only beyond it, and without it judges nothing (see "The T2 timing check", ADR-356). A job the harness runs takes it as its links' timing margin; without it the job's log judges no time against a limit, since the agent has no default (ADR-377) |
| `drift_bound_ppm` | Optional: how far the VCI's clock may run from the agent's, in parts per million (design 8.11 "Drift bound"). A job the harness runs takes it; without it the job takes the agent's default (100 ppm, ADR-357) for placing clock times, and judges no loss's settlement (ADR-378) |
| `clock_margin_us` | Optional: the tolerance of one reading of the VCI's clock in microseconds (design 8.11 "Drift bound"). A job the harness runs takes it; without it the job takes the agent's default (1 ms, ADR-357) for placing clock times, and judges no loss's settlement (ADR-378) |

`[link]`, the values an IR `Protocol` declaration carries:

| Key | Meaning |
|---|---|
| `protocol` | The ISO 22900-2 (2022) protocol short name the link's resource is created by, as the agent sends it: `"ISO_14229_3_on_ISO_15765_2"` for UDS on CAN. The guard compares it with the resource of a `CreateComLogicalLink` ignoring ASCII case and has no alias table |
| `baud_rate` | Bits per second |
| `pins` | The two connector pins (1 to 16; SAE J2534-1 6.8 lists their use), CAN high first |
| `request_id`, `response_id`, `functional_id` | The physical request and response identifiers, and optionally the functional one; all different |
| `id_bits` | 11 or 29; every identifier must fit |
| `addressing` | `"normal"`, `"extended"` or `"mixed"`; the last two need an `address` byte |
| `padding` | The padding byte; absent for no padding |
| `[link.comparams]` | Numeric protocol and timing ComParams by name (`CP_...`) that the guard enforces: P2, P2*, the ISO 15765-2 timing and flow control (`CP_BlockSize`, `CP_STmin`, ...), response pending and tester-present timing, each a value from 0 to 4294967295 (the workers set them as unsigned 32-bit numbers). A byte-field ComParam (such as `CP_TesterPresentMessage`), a ComParam the typed keys above give (`CP_Baudrate` is `baud_rate`, the identifiers, formats, address, filler and address modes) and an unknown or vendor name are refused, naming the key. Two keys that name one ComParam (the same name in another case, or an alias such as `CP_BSTx` and `CP_BS_TX`, which the guard resolves to one canonical name) are refused, whatever their values. A physical-layer ComParam is refused by its canonical name,
so an alias (`CP_Bit_Sample_Point`) is refused like the name. The physical layer (the bus type ComParams of ISO 22900-2 (2022) Annex B.3.2, such as `CP_Baudrate`, `CP_CANFDBaudrate` or `CP_TerminationType`, in any case) comes from the keys above and is refused here |

`[[hardware-control]]`, by `kind`, each with a `name` that checks refer to:

| `kind` | Parameters |
|---|---|
| `programming-voltage` | `pin`, `millivolts` |
| `pin-switch` | `pins`, each a connector pin once |
| `ioctl` | `id`, and either its exact `payload` (bytes) or its safe `range` (`[low, high]`) |

## Telling the two loopback ends apart

The profile cannot see the hardware, so it only refuses what cannot be two channels:

- Library, module, label and `pname` are compared exactly, as the library resolvers match them.
- Two J2534 ends are on one module when they name the same library and the same `label` or the
  same `pname` (ADR-281), whatever their `module` says. Two D-PDU API ends are on one module
  when they name the same library and `module`.
- Two ends on one module must describe it identically and name different `channel`s, compared
  as the J2534 worker reads them: `CAN_CH1`, `can_ch1` and `CAN_CH01` are one channel.
- One device reached through both APIs, one at each end, looks like two modules here. Design
  13.5's procedure (only the profile's VCIs connected) and the module match against the attached
  device cover it.

## The module match

`ngr_hw_checks::device` decides from a worker's answers whether the attached device is the
profile's, so that a check can refuse a wrong one before it creates a link (design 13.5, ADR-275
item 4). The answers are the modules of the worker's `GetModuleIds`, its `GetVersion` after
opening, and for J2534 the `modules` entries of the worker's configuration (ADR-281).

Values are compared without their surrounding spaces, and a profile whose `module` (D-PDU API)
or `label` (J2534) is blank is refused before anything is matched.

- **D-PDU API.** A library writes `vendor_module_name` as a list of `attribute='value'` pairs or
  as plain text (ISO 22900-2 (2022) 10.1.4.6). Exactly one reported module must have the
  profile's `module` as its whole `vendor_module_name` or as the value of one of its attributes,
  such as a serial number; none, or several, is refused. `vendor_additional_info` is not read: it
  describes how the module is connected, which can change while the module stays the same.
- **J2534.** A J2534 library reports no serial number and lists configured modules whether or not
  they are present, so the configured `modules` entry with the profile's `label` must be the only
  entry, and its `pname` must be the profile's. Exactly one reported module carries that label
  (`j2534-0404-service` reports the label as `vendor_module_name`), and its
  `vendor_additional_info`, the library the worker loaded, must be the profile's `library`. The
  profile's `module` is not compared. Requiring a single configured entry is ADR-275 item 4's
  rule; a worker that starts only the label it is asked for (ADR-281) would let it be relaxed.
- **Versions.** J2534 reports its versions as strings: the firmware version in `hw_name` and the
  library (DLL) version in `fw_name`, compared exactly with `firmware_version` and
  `library_version`. The D-PDU API reports numbers, coded as major, minor and revision in their
  three high bytes (ISO 22900-2 (2022) 10.1.4.15): `fw_version` and `pdu_api_sw_version` are
  compared as `major.minor.revision`, so a D-PDU profile writes `firmware_version = "2.1.0"`;
  any other form is refused when the profile is read.

## Opening the device

`ngr_hw_checks::open::open_device` is the harness's step from a profile's `[vci]` to an opened
device (design 13.5, ADR-275 item 4). It takes the `Vci`, the workers directory
(`WorkerLayout`), the operating mode and the launch options, and does the following in order:

1. Starts the worker for the profile's `library` through the agent's launch path
   (`agent::launch::launch_worker`), so the library is resolved, and the worker build chosen, as
   in production (design 7.1, 7.3).
2. Asks the worker for its modules (`GetModuleIds`), reads the `modules` entries of the
   configuration for J2534 (`vci-service-config`, in the harness's process), and runs
   `select_module`. A refusal here comes before anything is opened. For J2534 the worker must
   report exactly one module.
3. Opens the selected module (`ModuleConnect`), reads `GetVersion` and runs `check_versions`.

It returns an `OpenedDevice` only when both matches hold. On a refusal, or when a worker request
fails, it stops the worker, and disconnects the module first when `ModuleConnect` was called (a
failed connect can leave a J2534 device open). It returns an `OpenError`; `Refused` wraps the
`MatchRefusal` and says how the worker ended. Every worker request is bounded by the launch
options' `request_timeout` (set it high enough for the device's connect), a hung disconnect does
not keep the worker from being stopped, and a worker that does not exit is killed. It creates no
link; a check creates links only on the returned device, and `OpenedDevice::close` disconnects
the module and stops the worker.

Residual for J2534, until the worker can be started with the profile's label and `pname`
(ADR-281 Decision item 3): the worker lists every `modules` entry of the file it reads, and the
harness reads its own view of that file. A worker that reads a file with a single entry that
names another device than the profile's `pname` passes the match, since the harness cannot see
that file. The profile refuses the label `j2534-0404`, which a worker gives its default entry
when the file is absent, so that entry is never taken for the profile's. A file that fails the
worker's writability check (design 7.2) stops a device-mode worker from starting and is applied
in user mode. What covers the remaining case is design 13.5's procedure step 1, that only the
profile's VCI is attached while a check runs.

Tests: `crates/iso22900-service/tests/hw_open.rs` and `crates/j2534-0404-service/tests/hw_open.rs`
open the D-PDU API and J2534 mocks through a worker and show each refusal they can reach.

## Check declarations

A check declares its requirements as a `CheckDecl` constant in `ngr_hw_checks::CHECKS`: its
test name, the classes it runs on and the operations it needs beyond reads (the operations
above, `PowerSwitching`, and `HardwareControl` with a control's name). A check runs only when
the profile's class is one of its classes and the profile allows each of its operations. The
`list` command and `ngr_hw_checks::listing` compare every declaration with the profile without
running anything, and `ngr_hw_checks::begin`, the first call of every `hw_` test, refuses the
check the same way before it reaches any device. `begin` also returns the run's report guard, so
a refused check still leaves a report recording the refusal, and a profile that cannot be read
leaves one with the class `unknown` and every identifier masked. A check whose recorded result is
not a pass (`finish` with failed or refused) fails as a test as well, whether or not it panicked. A `hw_` test in any crate declares itself in
`CHECKS`, so the listing covers every check; a test that is not there is not listed.

"Permitted" means only that the profile's class and operations cover the declaration. What a
check then sends is limited by the guarded handle (design 13.5 "Guarded VCI access"): a T2 write
passes only inside an authorized job, and T3's other limits (no DTC clearing, resets, security
access or session beyond the extended one) are the class's lists, not declared operations.

## Guard

`ngr_hw_checks::guard::decide` is the decision core of the guarded VCI handle (design 13.5
"Guarded VCI access", ADR-275 item 4, ADR-333). It is a pure function of the profile, the links
the harness has bound, the ids it has learned from the worker's `GetResourceIds` and `GetObjectId`
answers, and one request (`guard::Request`, one variant per RPC of `service VciService`). It
returns a `Grant` (`Pass`, `CreatesLink`, `SetsComParams`, `DestroysLink`, `EntersSession`) or a `GuardRefusal`.
Anything it cannot classify is refused.

- **Link setup.** `ModuleConnect` is refused (the harness opens the device). A link is created
  only from the profile link's protocol, the bus type the agent uses
  (`ISO_11898_2_DWCAN`) and exactly its two pins (the first of `pins` as `HI`, the second as
  `LOW`), by resource data or by a learned resource id; a resource by name, or an id not learned,
  is refused. A create flag may carry RawMode only, and RawMode only on T0 (a worker passes raw
  data through, so on any other class the guard cannot classify its identifier or payload). One link per profile endpoint (T0: one per
  VCI end, created for the end whose `channel` the resource's protocol name is, or else whose
  `protocol` it is; a SocketCAN end has no worker link). `ConnectComLogicalLink` is refused until
  the pinned ComParams (baud rate, request, response and functional identifiers, the physical, response and functional formats on
  every link, the address byte when the addressing has one, the request and tester-present address modes (physical when the profile has no functional
  identifier, since the worker's preset sends tester present functionally), the tester-present message (T1 to T3), the filler handling (0 on an unpadded profile, since
  the worker's preset pads) and, when the profile pads, the filler byte, `[link.comparams]`) have
  been set to the profile's values, which the caller records from `SetsComParams` grants. A
  `SetUniqueRespIdTable` replaces the table on both workers, so it must carry the request and
  response identifiers (and the physical and response formats, and the address byte when the addressing has one), each at the profile's
  value, in exactly one entry; an empty or partial table, or one with more than one entry, is
  refused. The `Context` names the profile VCI its worker
  fronts, and a link is created only for an endpoint on it (on a T0 of two VCIs each worker's
  context binds its own end). `SetComParam` and the ComParams of `SetUniqueRespIdTable` keep the profile's values:
  `CP_Baudrate`, the identifiers, the CAN ID format (ISO 22900-2 (2022) Table B.13 and B.14), the
  address byte and the padding; a `CP_TesterPresentMessage` on T1, T2 and T3 may only be a
  TesterPresent request (`3E 00` or `3E 80`), since the worker sends it cyclically; other physical-layer ComParams are refused; functional
  addressing needs a `functional_id`; a `[link.comparams]` entry keeps its value. TxFlags that override the profile (frame padding
  without profile padding, the 29-bit or extended-addressing flag against the profile) are
  refused on every class. The rules are an
  allow-list over canonical names (case folded, the J2534 worker's aliases such as `DATA_RATE`
  resolved, the request's class field ignored): a ComParam no rule covers passes only when it is
  a listed protocol or timing ComParam (P2, P2*, ISO 15765-2 timing and flow control, response
  pending, tester-present timing) (only ones the J2534 worker can set: `CP_TesterPresentImmed` is not on its lists, so it is
  refused), `CP_RC21Handling` and `CP_RC23Handling` pass only at 0 (the agent's link turns the
  API's repeat on a busy answer off), `CP_TransmitIndEnable` and `CP_StartMsgIndEnable` at 0 or
  1, and every other name is refused. The tester-present message must also be set before
  a T1 to T3 link connects, and only as `3E 00` or `3E 80`. `COPT_RESTORE_PARAM` is refused.
- **Requests.** T1 passes every request. T0 creates RawMode links only, passes only raw frames with the sending end's own
  `request_id` and, under extended or mixed addressing, its `address` as the fifth byte and refuses `COPT_STARTCOMM`/`COPT_STOPCOMM` (no communication to start). On a
  RawMode link the 29-bit, addressing and padding flags must each equal the profile's, set or
  not. T2 (until an authorized job exists) and T3 pass the read-only list: session
  control to the default or extended session, ReadDataByIdentifier, ReadDTCInformation with a
  defined report type, and TesterPresent with sub-function 0, each with its length rule. The
  rule applies to direct requests, periodic ones and a `CP_TesterPresentMessage`. A periodic or
  repeated DiagnosticSessionControl to a non-default session is refused (it would enter the
  session again after the harness returned the ECU to the default one). A raw-bound
  handle on T1, T2 or T3 is refused, not decoded. T0 raw frames also need the profile's flags.
- **IO controls.** `PDU_IOCTL_READ_VBATT` and the queue, buffer and receive-filter housekeeping
  pass on every class. Every other IOCTL, including programming voltage, is refused on every
  class, whatever the profile's `[[hardware-control]]` list says.

## Guarded handle

`ngr_hw_checks::guard::GuardedHandle` is what a check gets of the worker (design 13.5 "Guarded VCI
access"); the check never holds the worker client. It owns the client and the state the decision
rests on (the link table and the learned ids). It registers its teardown steps through a clone of
the run's `TeardownHandle` (`RunGuard::teardown()`, clonable, register-only), so it borrows nothing
from the `RunGuard` (`open` and `attach` take `&RunGuard`): a T0 loopback of two VCI modules opens
one handle per module on the same run, and the steps of all handles run in one reverse
registration order. Each handle's guard context binds the endpoints of its own VCI only, so the
handles share no guard state; a check opens one handle per VCI (a second handle on the same module
would create the profile's links again). Devices a check wants in the report are added by the
check with `report().add_device`, once per device; the handle adds none. A check runs on a multi-thread tokio
runtime: a teardown step waits for its requests on a thread of its own while the worker's
connection is served by the runtime.

- **Opening.** `GuardedHandle::open(profile, vci, device, options, run)` takes the `OpenedDevice`
  of `open_device`; `attach` takes a client and an already connected module of a worker the caller
  hosts (the closing is then the module disconnect only). Both register the device's closing with
  the run first, then create the profile's links: per endpoint on this VCI, `GetResourceIds` and
  `CreateComLogicalLink` (RawMode on T0 only), every ComParam the guard requires with the profile's
  value (the identifiers, formats and address of the physical request and the USDT response in the
  response table's one entry, the rest with `SetComParam`; the tester-present message is `3E 80`
  and both address modes are physical), the agent's timing and response-pending defaults
  (P2 1 s, P2* 5 s, 0x78 handling, completion limit 25 s) unless the profile pins one, then
  `ConnectComLogicalLink` and a subscription to the link's events. Every `SetComParam` carries
  the class `PDU_PC_SPECIFIED`, so the worker resolves the ComParam's real class (a concrete class
  would be forwarded to a D-PDU library as given); the response table's items are `UNIQUE_ID`. Each request is decided like a
  check's own. A profile the handle cannot plan (no value for a pinned name) fails before the link
  is created.
- **Calls.** One method per RPC a check can use, each asking `decide` first. A refusal is
  `HandleError::Refused`, and nothing reaches the worker. `ModuleConnect`, `ModuleDisconnect`,
  `SubscribeEvent` and `GetEventItem` are not offered: the harness owns the device and the event
  stream, which `send_receive(cll, request)` reads (it returns the final response, a negative one
  included, or `NoResponse`). It waits up to `HandleOptions::response_timeout`, by default the link's ceiling
  (P2, the 0x78 completion timeout and P2* the link is set to, the profile's when it pins them,
  plus 2 s), and cancels the primitive on every way out of the wait while it may still run (the wait ending,
  lost events, a failed or ended event stream) (a cancel that fails is then the error returned: the worker answers a cancel of a primitive that
  already ended with success). Zeros are read as
  `j2534-0404-service` reads them: P2 zero is 50 ms, P2* zero is 5 s, and a zero completion
  timeout is no ceiling in the worker, so the handle waits 10 minutes in all (whatever P2 and P2*
  are) and then cancels, rather than forever. A `SetComParam` of P2, P2* or the completion timeout through the handle
  updates the ceiling: until the link is connected or a `COPT_UPDATEPARAM` primitive has finished
  successfully (the handle waits for it) the longer of the old and the new value is waited for,
  afterwards the new one; an update that fails, is cancelled or times out is the error of the `start_com_primitive` call and
  changes nothing (`response_ceiling` reports it). A read of
  one DID expects `62` and that DID; a read of several expects `62` only, since the ECU may
  return the records in any order.
- **Abandoned waits and suspended queues.** A wait whose future is dropped (a check's own
  `tokio::time::timeout`) leaves the primitive recorded; the next primitive on the link, the
  return to the default session and the link's closing cancel it first. A return interrupted
  that way stays pending. A suspended transmit queue (`PDU_IOCTL_SUSPEND_TX_QUEUE`) is resumed
  by a teardown step, and before any return to the default session, since a suspended queue
  would hold the `10 01` back. A session change marks the return pending before it is sent, and the mark stays when
  the call succeeds, times out or is dropped; only a rejection by the worker (`HandleError::WorkerRefused`: the
  statuses `InvalidArgument`, `FailedPrecondition`, `NotFound`, `ResourceExhausted`,
  `Unimplemented`, `AlreadyExists` and `Unauthenticated`, which the workers return for a request
  they did not apply; or the lost VCI) takes it back. Any other status and every transport error
  is `HandleError::Rpc`, which says nothing about the worker's state, so the mark, and the saved
  resume of a suspension, stay. Resuming a transmit queue forgets only the suspensions that
  resume covers (the same link; a module's resume covers its links). The closing step of a link
  has the time for its three requests. A response ceiling also adds the completion timeout of a
  `0x21` / `0x23` re-request chain when its handling is on (the handle sets none, and the guard
  refuses a check that does, so the worker's preset, off, stands).
- **Outstanding primitives.** Every primitive started through `start_com_primitive` is recorded
  until the handle sees it end. Before a link's session is settled, it is disconnected or
  destroyed, its communication is stopped, and in the teardown steps (the return to the default
  session and the link's closing), the handle cancels every one still recorded (cancelling one
  that already ended succeeds and changes nothing), so a direct `10 03` cannot race the `10 01`.
  A link's suspended transmit queue is resumed before the link is disconnected or destroyed (the guard refuses an IO control on a destroyed link), and the saved resume of a destroyed link is dropped. The steps that cancel and resume first are given a request timeout for each primitive and queue they handle. The device's closing step is bounded by the request timeout (plus the worker's stop grace
  period for a worker process), never less than the 30 s default.
- **Registration closes.** When the run ends its teardown (`RunGuard` drop), registration closes
  under the same lock that finds no step left. A `TeardownHandle` that outlives the run, and any
  handle call that must register a step, is refused with `TeardownClosed`
  (`HandleError::TeardownClosed`), and a link created at that moment is released at once rather
  than left without a step. An early `run_teardown` does not close registration.
- **Agent jobs.** `GuardedClient::open` / `attach` hold the device (the module and its closing,
  no link) and are the `WorkerApi` client of `agent::run_program` and the other runner entry points
  (ADR-334); the production path is unchanged. Every call the job makes goes through the handle,
  so the guard decides it, and a refusal reaches the runner as `PermissionDenied`. The harness owns
  the module (`held_module`): the job's link set-up lists, connects and disconnects none, and the
  device's closing step disconnects it once. The runner's set-up gives the baud rate, timing, the
  two identifiers and nothing more, so the client completes it from the profile: on
  `SetUniqueRespIdTable` it appends the profile's other table entries (formats, address bytes;
  each format is segmented transport with flow control on, at the profile's width and
  addressing, since the J2534 worker installs no flow-control filter for a response format
  without flow control, ADR-040);
  on `ConnectComLogicalLink` it first sets every other pinned ComParam (address modes, the
  functional identifier, the filler, the tester-present message, the profile's own) at the
  profile's value, each added request decided and recorded like any other. The job's own values
  are compared with the profile's unchanged, so a mismatch is still refused. The handle keeps no
  event stream for the job's link (the job subscribes itself, and a worker keeps one subscription
  per link); it subscribes again only when it must read events itself, such as the return to the
  default session after the job. `GuardedClient::trace()` is the record of every decision (the RPC
  and the guard's answer), in order. The handle's request timeout is at least the runner's 5 s
  unary deadline, which bounds a guarded call that carries several worker requests. T0 is refused
  at link creation (a job's link is not RawMode). `HandleError::Rpc` and `WorkerRefused` carry the
  worker's status code, so a refusal by the worker reaches the runner unchanged, and the lost VCI
  as the status the host recognises.
  The handle keeps only the latest primitive of a job's link, since it does not read the job's
  events, and cancels that one before a disconnect or destroy (a cancel answered with an invalid
  handle counts as already ended). A job's own `10 01` is not observed, so the disconnect after it
  sends one more. A failed create with a held module marks the job's guards unconfirmed unless the
  worker or the guard refused it. The runner's link set-up refuses 29-bit identifiers and a
  document-declared link with extended addressing; a hand-built extended-addressing link works
  (the client appends the profile's extended format and address bytes), with the runner's
  `CP_RCByteOffset` of 2 unchanged and unchecked for that addressing.
- **Links created by a check.** Whatever creates a link through the handle, including a link a
  check destroys and creates again, gets the same per-link set-up: the teardown step, the event
  stream and timings to wait by (the handle's defaults, 1 s, 5 s and 25 s, until the check sets
  others). The check then sets the profile's ComParams itself, as the guard requires before the
  connect.
- **Events.** A lost VCI (the worker's link-wide error event, with no primitive handle, or one
  bound to the primitive; or a request refused on the offline link) ends the wait with
  `HandleError::VciLost`, as in the agent. The handle keeps the pending return to the default
  session in step with what `send_receive` observes: a positive answer to DiagnosticSessionControl
  to the default session clears it, one to another session sets it.
- **State.** A `GetResourceIds` answer records the resource each id stands for and a `GetObjectId`
  answer the name of the id, into the learned ids. The `cll_handle` of a granted
  `CreateComLogicalLink` is bound to its endpoint, a granted `SetComParam` or
  `SetUniqueRespIdTable` marks its pinned ComParams as set once the worker accepted it, and a
  granted `DestroyComLogicalLink` drops the link.
- **Teardown.** The steps are registered as the resources are acquired and run in reverse: the
  device's closing, then each link's disconnect and destroy (skipped when the check destroyed it),
  then, per link, one return to the default session (`10 01`, itself decided) registered before a
  request that enters another session is sent. That step reads the link's response wait when it
  runs (the longer of the timings in effect and those staged), waits no longer than that, and
  cancels the primitive when the wait ends; its timeout is read at the same moment (that wait, the
  start and cancel requests and a margin, at least the 30 s default of other steps), via
  `register_async_with_timeout_fn` (a panicking timeout callback fails its step like a panic in
  the step). A check that disconnects or destroys a link, or stops its communication, in another
  session has it returned first: the handle sends the same `10 01`, and refuses the destroy if the ECU does not
  answer with the positive `50 01` (a negative response is a failure too), so the step stays
  registered and reports the failure when it runs. The device step fails when the worker had to
  be killed. Power: the handle registers nothing for the
  profile's power commands. A check that switches power registers the power-off step itself before
  it switches power on.
- **RawMode form.** On a T0 link the data of a send is the literal frame the worker passes on.
  `j2534-0404-service` transmits `cop_data` unchanged as `PassThruMessage.Data`: the CAN identifier
  in four bytes, most significant first; the address byte next when the `ISO15765_ADDR_TYPE` flag is
  set (the guard requires the flag to equal the profile's addressing); then the payload. On an ISO
  15765 end the link is a hardware ISO 15765 channel (a software ISO-TP link refuses RawMode),
  which segments the payload itself, so the payload holds no ISO 15765-2 (2024) protocol control
  information (clause 9.6). On a raw CAN end the payload goes on the bus as it is, as the CAN
  frame's data.
  `iso22900-service` forwards `cop_data` and the TxFlags to the D-PDU library untouched, and the
  form is the library's (ISO 22900-2 (2022) D.2.1, Table D.4 gives the identifier in four bytes);
  it has no test of a RawMode send against a library. This is the header the guard compares on
  T0 (the sending end's request identifier and, under extended or mixed addressing, its address
  byte); the payload is not read.

## The T0 loopback check

`hw_t0_two_endpoint_loopback` (`ngr_hw_checks::T0_LOOPBACK_CHECK`, declared for T0 only, no
operation beyond the class's) is the first hardware check. The harness lists it for a T0 profile
and refuses it on T1, T2 and T3 (`cargo run -p ngr-hw-checks -- list`), and the test fails without
`NGR_HW_PROFILE`, like every `hw_` test. Its test is
`crates/j2534-0404-service/tests/hw_t0_loopback.rs`; its body is `checks::t0_loopback::run`, a
function over guarded handles and the profile, so CI runs it against `j2534-0404-mock`
(`tests/grpc_mock/t0_loopback_check.rs`, with a relay task playing the loopback).

The profile parser refuses a loopback whose two ends cannot exchange the same bytes. The ends must
be both raw CAN or both ISO 15765, by the names `j2534-0404-service` maps to those channels
(ignoring case, and an additional channel's `_CHn` suffix): `CAN`, `ISO_11898_RAW` and the
worker's other CAN aliases, or `ISO15765`, `ISO-TP`, the worker's other ISO 15765 aliases and the
ISO 22900-2 (2022) Annex B.1 names over ISO 15765 it recognises, such as
`ISO_14229_3_on_ISO_15765_2`. An ISO 15765 end segments its payload and a raw CAN end does not,
and any other protocol, or a name the worker does not recognise, is refused. A name is matched as
the worker matches it, without trimming spaces, and a `_CH` with no digits after it is no
additional channel. A raw CAN end takes no `padding` and only `normal` addressing: frame padding is an
ISO 15765-2 TxFlag with no meaning on a raw CAN channel (SAE J2534-1 (v04.04) 8.7.3), and the
worker drops the extended-addressing flag there, so the address byte would be plain payload. The ends must also
share the `addressing` and, when addressed, the `address`, since each end accepts only frames
carrying its own address byte.

- **What it does.** Both ends listen, then each end sends single RawMode frames of 1 to 7 bytes
  (6 under extended or mixed addressing, which takes a byte of the frame) that the other must
  receive byte for byte, A to B and B to A. Before each send the receiving end's monitor is drained
  (`raw_drain`, until 50 ms pass with no frame; a link that never goes quiet fails the check), so a
  copy of the frame received before it was sent cannot answer it. A frame that does not arrive
  within 3 s fails the check and names the frame. The result message records the library path,
  the ABI and the `unsigned long` width of each device, masked as the report masks. A VCI end must be a J2534 one: a
  D-PDU API end is refused with nothing sent.
- **SocketCAN end (ADR-343).** One end may be a `socketcan` interface instead, on Linux only; any
  other OS refuses the check before a device opens. The loopback must then be raw CAN (both
  `protocol`s raw CAN names such as `CAN`): an ISO 15765 VCI channel frames each payload with the
  transport's protocol control byte, which the SocketCAN end does not build, so an ISO 15765
  SocketCAN end is refused. The interface must be up at the profile's baud rate already, since
  the harness changes no interface settings. The end is a `guard::GuardedSocketCan` on a raw CAN
  socket, which offers only the T0 frame of its own end: the end's `request_id` (a 29-bit
  identifier when `id_bits` is 29), then a payload of 1 to 7 bytes. It takes no whole frame and no
  other identifier. It receives every frame other nodes send, skips error and remote frames, and
  matches the other end's frame byte for byte with the loopback's identifier width. Its drain ends
  after 50 ms without a frame and fails after 2 s, as a VCI end's does. The result message names
  the interface.
- **ABI check (design 7.1.2).** For each device the ABI read from the library file's header must
  be the worker's, and the width the launch passed to the worker must be the registration
  definition's `LongSize`, else the ABI's default (the test looks the definition up the way the
  launch did; on Windows it checks the default only). That is all it verifies: the worker does not
  report its width or ABI back, so neither is read from the running worker, and a mismatch shows a
  launch that went wrong. `OpenedDevice::library()` and `long_size()` give what the launch
  resolved (`agent::launch::LaunchedWorker`). The exact frame round trip is what exercises the
  `unsigned long` fields in practice; the result message says what each value is.
- **Addressing of the ends.** A link uses one address byte for the requests it sends and for the
  responses it accepts, and the worker builds its flow-control filter from it, so the profile
  check refuses a T0 loopback whose two ends differ in `addressing`, or in `address` when
  addressed: every frame would be dropped, and the failure would look like a hardware fault.
- **Workers and the library.** The worker directory (`<abi>/j2534-0404-service`, design 7.3) is
  the root `NGR_HW_WORKERS` names, else this workspace's build for the host ABI, installed in a
  temporary directory. The vendor's library is found by the OS lookup (design 7.1.1): the test fails
  if `NGR_J2534_DEFINITION_DIR`, the debug override of that lookup (ADR-282), is set. The test is
  not gated on `debug_assertions`. A launch that finds no worker for a library's ABI (a 32-bit DLL on Windows without
  `NGR_HW_WORKERS`) fails the check with a message that says to set it, and the temporary
  directory outlives the run's teardown.
- **Raw API.** On a T0 link the handle offers `raw_flags`, `raw_frame`, `raw_send`, `raw_listen`
  and `raw_receive`, each decided by the guard and refused on a non-T0 or non-RawMode link.
  `raw_flags` is the profile's TxFlags (29-bit identifier, extended addressing, padding), and
  `raw_frame` the end's request identifier in four bytes, most significant first, the address
  byte under extended or mixed addressing, then the payload; `raw_send` is a send with no
  receive. `raw_listen` starts the link's receive-only monitor (no send, receives until
  cancelled), with the end's own header as its data, since the guard reads any non-empty data of
  a RawMode primitive as a frame, and one expected response that matches any frame. The monitor is
  a started primitive, so the link's closing cancels it.
- **RawMode receive form.** A receiver gets the whole frame the sender transmitted: the sender's
  request identifier, the address byte if any, then the payload, in `ResultData.data_bytes` with
  no header split. `raw_receive(cll, expected, timeout)` returns the first frame equal to
  `expected`, skips the echoes of the link's own transmissions (the `TX_MSG_TYPE` and
  `TX_INDICATION` RxFlag bits) and buffers other frames of the monitor for a later call (up to
  1024 per link, then `RawBufferFull`), so the order of the calls loses none; `NoResponse` when the
  deadline passes and `MonitorEnded` when the monitor ended (a cancel, or a nonzero pinned
  `CP_CyclicRespTimeout`). `raw_send` also waits (2 s) for the send's own end, so a transmit error
  is `SendFailed`, which the check reports as a send failure of that end. A link has one monitor:
  a second `raw_listen` returns it.
- **One module, two channels.** The check takes the two ends as one handle with two links or as
  two handles. The test runs two modules: a link on an additional channel (`..._CH1`, SAE J2534-2
  clause 7) is made by the worker from a compound protocol name that `GetResourceIds` does not
  accept, and the handle creates links through `GetResourceIds`, so a loopback of two channels of
  one module is refused by the test with that reason.

## The T2/T3 read check

`hw_t2_t3_read` (`ngr_hw_checks::T2_T3_READ_CHECK`, declared for T2 and T3, no operation beyond
reads) runs a CSV + Starlark definition's reads on a bench ECU or a vehicle through the agent's job
runner. The harness lists it for a T2 or T3 profile and refuses it on T0 and T1, and the test
fails without `NGR_HW_PROFILE`. It has a test beside each worker, under the same name:
`crates/j2534-0404-service/tests/hw_t2_t3_read.rs` for a J2534 VCI and
`crates/iso22900-service/tests/hw_t2_t3_read.rs` for a D-PDU API VCI. Both run the body
`checks::t2_t3_read::run` over the guarded client (`guard::GuardedClient`, ADR-334), so CI runs
it on the example ECU's definition through each worker (`tests/t2_t3_read_check.rs` in both
crates): against `sim-vci` behind `j2534-0404-service`, and against the simulated ECU
`iso22900-mock` answers from (ADR-299) behind `iso22900-service`. Each test refuses a profile
whose VCI belongs to the other worker, and a set debug-only override of the library lookup
(`NGR_J2534_DEFINITION_DIR` or `NGR_PDU_API_ROOT_FILE`, ADR-282).

- **The definition.** The profile's `[read]` names it. `checks::t2_t3_read::convert` converts
  every file of `definition` but the Starlark ones (`.star` in any case, as the frontend reads
  them), with the `procedure` file, which must be one, into one document
  per variant (design 8.4), before any device opens, so a definition that does not convert sends
  nothing. The definition must declare the profile's link: the guard compares the job's link with
  the profile's, so the profile's `protocol` is the name the agent's link gives its resource,
  `ISO_14229_3_on_ISO_15765_2`, and its identifiers and baud rate are the definition's.
- **What it does.** Each candidate variant's identification services are sent to the ECU as one
  job (`diag_ir::identify`, as a server identifies before it sends a job, design 8.9.1), and the
  variant whose patterns the answers match is selected; then that variant's read procedure runs as
  a job, and it checks the values it decodes. A job whose program writes is refused before it
  starts. The jobs take the VCI's slot in a lock directory of the run's own, and their
  communication logs go to the run's temporary directory.
- **What the guard does.** Every request goes through it, so on T2 and T3 a write, routine, reset
  or security access is refused before it reaches the worker, and the job fails with the refusal.
- **Result.** It passes with the variant identified when the read procedure finishes. A
  definition that does not convert, a profile without `[read]`, and a profile whose VCI is
  the other worker's are recorded as refused; a job that fails, or answers that match no variant, as
  failed.

## The T2 timing check

`hw_t2_timing` (`ngr_hw_checks::T2_TIMING_CHECK`, declared for T2 only, no operation beyond
reads) records a bench ECU's session timing beside the values `sim-ecu` uses (ISO 14229-2 (2021)
9.2), so the simulator can be checked against a real ECU. The harness lists it for a T2 profile
and refuses it on T0, T1 and T3, and the test fails without `NGR_HW_PROFILE`. It has a test
beside each worker, `crates/j2534-0404-service/tests/hw_t2_timing.rs` for a J2534 VCI and
`crates/iso22900-service/tests/hw_t2_timing.rs` for a D-PDU API VCI; each refuses a profile
whose VCI belongs to the other worker and a set debug-only override of the library lookup, as the
read check's tests do. The body is `checks::t2_timing::run`, over the guarded handle and the
profile's link, and CI runs it against `sim-vci` behind `j2534-0404-service`
(`tests/t2_timing_check.rs`), with the simulated ECU configured to a P2 and P2* other than
`sim-ecu`'s defaults.

- **What it sends.** DiagnosticSessionControl for the default session, whose positive response
  carries the server's P2 in 1 ms units and P2* in 10 ms units (ISO 14229-1 (2026) 9.2), then
  TesterPresent with its response not suppressed (9.7), `checks::t2_timing::SAMPLES` requests in
  all. Both are on the T2 and T3 read-only list, so the guard passes them.
- **What it records.** The reported P2 and P2* beside `sim-ecu`'s defaults
  (`DEFAULT_P2_SERVER_MAX_MS`, `DEFAULT_P2_STAR_SERVER_MAX_MS`), each marked when it differs, the
  agent's round trip of each request, and the latency the VCI measured for each (ADR-356). The
  round trips start before the RPC and include the worker and the event path, so they are
  reported and never judged against P2 (design 8.11 "Timing judged").
- **The VCI's latency.** The guarded handle turns the transmit and start-of-message indications
  on for the link (a library may refuse either) and `GuardedHandle::send_receive_timed` returns
  each request's transmit done, response start and response end with the clock each item names.
  The latency runs from the transmit done to the response's start, or without one on that clock
  to its end, named `vci_tx_done_to_rx_start` or `vci_tx_done_to_rx_end` (design 8.11
  `measured_by`). Two times on different clocks, a time with no clock, or a later time that reads
  earlier (the 32-bit values are not unwrapped) give no latency. With `timing_margin_us` in
  `[vci]` each latency is judged against the reported P2 plus the margin: a start both ways, an
  end one way only, so an end past the limit proves a response of at most six bytes (one frame
  under every addressing format) late only past one frame's transfer at the link's baud rate,
  and never proves a longer one late. A response past the limit is not judged after a response
  pending (the limit in force is then P2*, design 8.11 "Limit in force") or on a link whose
  worker does not say it passes on every pending (`pending_forwarding`, ADR-355), since the VCI
  may have absorbed one. Without the key the latency is recorded and nothing judged. Against `sim-vci` the latency runs to the response's end, since `sim-vci` gives no
  start of message for a single frame.
- **Result.** It passes when the ECU answers every request with its positive response; the
  report's message holds the comparison and each latency with its verdict. A reported value
  that differs from `sim-ecu`'s, and a late response, are not failures. A negative or malformed
  response, a request the ECU does not answer, and a link that does not open fail it.

## Teardown

`ngr_hw_checks::teardown::Teardown` holds the steps a check registers as it acquires a resource
(power, device, link): `register` for a closure, `register_async` for a future. `run` executes
them in reverse, each at most once, each under `catch_unwind`; a failing or panicking step does
not stop the others. An async step runs on a detached thread with its own runtime; the caller waits for it with a
timeout (`set_timeout`; 30 s by default), so it can be run from inside a runtime and a step that
blocks is reported as timed out and left behind while the rest run. A `RunGuard` owns one: `RunGuard::teardown()` gives a handle that registers steps and sets
the timeout only, `RunGuard::run_teardown()` runs them early and keeps their failures, and the
guard runs the rest when dropped, also while a check panics, and turns a failed
step into a failed check even if the check recorded a pass.

## Reports

Each run writes a report (design 13.5 "Reports"). A check builds it with
`ngr_hw_checks::report::Report::new(&profile)`, adds its results with `record` (passed, failed or
refused, with a message) and `record_refusal`, adds what the opened device reported with
`add_device` for each device it opened, and ends with `write`. A `hw_` test normally holds a `report::RunGuard` instead
(`RunGuard::start(&profile, name)`), which writes the report when it is dropped: a check that fails
by panic still leaves its report, with the check recorded as failed, and so does one that ends
without recording a result. The test ends with `pass()` or `finish(outcome, message)`.
`hw_the_profile_permits_a_check` does this. A report that cannot be written fails a check
that would otherwise pass, since every run leaves a report; when the check is already failing by
panic, the write error goes to stderr so it does not hide the check's own failure.

- **Content (TOML).** `started_at` (UTC, RFC 3339), `host` (the `HOSTNAME` or `COMPUTERNAME`
  variable, else `/etc/hostname`), `os` (the OS and architecture the harness was built for) with
  `os_version` where `/etc/os-release` gives one, `class`, `identifiers_in_full`, one `[[device]]`
  table per opened device (a T0 loopback may open two) and one `[[check]]` table per result.
  `[[device]]` holds `abi` (the worker's), `vendor` and
  `model`, `firmware_version` and
  `library_version` (J2534: the strings the worker reports; D-PDU API: the coded numbers as
  `major.minor.revision`). There is none when no check opened a device. For the D-PDU API,
  `vendor` and `model` are the vendor and hardware names `GetVersion` reports, falling back to
  the profile's library and the module's `vendor_module_name` when the library leaves them empty.
  J2534 reports neither (its worker answers constants), so for J2534 they are the profile's
  library and `pname`, the device name of the configured entry (the label only selects it). No serial-number field is read from the device.
- **Place.** `nekoguruma/hw-reports` in the user's data directory: `$XDG_DATA_HOME` or
  `~/.local/share` on Linux, `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on
  Windows. A file is named by the run's start time and process id (`20261019T101500Z-4242.toml`),
  with a counter if that name exists; a report never overwrites another. A directory inside this
  repository's work tree is refused, after symlinks in the path are resolved (a `..` below the existing part of the path is refused). How long reports are kept is up to the owner of the setup.
- **Permissions.** On Unix the directories are created with mode 0700 and each file with mode 0600
  in the creating call; an existing report directory that group or others can access is refused.
  On Windows the files take the ACLs of the user's profile, which by default only that user and
  administrators can read; the harness sets no ACL.
- **Masking.** Unless the profile sets `report_full_identifiers`, a masked value is `****` and its
  last 4 characters (all of a value shorter than 8 characters is replaced by `****`). In every
  text field (the host name, the OS description, check messages, what the device reports, and
  the library, label and `pname` the profile gives) every serial-like token is masked: a run of letters, digits, `-` and `_` that holds at least 8
  letters and digits, one of them a digit. That covers every VIN and serial numbers in forms such
  as `SN-1234567890`, whether or not the profile names them; a hexadecimal literal (`0x7E8`), or a
  run of them joined by `-` or `_`, is kept as a code. Masked too are the values of the module
  name's attributes whose name contains "serial", a D-PDU API module name that has no attributes
  (a device name or a serial number), and the profile's D-PDU API `module` wherever it occurs.
  A library named like `op20pt32` is therefore written as `****pt32`. A check that writes its own free text goes through `Report::masker()`. Masking
  is a heuristic both ways: it also masks other long codes written without `0x`, such as a 29-bit
  CAN identifier, and a serial number in a form none of these rules covers stays in the report,
  so read a report before sharing it.
