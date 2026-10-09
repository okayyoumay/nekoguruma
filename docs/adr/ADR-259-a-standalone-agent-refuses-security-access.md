# ADR-259: A Standalone Agent Refuses the SecurityAccess Instruction

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/policy.rs`, `src/host.rs`, `docs/ngr-agent.md`), ADR-247 item 4

## Context

The IR has a dedicated `SecurityAccess` instruction. The VM gets the seed, and the host must
supply the key. Design 8.10 keeps seed-key algorithms off the device: the agent sends the seed
to the server, and the server (or an HSM) returns the key, so operations that need a key are
online only. The M1 agent runs without a server, so it has no key source. `WorkerHost` answers
the instruction with `HostError::Unsupported`, and the policy refuses it for every permission
before the link opens. ADR-247 item 4 gave the reason as "the host does not implement it yet",
which read as a gap to close rather than a decision.

The alternatives for a standalone agent were a key source on the device: a key algorithm
library loaded by the agent, or keys configured per ECU. Both put on the device what design
8.10 keeps off it.

## Decision

1. **A standalone agent refuses the `SecurityAccess` instruction.** The policy refuses it for
   every permission, the simulator's included, before the link opens
   (`HostError::NotAllowed(0x27)` from `check_program`). The host keeps answering it with
   `HostError::Unsupported`, as a second line. No key source is added to the device.
2. **The key comes from the server once there is one.** The instruction is implemented together
   with the server's key service (design 8.10), not before.
3. **Test procedures keep using `ServiceRequest` 0x27 on the simulator.** A debug build on
   `sim-vci` may send any request (ADR-247 item 4), and `sim-ecu`'s seed-key rule is part of the
   simulator. This grants nothing against a real VCI.

## Consequences

- ADR-247 item 4's reason for refusing the dedicated instruction ("not implemented yet") is
  replaced by this decision; the refusal itself is unchanged.
- An M1 procedure that needs security access cannot run against a real ECU. That matches design
  8.10, which makes such operations online only.
- The existing tests stay the evidence: the policy test that refuses the instruction on the
  simulator permission, and the CLI test that refuses it before a worker starts.
