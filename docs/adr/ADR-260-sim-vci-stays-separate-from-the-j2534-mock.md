# ADR-260: `sim-vci` Stays Separate from `j2534-0404-mock`

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `sim-vci`, `j2534-0404-mock` (crate boundaries), `crates/sim-vci/docs/simulated-vci.md`, `docs/worker-crates.md`

## Context

The workspace has two cdylibs that export the J2534 v04.04 PassThru API:

- `j2534-0404-mock` is the test double for `j2534-0404-service`'s own tests.
- `sim-vci` is the simulated VCI that the agent's end-to-end tests load, with a `sim-ecu`
  behind it (ADR-241).

Building `sim-vci` on the mock, or sharing parts of it, would remove one implementation of
the API. The question was whether to merge them, share parts, or keep them separate.

## Decision

1. **The two stay separate crates, with no shared code.** The reasons:
   - **`unsigned long` width.** The mock exports the `j2534-0404-sys` binding types, which are
     always 32-bit. `sim-vci` uses the native `c_ulong` on 64-bit Linux, as the counterpart of
     `NGR_J2534_LONG_SIZE=8`. Building on the mock would lose that, unless the whole mock were
     made generic over the width.
   - **Responses.** The mock answers itself, for example with a loopback echo or fixed
     per-protocol replies. `sim-vci` hands ISO-TP requests to `sim-ecu`. A merged crate would
     need a rule for which of the two answers a channel, and the mock's tests rely on its
     current behaviour.
   - **Test control.** The mock is driven per test through its `__mock_*` exports and a reset.
     `sim-vci` holds one process-wide ECU, driven by control commands.
   - **CI cost and dependency direction.** `sim-vci` is cross-checked for every worker target.
     The mock is built for host tests only. Merging would cross-build the whole mock, and would
     make the worker's test double depend on the vehicle simulator.
2. **Small shared pieces are not pursued on their own.** Constants could go through
   `j2534-defs`. The calling convention needs nothing shared, since each crate declares its
   exports in its own way. Sharing such pieces is left to a change that needs it for another
   reason.

## Consequences

- The PassThru API stays implemented twice. A change to its behaviour that both crates should
  follow has to be made in both.
- `crates/sim-vci/docs/simulated-vci.md` and the mock's row in `docs/worker-crates.md` state the
  separation and point here.
