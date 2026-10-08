# ADR-247: Debug Builds May Send Write Requests to `sim-vci`

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `agent` (`src/policy.rs`, `src/link.rs`, `src/host.rs`, `src/runner.rs`), `j2534-0404-service` (`tests/agent_end_to_end.rs`), ADR-235 item 8

## Context

ADR-235 item 8 limits the agent's runner to read-only requests: ReadDTCInformation and
ReadDataByIdentifier. Session changes, resets, writes, routines, security access and transfers
are all refused until the policy layer of design 5.5, 5.6, 6 and 8.9 exists. That layer needs:

- signed job instructions (design 11.1), planned for milestone M3;
- approval levels and the 8.9 execution preconditions, planned for milestone M6.

Milestone M1 runs the agent standalone (`ngr-agent run`, no server) against the simulators. One
of its exit criteria is an interrupted write that resumes against `sim-ecu` (design 5.3 and 5.6,
ADR-229). That needs a runner that can change the session, reset the ECU, run routines and
download. Neither the design nor an ADR says how such a job is authorized without a server, so
the M1 work on interrupted writes could not start.

The options were:
- allow writes in debug builds, and only against the simulator;
- bring a minimal form of the M3 signed-job check forward, with a local test key;
- an explicit command-line opt-in with a confirmation prompt, available in every build.

## Decision

1. **Debug builds, simulator only.** A job may send any UDS request (any service ID) only when
   both hold:
   - the agent is a debug build (`cfg(debug_assertions)`), as for the configuration override of
     ADR-073;
   - the worker's VCI identifies itself as `sim-vci`.

   Otherwise the read-only rule of ADR-235 item 8 applies unchanged. In a release build the
   simulator permission does not exist in the code, so no input can unlock it.
2. **How the simulator is identified.** After `ModuleConnect` and before it creates the logical
   link, a debug agent reads the module's version (`GetVersion`). The VCI counts as `sim-vci`
   only when both version strings carry `sim-vci`'s prefixes:
   - the firmware version starts with `NGR-SIM `;
   - the library version starts with `sim-vci `.

   Any other VCI, including one whose version cannot be read, leaves the job read-only. A
   release agent does not read the version.
3. **Same two checkpoints.** The checkpoints of ADR-235 item 8 stay. They now take the
   permission:
   - Before the link opens, the program is scanned against the most this build can allow:
     read-only in a release build, the simulator permission in a debug build. A release build
     therefore still refuses a writing program before it opens anything.
   - Once the link is open, the program is scanned again against the permission the VCI earned,
     before any instruction runs. A refusal closes the link; nothing has reached the ECU by then.
   - Every request is still checked in the host's `send_recv`, against the permission the link
     carries.
4. **What the simulator permission covers.** It covers `ServiceRequest` with any service ID and
   `RoutineControl`, which the host now sends as a RoutineControl request. A `ServiceRequest`
   may therefore carry a SecurityAccess or a download request too. The dedicated `SecurityAccess`
   and `FlashTransfer` instructions stay refused, because the host does not implement them yet.
5. **Replaced, not extended.** The policy layer of design 5.5, 5.6, 6 and 8.9 replaces this
   permission. A job is then allowed by its signed, approved instruction and its preconditions,
   not by the VCI it runs on. This ADR grants no permission against a real VCI in any build.

## Consequences

- An M1 procedure against the simulators can change sessions, reset the ECU, write identifiers
  and run routines. The interrupted-write work can build on this permission.
- A real VCI whose library reported `sim-vci`'s version prefixes would be treated as the
  simulator in a debug build. Debug builds are developer builds, and the prefixes are
  `sim-vci`'s own strings, so this needs a deliberately misleading library. Release builds are
  not affected.
- A debug agent makes one more call (`GetVersion`) per job before it creates the link.
- A signed-instruction scheme brought forward to M1 was rejected. It would fix the key handling
  and trust root (design 11.1) before the server that issues instructions exists. An opt-in flag
  available in release builds was rejected, because a mistaken flag could write to a vehicle.
