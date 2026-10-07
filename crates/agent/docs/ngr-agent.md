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

1. Read the program and check it: a program the VM cannot run, or one with a request outside
   the read-only policy, fails here, before any worker starts.
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
5. Run the program under the read-only policy and the default job limits (ADR-235), then stop
   the worker.

## Output and exit status

| Exit status | Meaning | Output |
|---|---|---|
| 0 | The program ran to its end | The final `diag_ir::VmState` as one line of JSON on stdout. A negative response is a value on the stack, not a failure |
| 1 | Reading the program, launching the worker or the job failed | The reason on stderr |
| 2 | Invalid command line | The error and the usage on stderr |

The worker's own log goes to stderr.

## Configuration file

The agent and the worker read the same `config.toml` only if both binaries were built with the
same configuration root (`vci-service-config`'s `config-root-*` features). The default roots
agree. With `config-root-exe-dir`, the worker reads the file next to its own build under
`--workers`; with `config-root-win-program-files`, a 32-bit worker sees `Program Files (x86)`.
In both cases the agent can resolve a different library than the worker loads; build the
agent and the workers with the same root.
