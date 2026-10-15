> **TEMPORARY WORKING MATERIAL.** This file is consumable: rewrite it as milestones finish or change, and delete it when it no longer describes the plan. Permanent files must not reference it. See `work/README.md`.

# Development plan

Milestones from the current skeleton to a deployable product. Each milestone has a goal, its scope (by design-document section) and exit criteria a reviewer can check. The individual work items live in `backlog.md` and `worker-crates-backlog.md`; this file only says which milestone they belong to. When a milestone finishes, delete its section, move any lasting decision into `docs/system-architecture.md` or an ADR, and name the next milestone in `backlog.md`'s Status section (that is what P1 means).

Order of work: the device side first (M1, finished 2026-10-14, then M2), because interruption safety (R5) and the IR runtime are the riskiest parts and can be verified without a server. The server and UI follow on the local deployment profile (M3, M4), which needs no cloud. Trust, approval and reprogramming (M6) come before real-time monitoring (M5), so the highest-risk job type continues directly from the M1/M2 interrupted-write work and signing is in place before M7 ingests extension packages. The ODX converter and the SOVD endpoints of M7 can be developed alongside M5/M6, since those standards are held, but the server does not ingest ODX-derived IR as extension packages (9.4) until M6's package signing (11.1) exists; the OTX and ExVe parts come last, once ISO 13209 and ISO 20077/20078 are obtained.

Standards: `vehicle-comm-specs` holds SAE J2534-1 (v04.04), SAE J2534-2 (DEC2020), ISO 22900-2 (2009 and 2022), ISO 14229-1 (2026 edition), ISO 14229-2 (2021 edition), ISO 15765-2 (2024 edition), ISO 22901-1 (2008 edition) and ISO 17978-1/-2/-3 (2026 edition). Clause numbers below for ISO 14229-1 refer to the 2026 edition. Standards marked "not held" still have to be obtained.

## Overview

| Milestone | Goal | Main crates | Standards needed |
|---|---|---|---|
| M2 Diagnostic runtime | Diagnostic procedures defined as data (proprietary CSV + Starlark format) run end to end | `diag-ir`, `diag-frontend`, worker L1 (ISO-TP, UDS), `agent` L2 | ISO 14229-1, ISO 14229-2, ISO 15765-2 (all held) |
| M3 Server and job control (local profile) | A job created through the Web API is executed by the agent over the control channel and its result is stored | `server`, `agent`, `shared-proto`, `shared-crypto`, `db/` | none (RFC/IT specs only) |
| M4 Web UI and acquired data | Operators run jobs and view/edit acquired data in the browser; offline start works | UI framework, reference UI, `server` | none |
| M6 Trust, approval and reprogramming | Signed packages and artifacts, approval levels, ECU reprogramming with preconditions | `shared-crypto`, `vendor-manifest`, `agent`, `server` | ISO 14229-1 clause 16 (held); SAE J3138 (not held) |
| M5 Real-time monitoring | Monitoring at the 10 ms target, interval capture, multiple subscribers | `agent`, worker, `server`, UI | none |
| M7 Standard formats and external API | ODX/PDX and OTX ingestion; SOVD / ExVe compatible endpoints; generic OBD package | `diag-frontend`, `server`; for generic OBD also `agent` (L2), worker L1 (functional requests), `diag-ir` and `sim-ecu` | ISO 22901-1 (ODX), ISO 17978 (SOVD) (held); ISO 13209 (OTX), ISO 20077/20078, SAE J1979 / J1979-2, ISO 15031-5, ISO 15765-4, ISO 27145 (not held) |
| M8 Cloud deployment and field validation | Standard (cloud) deployment, scale measures for size S, validation with real VCIs | `server`, deployment, CI | UNECE R155/R156 as needed (free to obtain) |

Each milestone also implements real-hardware checks (design 13.5, ADR-275) by target class (T0 loopback, T1 stand-in ECU, T2 bench ECU, T3 vehicle) for what it adds (maintainer's decision, 2026-10-15). Implementing them is part of the milestone's scope and exit criteria: each check is written, the harness lists it for a profile of its class, and the parts that need no hardware (profile parsing, the guard, reports, listing) are tested in CI. Running them on real hardware is optional and never an exit criterion, except in M8. A milestone takes only the checks it can implement with the infrastructure it already builds; a check that needs more waits for the milestone that builds it.

M5 and M6 do not depend on each other; M6 comes first (maintainer's decision, 2026-10-06). M7's ODX converter and SOVD endpoints can be developed in parallel with M5/M6; ingesting ODX-derived IR as extension packages waits on M6's package signing, and the OTX and ExVe parts wait on those standards.

## M2 Diagnostic runtime (current milestone)

**Goal.** Vehicle knowledge supplied as data is turned into IR on the server side and executed by the agent, with format differences not leaking below L3 (design 8.1, 8.2).

**Scope.**

- Worker L1: ISO-TP (ISO 15765-2) and UDS client behaviour on both the J2534 and D-PDU workers (design 8.1): session-layer timing and response pending from ISO 14229-2 (ADR-018 and ADR-102 still cite the wrong clause for it), request/response handling per ISO 14229-1 clause 7. `sim-ecu`'s P2 and P2* are still fixed constants from M1; here they become configurable and are checked against ISO 14229-2.
- Agent L2 primitives (design 8.1) built on ISO 14229-1: read/write DID (10.2, 10.7) with the DID ranges in Annex C.1, read DTC with the status-mask bits and DTC formats in Annex D.2 / D.4, routine control (13.2, Annex F), security access (9.4). The 2026 edition also defines the Authentication service (9.6, certificate exchange and challenge-response) as an alternative to seed/key; L2 keeps the security step replaceable so it can be added in M6.
- `diag-ir`: declaration part in FlatBuffers with pre-expanded decode plans and COMPU-METHOD conversions (8.2.2); the full procedure-part instruction set (8.2.4); resume model (8.2.5); interruptibility attributes (8.10.1).
- `diag-frontend`: the proprietary format first, CSV tables + Starlark subset -> IR (8.4), because it needs no purchased standard. Dry-run validation with vehicle access mocked (12.1).
- COMPARAM mapping on the J2534 side for the base protocols (8.5).

**Exit criteria.**

1. A CSV + Starlark definition of one ECU (variant identification, a few DIDs with conversions, DTC read, one routine) is converted to IR and runs against `sim-ecu` through both worker kinds (J2534 and D-PDU mock).
2. IR golden tests (definition -> IR -> request bytes and decoded values) and a fuzz target for malformed IR run in CI (12.1).
3. A procedure interrupted mid-run resumes at the IR level according to its section attributes (8.2.5, 8.10.1).
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- The harness the checks run in: the hardware profile named by `NGR_HW_PROFILE` and its parser, the static check declarations and the listing of the checks a profile permits, the `hardware` nextest profile, the guarded VCI handle with its cleanup (including the seam in the agent's job runner that hands it a guarded worker client) for read-only checks, refusing every state-changing request and hardware control, and the report writer.
- T0: for each OS and worker ABI the vendor ships a J2534 library for (Windows x86_64 and i686, Linux x86_64 and i686, and Linux ARM where offered), the library is found the way that OS finds it (the Windows registry; the registration definition in `/etc/nekoguruma/j2534/` on Linux, design 7.1, 7.1.1), opens, connects two CAN endpoints and passes loopback frames. On Linux this also checks the ABI and `unsigned long` width of design 7.1.2.
- T2/T3: the read parts of exit criterion 1's CSV + Starlark definition (variant identification, DIDs, DTC read), with the definition for the bench ECU or vehicle named by the profile, run through a J2534 VCI and through a D-PDU API VCI (the first real use of the D-PDU worker). The routine waits for M6, when section 6's controls for it exist (design 13.5).
- T2: the session timing (P2 / P2*) the bench ECU reports and the response times measured through a real VCI, compared with the values `sim-ecu` uses (ISO 14229-2 (2021) 9.2).

## M3 Server and job control (local deployment profile)

**Goal.** The server and agent work together on the local deployment profile (design 13): a job created through the Web API reaches the agent, runs, and its result and events are stored.

**Scope.**

- `server`: axum, PostgreSQL via sqlx with the `db/migrations` schema, local users (13.3), job queue with `FOR UPDATE SKIP LOCKED` (12), object storage trait with the local-file implementation.
- Control channel WSS and data channel HTTPS with resumable chunks (5.2); idempotent commands, start deadlines, sequence-numbered outbox and reconnection reconciliation (5.3).
- Agent identity, installation and initial registration (6.1, 6.4, 6.7); job instruction signing and verification in `shared-crypto` (11.1).
- `api/openapi.yaml` and `api/asyncapi.yaml` kept in sync with the implementation.

**Exit criteria.**

1. In CI, a local-profile server and an agent with the simulators run an acquisition job and a configuration write job end to end, created through the Web API.
2. A network fault test (disconnect, slow link; design 13.4) shows no duplicate execution, no automatic reissue of a write job, and complete event delivery after reconnection (5.3).
3. An agent rejects a job instruction with a bad signature.
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- The stand-in ECU for T1: `sim-ecu` served over a Linux SocketCAN interface, testable without hardware on a virtual CAN interface (`vcan`).
- T1 (carried over from M1 and M2): the local end-to-end read of M1 with a real J2534 VCI against the stand-in ECU, and M2's session timing measured the same way, to separate the VCI's own delays from the ECU's.
- T1: an acquisition created through the Web API runs on a real VCI on the local profile.
- Writes through a real VCI wait for M6: the agent lets only `sim-vci` receive state-changing requests until the authorization layer replaces that rule (ADR-247).
- T2: the same acquisition against a bench ECU (read only; T2 writes wait for M6, design 13.5).

## M4 Web UI and acquired data

**Goal.** Operators use the system from an ordinary browser (R4): run jobs, view and edit acquired data, and start pre-fetched jobs offline.

**Scope.** UI framework and reference implementation (9.6); acquired data, record templates and annotations (4.3, 4.3.1, R21); concurrent editing and merge (R7); local-first sync (4.4, R6); PWA caching and the offline local connection (5.7, 5.7.1); confirmation levels for the job types (5.1, 6.3).

**Exit criteria.**

1. A browser end-to-end test (Playwright) runs an acquisition against the simulators, edits a record-template field, and sees the change from a second session.
2. With the server unreachable, a pre-fetched acquisition job starts from the cached UI over the local connection and syncs once the server is back.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- None new. The browser and the offline local connection change nothing between the agent and the VCI, so M3's acquisition checks cover the hardware side of this milestone.

## M6 Trust, approval and reprogramming

**Goal.** The trust model and the highest-risk job type are complete: three trust layers (11.1), extension packages and VCI profiles (9.3, 9.4), artifact ingestion (11.2), approval levels and two-person approval (6.3, 6.8), journal encryption and agent key protection (5.5, 16.3), ECU reprogramming with preconditions and safety guards (8.9, 8.9.1), seed/key and OEM authentication path (8.10).

**Scope from ISO 14229-1.** The reprogramming job follows the programming process in clause 16: the programming phases and their pre-programming, programming and post-programming steps (16.2, 16.3), with CommunicationControl (9.5) and ControlDTCSetting (9.8) in the pre-programming step and an ECU reset or return to the default session afterwards. Clause 16.4 requires that a server can still be reprogrammed after power loss, ground loss, a communication break or a voltage problem during programming; `sim-ecu`'s boot/application split and the agent's recovery path (design 5.6, 8.9.1) are tested against exactly those four faults. The Authentication service (9.6) and SecuredDataTransmission (15.2) are evaluated as options for the OEM authentication path (design 8.10).

**Exit criteria.**

1. Unsigned or wrongly signed packages, artifacts and job instructions are each rejected, with a test per layer.
2. An ECU reprogramming job runs the ISO 14229-1 clause 16 sequence against `sim-ecu`'s flash state machine, recovers from each of the four fault classes in clause 16.4 per design 5.6, and is refused when a precondition (8.9.1) is not met.
3. A two-person approval flow blocks reprogramming on an unattended device until the second approval.
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T1: the clause 16 reprogramming sequence with a real VCI against the stand-in ECU, with power cut through the host-controlled switch during the transfer.
- T1 (carried over from M1 and M3): the interrupted write of M1 and a configuration write created through the Web API, with a real J2534 VCI against the stand-in ECU, and the VCI-disconnect fault produced by unplugging the VCI when the check asks for it, instead of by `sim-vci`.
- The guard's authorized-job path: on T1 and T2, state-changing requests and listed hardware controls pass only from an authorized job, checked against what the job declares (design 13.5).
- T2: a configuration write and reprogramming of an ECU the profile declares expendable, once this milestone's approval levels exist, and only after the 8.9.1 checks pass.
- T2: the configuration write above unlocks the bench ECU through the real seed/key or OEM authentication path (8.10), where an algorithm or account is available.

## M5 Real-time monitoring

**Goal.** Monitoring per design 10: WebRTC direct path with server fallback, the 10 ms target measured at the 99th percentile, capture of any interval as acquired data (R18), multiple subscribers (R19).

**Exit criteria.**

1. A monitoring session against the simulators reports the measured achievable interval and the p99 arrival interval (10.3).
2. A captured interval is stored as acquired data with its metadata (4.6).
3. A second browser subscribes through the server path while the first uses the direct path (10.4).
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T0/T2: the achievable monitoring interval and p99 arrival interval of each VCI at hand, recorded as the VCI profile's minimum monitoring period (9.3).
- T3: a monitoring session on a vehicle.

## M7 Standard formats and external API

**Goal.** ODX/PDX and OTX as the primary vehicle-knowledge formats (R14), and SOVD / ExVe compatible external endpoints (R13, design 8.7). The ODX converter and SOVD endpoints can start now; ingesting ODX-derived IR as extension packages waits on M6's package signing (11.1); OTX and ExVe wait on ISO 13209 and ISO 20077/20078. Generic OBD (SAE J1979, J1979-2, WWH-OBD) ships as a standard vehicle-knowledge package once those standards are held (ADR-237).

**Exit criteria.**

1. A sample PDX converts to the same IR shape as the M2 proprietary-format definition for the same ECU, and runs against `sim-ecu`.
2. The SOVD resource mapping (L4) serves data reads for that ECU.
3. The generic OBD package reads the supported data identifiers (PIDs for classic SAE J1979, DIDs for J1979-2 and ISO 27145), current data and stored DTCs against `sim-ecu`, for each generic OBD family whose standard is held.
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T3: the generic OBD package reads supported identifiers, current data and stored DTCs on a vehicle, for each family whose standard is held.
- T2: a PDX for the bench ECU, where one is available, converts and runs.

## M8 Cloud deployment and field validation

**Goal.** The standard deployment (design 14) with OIDC, S3-compatible storage and containers; the size-S measures that apply regardless of scale (15.2); validation with real VCIs against the per-vendor checklist (design 17, "Items to Confirm Early").

**Exit criteria.**

1. The server runs as a container with OIDC and S3-compatible storage, and an agent over the internet completes a job (14.4).
2. At least one real J2534 VCI and one D-PDU API VCI complete acquisition and a configuration write on a real ECU or bench.

This is the one milestone whose exit criteria require real hardware. It runs the hardware checks implemented in earlier milestones (design 13.5) at T2, and answers the design 17 per-vendor items for each VCI.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T0: whether each VCI's library opens and passes loopback frames when the agent runs as a service (design 6.2, 6.5; design 17, "Items to Confirm Early"). It needs the agent's service modes, which this milestone's deployment uses.

## Decisions needed

These change the plan's scope or order; each is a design-17 item or a purchase.

- Standards purchase: SAE J3138 before M6; ISO 13209 (OTX) and ISO 20077/20078 (ExVe) before M7's OTX and ExVe parts start; SAE J1979, J1979-2, ISO 15031-5, ISO 15765-4 and ISO 27145 before M7's generic OBD package. The M2 standards, ISO 22901-1 (ODX) and ISO 17978 (SOVD) are held.
- Design 17 P5 (non-functional targets) before M8.
- Hardware purchase for the optional real-hardware checks (design 13.5): the minimum set is one VCI meeting both the J2534 and the D-PDU API requirements of design 13.5's device table, a SocketCAN USB-CAN adapter, a diagnostic-connector breakout with terminators, and a laboratory power supply (T0 and T1). A bench ECU (T2) and access to a vehicle (T3) add the later checks. Until hardware exists the checks are written but not run.
