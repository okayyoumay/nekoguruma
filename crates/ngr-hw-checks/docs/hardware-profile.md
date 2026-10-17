# Hardware profile and check declarations

`ngr-hw-checks` is the harness of the real-hardware checks (design 13.5, ADR-275). This page
describes the hardware profile it reads, how a check declares what it needs, and how the two are
compared. The guarded VCI handle, the module match against the attached device and the report
writer are not part of the crate yet.

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
| `module` | The device name or serial number the library reports |
| `label`, `pname` | J2534 only, and required there, not empty: the label and the device name of the configured `modules` entry (ADR-281) |
| `firmware_version`, `library_version` | What the device reports, compared after opening |

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

- Names are compared as the libraries look them up, ignoring ASCII case and surrounding spaces.
- Two J2534 ends are on one module when they name the same library and the same `label` or the
  same `pname` (ADR-281), whatever their `module` says. Two D-PDU API ends are on one module
  when they name the same library and `module`.
- Two ends on one module must describe it identically and name different `channel`s, compared
  as the J2534 worker reads them: `CAN_CH1`, `can_ch1` and `CAN_CH01` are one channel.
- One device reached through both APIs, one at each end, looks like two modules here. Design
  13.5's procedure (only the profile's VCIs connected) and the module match against the attached
  device cover it.

## Check declarations

A check declares its requirements as a `CheckDecl` constant in `ngr_hw_checks::CHECKS`: its
test name, the classes it runs on and the operations it needs beyond reads (the operations
above, `PowerSwitching`, and `HardwareControl` with a control's name). A check runs only when
the profile's class is one of its classes and the profile allows each of its operations. The
`list` command and `ngr_hw_checks::listing` compare every declaration with the profile without
running anything, and `ngr_hw_checks::begin`, the first call of every `hw_` test, refuses the
check the same way before it reaches any device. A `hw_` test in any crate declares itself in
`CHECKS`, so the listing covers every check; a test that is not there is not listed.

"Permitted" means only that the profile's class and operations cover the declaration. What a
check then sends is limited by the guarded handle (design 13.5 "Guarded VCI access"): a T2 write
passes only inside an authorized job, and T3's other limits (no DTC clearing, resets, security
access or session beyond the extended one) are the class's lists, not declared operations.
