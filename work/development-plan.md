> **TEMPORARY WORKING MATERIAL.** This file is consumable: rewrite it as milestones finish or change, and delete it when it no longer describes the plan. Permanent files must not reference it. See `work/README.md`.

# Development plan

Milestones from the current skeleton to a deployable product. Each milestone has a goal, its scope (by design-document section) and exit criteria a reviewer can check. The individual work items live in `backlog.md` and `worker-crates-backlog.md`; this file only says which milestone they belong to. When a milestone finishes, delete its section, move any lasting decision into `docs/system-architecture.md` or an ADR, and name the next milestone in `backlog.md`'s Status section (that is what P1 means).

Order of work: the device side first (M1, M2), because interruption safety (R5) and the IR runtime are the riskiest parts and can be verified without a server. The server and UI follow on the local deployment profile (M3, M4), which needs no cloud. Features that depend on standards we do not hold yet (ODX/OTX, SOVD/ExVe) come last.

Standards: `vehicle-comm-specs` holds SAE J2534-1 (v04.04), SAE J2534-2 (DEC2020), ISO 22900-2 (2009 and 2022) and ISO 14229-1 (2026 edition). Clause numbers below for ISO 14229-1 refer to the 2026 edition. Standards marked "not held" still have to be obtained.

## Overview

| Milestone | Goal | Main crates | Standards needed |
|---|---|---|---|
| M1 Local E2E | agent -> worker -> `sim-vci` -> `sim-ecu` works on the worker targets, including an interrupted-write resume | `agent`, `worker-host`, `sim-vci`, `sim-ecu`, `diag-ir`, worker crates | J2534-1, J2534-2, ISO 22900-2, ISO 14229-1 (all held) |
| M2 Diagnostic runtime | Diagnostic procedures defined as data (proprietary CSV + JS format) run end to end | `diag-ir`, `diag-frontend`, worker L1 (ISO-TP, UDS), `agent` L2 | ISO 14229-1 (held); ISO 14229-2, ISO 15765-2 (not held) |
| M3 Server and job control (local profile) | A job created through the Web API is executed by the agent over the control channel and its result is stored | `server`, `agent`, `shared-proto`, `shared-crypto`, `db/` | none (RFC/IT specs only) |
| M4 Web UI and acquired data | Operators run jobs and view/edit acquired data in the browser; offline start works | UI framework, reference UI, `server` | none |
| M5 Real-time monitoring | Monitoring at the 10 ms target, interval capture, multiple subscribers | `agent`, worker, `server`, UI | none |
| M6 Trust, approval and reprogramming | Signed packages and artifacts, approval levels, ECU reprogramming with preconditions | `shared-crypto`, `vendor-manifest`, `agent`, `server` | ISO 14229-1 clause 16 (held); SAE J3138 (not held) |
| M7 Standard formats and external API | ODX/PDX and OTX ingestion; SOVD / ExVe compatible endpoints | `diag-frontend`, `server` | ISO 22901-1 (ODX), ISO 13209 (OTX), ISO 17978 (SOVD), ISO 20077/20078 (none held) |
| M8 Cloud deployment and field validation | Standard (cloud) deployment, scale measures for size S, validation with real VCIs | `server`, deployment, CI | UNECE R155/R156 as needed (free to obtain) |

M5 and M6 do not depend on each other; their order can be swapped. M7 can start as soon as the standards are obtained, in parallel with M5/M6.

## M1 Local E2E (current milestone)

**Goal.** The local end-to-end path works and is verified on the worker targets: agent -> worker service -> `sim-vci` -> `sim-ecu`, with the ABI interpretation table (design 7.1.2) checked on the six worker targets and the interrupted-write resume scenario (design 5.3 / 5.6) passing against the simulators.

**Scope.**

- `sim-ecu` as a UDS server per ISO 14229-1: the services an M1 write job and its resume exercise are DiagnosticSessionControl (9.2), ECUReset (9.3), SecurityAccess (9.4, with the security access state chart in Annex I), TesterPresent (9.7), ReadDataByIdentifier (10.2), WriteDataByIdentifier (10.7, the configuration-value write), ClearDiagnosticInformation and ReadDTCInformation (11.2, 11.3), RoutineControl (13.2) and RequestDownload / TransferData / RequestTransferExit (14.2, 14.4, 14.5). Server response rules (7.7: negative responses, suppressing positive responses, physical vs. functional addressing) and the negative response codes in Annex A.1 apply to all of them. The TransferData block sequence counter (14.4) lets `sim-ecu` accept a block repeated after a lost response without writing it twice; a transfer interrupted by a reset or a lost session restarts from RequestDownload (ADR-229).
- Simulators: session/flash state in `sim-ecu`; `sim-vci` delegating to `sim-ecu` with fault injection (delay, disconnect, crash, write failure; design 13.4); all functions the j2534-0404 service needs, with stdcall exports on Windows x86 and `long_size = 8` on Linux.
- Agent: worker launch through `worker-host`, bearer-token minting and the tonic client (design 7.4); Linux library resolution (design 7.1.1).
- Core logic: `diag-ir` instruction dispatch for the instructions a configuration-value write needs, VM state serialized with postcard (design 8.2.5); a minimal agent journal recording each write step (design 5.5 / 5.6). Journal encryption is M6.
- CI: the launch test (design 7.3, through `j2534-0404-service`) and ABI table check on the six targets; the resume scenario in the core test job.

**Exit criteria.**

1. A test in CI starts the agent, which launches the j2534-0404 worker against `sim-vci`, opens a channel, and reads a DID from `sim-ecu` (Linux x86_64 at least; the cross-built targets run the launch test).
2. The ABI table check passes on all six worker targets in CI (`scripts/abi-roundtrip.sh` or its successor).
3. A write job interrupted at each injected fault (power loss during transfer, worker crash, VCI disconnect) resumes from the journal or ends in a defined failure state, never in an undefined one (design 5.6 state diagram), with a test per fault.
4. No P1 item for M1 remains open in `backlog.md` or `worker-crates-backlog.md`.

**Risk.** ISO 14229-1 defines the services but not the session-layer timing (P2 / P2* and response-pending handling), which is in ISO 14229-2 (not held). `sim-ecu` uses fixed, configurable timing values in M1; they are checked against ISO 14229-2 in M2.

## M2 Diagnostic runtime

**Goal.** Vehicle knowledge supplied as data is turned into IR on the server side and executed by the agent, with format differences not leaking below L3 (design 8.1, 8.2).

**Scope.**

- Worker L1: ISO-TP (ISO 15765-2, not held) and UDS client behaviour on both the J2534 and D-PDU workers (design 8.1): session-layer timing and response pending from ISO 14229-2 (not held; existing worker ADRs already cite its timing clause), request/response handling per ISO 14229-1 clause 7.
- Agent L2 primitives (design 8.1) built on ISO 14229-1: read/write DID (10.2, 10.7) with the DID ranges in Annex C.1, read DTC with the status-mask bits and DTC formats in Annex D.2 / D.4, routine control (13.2, Annex F), security access (9.4). The 2026 edition also defines the Authentication service (9.6, certificate exchange and challenge-response) as an alternative to seed/key; L2 keeps the security step replaceable so it can be added in M6.
- `diag-ir`: declaration part in FlatBuffers with pre-expanded decode plans and COMPU-METHOD conversions (8.2.2); the full procedure-part instruction set (8.2.4); resume model (8.2.5); interruptibility attributes (8.10.1).
- `diag-frontend`: the proprietary format first, CSV tables + JavaScript subset -> IR (8.4), because it needs no purchased standard. Dry-run validation with vehicle access mocked (12.1).
- COMPARAM mapping on the J2534 side for the base protocols (8.5).

**Exit criteria.**

1. A CSV + JS definition of one ECU (variant identification, a few DIDs with conversions, DTC read, one routine) is converted to IR and runs against `sim-ecu` through both worker kinds (J2534 and D-PDU mock).
2. IR golden tests (definition -> IR -> request bytes and decoded values) and a fuzz target for malformed IR run in CI (12.1).
3. A procedure interrupted mid-run resumes at the IR level according to its section attributes (8.2.5, 8.10.1).

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

## M4 Web UI and acquired data

**Goal.** Operators use the system from an ordinary browser (R4): run jobs, view and edit acquired data, and start pre-fetched jobs offline.

**Scope.** UI framework and reference implementation (9.6); acquired data, record templates and annotations (4.3, 4.3.1, R21); concurrent editing and merge (R7); local-first sync (4.4, R6); PWA caching and the offline local connection (5.7, 5.7.1); confirmation levels for the job types (5.1, 6.3).

**Exit criteria.**

1. A browser end-to-end test (Playwright) runs an acquisition against the simulators, edits a record-template field, and sees the change from a second session.
2. With the server unreachable, a pre-fetched acquisition job starts from the cached UI over the local connection and syncs once the server is back.

## M5 Real-time monitoring

**Goal.** Monitoring per design 10: WebRTC direct path with server fallback, the 10 ms target measured at the 99th percentile, capture of any interval as acquired data (R18), multiple subscribers (R19).

**Exit criteria.**

1. A monitoring session against the simulators reports the measured achievable interval and the p99 arrival interval (10.3).
2. A captured interval is stored as acquired data with its metadata (4.6).
3. A second browser subscribes through the server path while the first uses the direct path (10.4).

## M6 Trust, approval and reprogramming

**Goal.** The trust model and the highest-risk job type are complete: three trust layers (11.1), extension packages and VCI profiles (9.3, 9.4), artifact ingestion (11.2), approval levels and two-person approval (6.3, 6.8), journal encryption and agent key protection (5.5, 16.3), ECU reprogramming with preconditions and safety guards (8.9, 8.9.1), seed/key and OEM authentication path (8.10).

**Scope from ISO 14229-1.** The reprogramming job follows the programming process in clause 16: the programming phases and their pre-programming, programming and post-programming steps (16.2, 16.3), with CommunicationControl (9.5) and ControlDTCSetting (9.8) in the pre-programming step and an ECU reset or return to the default session afterwards. Clause 16.4 requires that a server can still be reprogrammed after power loss, ground loss, a communication break or a voltage problem during programming; `sim-ecu`'s boot/application split and the agent's recovery path (design 5.6, 8.9.1) are tested against exactly those four faults. The Authentication service (9.6) and SecuredDataTransmission (15.2) are evaluated as options for the OEM authentication path (design 8.10).

**Exit criteria.**

1. Unsigned or wrongly signed packages, artifacts and job instructions are each rejected, with a test per layer.
2. An ECU reprogramming job runs the ISO 14229-1 clause 16 sequence against `sim-ecu`'s flash state machine, recovers from each of the four fault classes in clause 16.4 per design 5.6, and is refused when a precondition (8.9.1) is not met.
3. A two-person approval flow blocks reprogramming on an unattended device until the second approval.

## M7 Standard formats and external API

**Goal.** ODX/PDX and OTX as the primary vehicle-knowledge formats (R14), and SOVD / ExVe compatible external endpoints (R13, design 8.7). Starts when the standards are obtained.

**Exit criteria.**

1. A sample PDX converts to the same IR shape as the M2 proprietary-format definition for the same ECU, and runs against `sim-ecu`.
2. The SOVD resource mapping (L4) serves data reads for that ECU.

## M8 Cloud deployment and field validation

**Goal.** The standard deployment (design 14) with OIDC, S3-compatible storage and containers; the size-S measures that apply regardless of scale (15.2); validation with real VCIs against the per-vendor checklist (design 17, "Items to Confirm Early").

**Exit criteria.**

1. The server runs as a container with OIDC and S3-compatible storage, and an agent over the internet completes a job (14.4).
2. At least one real J2534 VCI and one D-PDU API VCI complete acquisition and a configuration write on a real ECU or bench.

## Decisions needed

These change the plan's scope or order; each is a design-17 item or a purchase.

- Standards purchase: ISO 14229-2 and ISO 15765-2 before M2; SAE J3138 before M6; ISO 22901-1 (ODX) and the other M7 standards before M7. ISO 14229-1 is held (2026 edition).
- Order of M5 and M6.
- Design 17 P3 (practical scope of Linux support) affects how much of M1's Linux path is kept as a supported product feature rather than a test path.
- Design 17 P5 (non-functional targets) before M8.
