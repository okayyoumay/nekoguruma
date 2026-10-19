# Hardware profile and check declarations

`ngr-hw-checks` is the harness of the real-hardware checks (design 13.5, ADR-275). This page
describes the hardware profile it reads, how a check declares what it needs, and how the two are
compared, how the device a worker reports is matched with the profile, and how the harness opens
it, how a run's report is written, and what the guard decides about each worker request and how a
run's teardown works (ADR-333). The seam that routes a check's, or the agent's, worker calls
through the guard is not part of the crate yet.

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
  refused) and every other name is refused. The tester-present message must also be set before
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
  set (the guard requires the flag to equal the profile's addressing); then the payload. The
  link is a hardware ISO 15765 channel (a software ISO-TP link refuses RawMode), which segments
  the payload itself, so the payload holds no ISO 15765-2 (2024) protocol control information.
  `iso22900-service` forwards `cop_data` and the TxFlags to the D-PDU library untouched, and the
  form is the library's (ISO 22900-2 (2022) D.2.1, Table D.4 gives the identifier in four bytes);
  it has no test of a RawMode send against a library. This is the header the guard compares on
  T0 (the sending end's request identifier and, under extended or mixed addressing, its address
  byte); the payload is not read.

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
