# Hardware profile and check declarations

`ngr-hw-checks` is the harness of the real-hardware checks (design 13.5, ADR-275). This page
describes the hardware profile it reads, how a check declares what it needs, and how the two are
compared, how the device a worker reports is matched with the profile, and how the harness opens
it, and how a run's report is written. The guarded VCI handle is not part of the crate yet.

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
| `protocol` | The protocol name, such as `"ISO15765"` |
| `baud_rate` | Bits per second |
| `pins` | The two connector pins (1 to 16; SAE J2534-1 6.8 lists their use), CAN high first |
| `request_id`, `response_id`, `functional_id` | The physical request and response identifiers, and optionally the functional one; all different |
| `id_bits` | 11 or 29; every identifier must fit |
| `addressing` | `"normal"`, `"extended"` or `"mixed"`; the last two need an `address` byte |
| `padding` | The padding byte; absent for no padding |
| `[link.comparams]` | The transport ComParams by name (`CP_...`). The physical layer (the bus type ComParams of ISO 22900-2 (2022) Annex B.3.2, such as `CP_Baudrate`, `CP_CANFDBaudrate` or `CP_TerminationType`, in any case) comes from the keys above and is refused here |

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
  library and the configured label. No serial-number field is read from the device.
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
  last 4 characters (all of a value shorter than 8 characters is replaced by `****`). Masked are
  every VIN-shaped token (17 letters and digits with at least one digit, whether or not its check
  digit holds) in any text, the values of the module name's attributes whose name contains
  "serial", a D-PDU API module name that has no attributes (a device name or a serial number),
  and the profile's D-PDU API `module` wherever it occurs in a message. A J2534 module name is a
  configured label and is kept. A check that writes its own free text goes through
  `Report::masker()`. Masking is a heuristic: a serial number in a form none of these rules
  covers stays in the report, so read a report before sharing it.
