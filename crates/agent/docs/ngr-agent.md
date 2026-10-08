# `ngr-agent`

`ngr-agent` is the binary of the `agent` crate (design 3.3). It has one command, which runs a
single IR program on a VCI for local end-to-end runs and tests; it does not connect to the
server or take jobs from it.

## `ngr-agent run`

```sh
ngr-agent run --vci <name> --program <file> [--workers <dir>] [--tx-id <hex>] [--rx-id <hex>]
```

| Option | Meaning |
|---|---|
| `--vci` | J2534 v04.04 library name, as the worker service resolves it (registry key on Windows, `library_path` entry in the service's `config.toml`) |
| `--program` | IR program file: a `diag_ir::Program` serialized as JSON |
| `--workers` | Directory of worker builds, laid out as `<dir>/<ABI name>/j2534-0404-service[.exe]` (design 7.3). Default: `workers` next to the `ngr-agent` executable |
| `--tx-id`, `--rx-id` | Physical request and response CAN IDs in hex, with or without `0x`. Default `7E0` / `7E8`. Only 11-bit IDs: the link does not set the CAN ID format |

The link is UDS on ISO 15765 at 500 kbit/s (`LinkConfig::iso15765`), with the CAN IDs from the
command line rather than from the program.

Steps (`agent::check_program`, `agent::launch::launch_j2534_worker`, then `agent::run_program`):

1. Read the program and check it (`agent::check_program`): a program whose schema version or
   size this VM does not accept, one whose restart declaration `Program::validate` refuses
   (ADR-245), or one with a request this build may never send (a release build sends only
   read-only requests; ADR-235 item 8, ADR-247), fails here,
   before any worker starts. A bad operand, such as a missing constant, fails only when the VM
   reaches it, after the worker has started.
2. Resolve the VCI name the way each worker build would resolve it on its side
   (`j2534_0404_registry`): a `library_path` entry in `config.toml` under that build's
   architecture key, else at api level, else the build's registry view. The first build whose
   library header (`worker_host::abi::detect_file`) matches the build's own ABI is chosen, so
   the header that selects the build belongs to the file that build loads (ADR-240).
   - Windows x64 agent: the x64 build (`x86_x64` key, 64-bit registry view) first, then the x86
     build (`x86` key, 32-bit view, `KEY_WOW64_32KEY`). A VCI registered in both views runs on
     the x64 build.
   - Windows x86 agent: the x86 build only, so a 64-bit library is not found.
   - Linux: one lookup without an architecture key; the registry does not apply, so the
     library needs a `library_path` entry, and the build follows the header's ABI.
3. Pick the service build for that ABI with `WorkerLayout::find`. A missing build fails as
   `UNSUPPORTED_ABI`.
4. Launch the service with the library name and the ABI's default `long_size` (design 7.1.2;
   always the default, whatever a registration definition declares), provision its
   auth key and connect the gRPC client (design 7.4).
5. Run the program under the request policy and the default job limits (ADR-235), then stop
   the worker. The policy is read-only, except that a debug build may send any request once
   the worker's VCI has identified itself as `sim-vci` (ADR-247); a program that needs more
   than the VCI allows is refused after the link opens, before its first instruction runs.
   On that permission the `FlashTransfer` instruction also works (ADR-250); `SecurityAccess`
   stays refused.

## Data transfer

`FlashTransfer` sends one TransferData request (service 0x36) per instruction and requires a
positive response that echoes the block sequence counter; a negative response or a wrong echo
fails the job (ADR-250). The instruction's `block` operand is constant per instruction, so the
host ignores it and keeps its own count of the blocks of the transfer: the counter is 1 for the
first block after a RequestDownload (0x34) or RequestUpload (0x35) sent through the host
(a `ServiceRequest` answered positively), rises by one per accepted block and continues at 0
after 0xFF (ISO 14229-1:2026 clause 14.4). A `FlashTransfer` with no such request before it is
an error and sends nothing. `WorkerHost::transfer_block_index` gives the count of accepted
blocks, which the write-job journal records (ADR-244).

## Output and exit status

| Exit status | Meaning | Output |
|---|---|---|
| 0 | The program ran to its end | The final `diag_ir::VmState` as one line of JSON on stdout. A negative response is a value on the stack, not a failure |
| 1 | Reading the program, launching the worker or the job failed, or the final state holds a non-finite float (infinity or NaN), which JSON cannot represent | The reason on stderr |
| 2 | Invalid command line | The error and the usage on stderr |

The worker's own log goes to stderr.

## Configuration file

The agent and the worker read the same `config.toml` only if both binaries were built with the
same configuration root (`vci-service-config`'s `config-root-*` features). The default roots
agree. With `config-root-exe-dir`, the worker reads the file next to its own build under
`--workers`; with `config-root-win-program-files`, a 32-bit worker sees `Program Files (x86)`.
In both cases the agent can resolve a different library than the worker loads; build the
agent and the workers with the same root.

## Write-job journal

The `agent` library's `journal` module keeps the write-job journal (design 5.5, ADR-244): one
append-only file, `{job_id}.g{generation}.journal`, per job and ownership generation, in a
directory the caller passes. Every commit is synced before it returns. Only the job's writer opens
the journal for writing; other readers use `Journal::read`, which never changes the file. The journal only
records; committing an intent marker before the request it guards, and stopping the job when a
commit fails, are the job runner's duties. `ngr-agent run` keeps no journal; it sends read-only
requests, or any request to `sim-vci` in a debug build (ADR-247).

## Restart inputs

The `inputs` module supplies what an interrupted-write restart reads (design 8.2.5, 8.9;
ADR-229, ADR-245 item 2). `resolve_source` reads one `diag_ir::Source` and gives a `Reading`:
an integer, a text, `CannotBeEstablished` or `CannotBeDecoded`. The last two are readings, not
errors; a check treats them as failed. A failure to reach or use the worker (a transport
failure, a refused RPC or a request the policy refuses) stays a `HostError`, which fails the
check as well.

- Runtime inputs (`RuntimeInputs`, implemented by `WorkerHost`, which is also the sender, so
  `resolve_source(source, &table, &mut host)` takes one value): the supply voltage is read
  through the worker's IoCtl RPC. The host looks up the id of `PDU_IOCTL_READ_VBATT` by name
  (`GetObjectId`, IOCTL object type) on first use and keeps it. When the J2534 worker does not
  know the name, the VCI offers no voltage and the input reads "cannot be established"; a
  D-PDU worker reports an unknown name as an internal error, which stays a `HostError`.
  The worker interface has no defined source for external supply, ignition, engine running or
  vehicle speed: raw pin voltages and analog inputs carry no agreed meaning for them, and
  `sim-vci` simulates only the battery voltage (ADR-238). So they read "cannot be established". `FixedInputs` holds fixed readings for tests.
- Service fields: `ServiceSources` is a table from `(service_id, field_id)` to the request
  (SID first), the offset after the response SID, the length and the encoding (unsigned
  big-endian integer, or printable ASCII). It is a stand-in until the declaration part has a
  decoder (ADR-245, consequences). Until then a table accepts only single-identifier
  ReadDataByIdentifier entries (request `[0x22, hi, lo]`, offset 2): other services do not echo
  their request in a form the table can check, and a request for several identifiers lets the
  ECU leave some out. Building a table also refuses a duplicate id pair. The response must be
  exactly the positive SID, the echoed identifier and the field. A source missing from the
  table, a negative, short or long response, a wrong echo, or a field that does not fit its
  encoding reads as "cannot be decoded". ASCII text keeps surrounding spaces as they are; a
  field of only spaces, or with any byte outside 0x20 to 0x7E, cannot be decoded.
