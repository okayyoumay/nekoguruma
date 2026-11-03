# ADR-415: The Hardware Profile Names the Worker ABI to Launch

**Date:** 2026-11-03
**Status:** Accepted
**Affects:** `crates/agent/src/launch.rs`, `crates/ngr-hw-checks` (`profile`, `open`), design 7.3 and 13.5

## Context

ADR-240 makes the agent try the worker builds in a fixed order and start the first one whose
resolved library is built for that build's ABI. On 64-bit Windows the order is x64 first, so a
VCI with a library in both registry views is always checked on the x64 build. A vendor's x86
library is then checked by the hardware harness (design 13.5, ADR-275) only on a host with no
x64 registration, and a check cannot choose which ABI it tests.

## Decision

1. `launch_worker` takes an optional ABI. Only the hardware harness passes one, from the
   hardware profile's optional `[vci] worker_abi` (an ABI table name, design 7.1.2). The agent
   passes none and keeps ADR-240's order unchanged; no agent code branches on the worker.
2. A named ABI restricts the resolution to the build for it, through the same resolution loop
   as ADR-240. On a host whose one build takes any ABI (Linux), the named ABI must equal the
   ABI detected from the library, or the launch is refused.
3. A named build is never replaced by another. An ABI the agent build cannot run fails with
   `LaunchError::AbiNotRunnable` before any lookup; a VCI with no library of that ABI fails with
   the resolution reasons of that build alone. A missing worker binary for it fails as in
   ADR-240.
4. A profile naming an ABI the agent build cannot run is refused when it is loaded
   (`agent::launch::host_runs`: on Windows the ABIs its registry views serve, on Linux the
   host table of design 7.3).
5. The report states the ABI that ran, named or resolved.

## Consequences

- Checking an x86 library on a 64-bit Windows host needs `worker_abi = "win-x86"` and a workers
  directory holding that build (`NGR_HW_WORKERS`); the harnesses install only the host-ABI one.
- A profile without `worker_abi` behaves as before.
