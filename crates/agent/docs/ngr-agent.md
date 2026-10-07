# `ngr-agent`

`ngr-agent` is the binary of the `agent` crate (design 3.3). The daemon around the job runner
(server connection, discovery, job intake) does not exist yet; the binary has one command that
runs a single IR program on a VCI, for local end-to-end runs and tests.

## `ngr-agent run`

```sh
ngr-agent run --vci <name> --program <file> [--workers <dir>] [--tx-id <hex>] [--rx-id <hex>]
```

| Option | Meaning |
|---|---|
| `--vci` | J2534 v04.04 library name, as the worker service resolves it (registry key on Windows, `library_path` entry in the service's `config.toml`) |
| `--program` | IR program file: a `diag_ir::Program` serialized as JSON |
| `--workers` | Directory of worker builds, laid out as `<dir>/<ABI name>/j2534-0404-service[.exe]` (design 7.3). Default: `workers` next to the `ngr-agent` executable |
| `--tx-id`, `--rx-id` | Physical request and response CAN IDs in hex, with or without `0x`. Default `7E0` / `7E8` |

The link is UDS on ISO 15765 at 500 kbit/s (`LinkConfig::iso15765`). Procedures do not declare
their link yet, so the CAN IDs come from the command line.

Steps (`agent::launch::launch_j2534_worker`, then `agent::run_program`):

1. Resolve the VCI name to its library path with `j2534_0404_registry::resolve_library_path`,
   the resolver the worker service uses on its side, so the header that selects the ABI belongs
   to the file the service loads. The agent does not know the worker's architecture before it
   reads that header, so only the api-level `library_path` entry and the registry apply; an
   architecture-specific `library_path` entry on Windows is seen by the worker only.
2. Detect the ABI from the library header (`worker_host::abi::detect_file`) and pick the
   matching service build with `WorkerLayout::find`. A missing build fails as
   `UNSUPPORTED_ABI`.
3. Launch the service with the library name and the ABI's default `long_size` (design 7.1.2;
   the Linux registration definition that could override it is not read yet), provision its
   auth key and connect the gRPC client (design 7.4).
4. Run the program under the read-only policy and the default job limits (ADR-235), then stop
   the worker.

## Output and exit status

| Exit status | Meaning | Output |
|---|---|---|
| 0 | The program ran to its end | The final `diag_ir::VmState` as one line of JSON on stdout. A negative response is a value on the stack, not a failure |
| 1 | Reading the program, launching the worker or the job failed | The reason on stderr |
| 2 | Invalid command line | The error and the usage on stderr |

The worker's own log goes to stderr.
