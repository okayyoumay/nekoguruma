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
| `[[endpoint]]` | T0 only, exactly two: each names a `[endpoint.vci]` or a `socketcan` interface (such as `"can0"`), and a `[endpoint.link]` |
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
| `label`, `pname` | J2534 only, and required there: the label and the device name of the configured `modules` entry (ADR-281) |
| `firmware_version`, `library_version` | What the device reports, compared after opening |

`[link]`, the values an IR `Protocol` declaration carries:

| Key | Meaning |
|---|---|
| `protocol` | The protocol name, such as `"ISO15765"` |
| `baud_rate` | Bits per second |
| `pins` | The two connector pins, CAN high first |
| `request_id`, `response_id`, `functional_id` | The physical request and response identifiers, and optionally the functional one |
| `id_bits` | 11 or 29; every identifier must fit |
| `addressing` | `"normal"`, `"extended"` or `"mixed"`; the last two need an `address` byte |
| `padding` | The padding byte; absent for no padding |
| `[link.comparams]` | The transport ComParams by name (`CP_...`) |

`[[hardware-control]]`, by `kind`, each with a `name` that checks refer to:

| `kind` | Parameters |
|---|---|
| `programming-voltage` | `pin`, `millivolts` |
| `pin-switch` | `pins` |
| `ioctl` | `id`, and either its exact `payload` (bytes) or its safe `range` (`[low, high]`) |

## Check declarations

A check declares its requirements as a `CheckDecl` constant in `ngr_hw_checks::CHECKS`: its
test name, the classes it runs on and the operations it needs beyond reads (the operations
above, `PowerSwitching`, and `HardwareControl` with a control's name). A check runs only when
the profile's class is one of its classes and the profile allows each of its operations. The
`list` command and `ngr_hw_checks::listing` compare every declaration with the profile without
running anything, and `ngr_hw_checks::begin`, the first call of every `hw_` test, refuses the
check the same way before it reaches any device.
