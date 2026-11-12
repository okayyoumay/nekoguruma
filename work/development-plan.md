> **TEMPORARY WORKING MATERIAL.** This file is consumable: rewrite it as milestones finish or change, and delete it when it no longer describes the plan. Permanent files must not reference it. See `work/README.md`.

# Development plan

Milestones from the current skeleton to a deployable product. Each milestone has a goal, its scope (by design-document section) and exit criteria a reviewer can check. The individual work items live in `backlog.md` and `worker-crates-backlog.md`; this file only says which milestone they belong to. When a milestone finishes, delete its section, move any lasting decision into `docs/system-architecture.md` or an ADR, and name the next milestone in `backlog.md`'s Status section (that is what P1 means).

Order of work: the device side first (M1, finished 2026-10-14, then M2, finished 2026-11-11), because interruption safety (R5) and the IR runtime are the riskiest parts and can be verified without a server. The server and UI follow on the local deployment profile (M3, M4), which needs no cloud. Trust, approval and reprogramming (M6) come before real-time monitoring (M5), so the highest-risk job type continues directly from the M1/M2 interrupted-write work and signing is in place before M7 ingests extension packages. The ODX converter and the SOVD endpoints of M7 can be developed alongside M5/M6, since those standards are held, but the server does not ingest ODX-derived IR as extension packages (9.4) until M6's package signing (11.1) exists; the OTX and ExVe parts come last, once ISO 13209 and ISO 20077/20078 are obtained.

Standards: `vehicle-comm-specs` holds SAE J2534-1 (v04.04), SAE J2534-2 (DEC2020), ISO 22900-2 (2009 and 2022), ISO 14229-1 (2026 edition), ISO 14229-2 (2021 edition), ISO 15765-2 (2024 edition), ISO 22901-1 (2008 edition) and ISO 17978-1/-2/-3 (2026 edition). Clause numbers below for ISO 14229-1 refer to the 2026 edition. Standards marked "not held" still have to be obtained.

## Overview

| Milestone | Goal | Main crates | Standards needed |
|---|---|---|---|
| M3 Server and job control (local profile) | A job created through the Web API is executed by the agent over the control channel and its result is stored | `server`, `agent`, `shared-proto`, `shared-crypto`, `db/` | none (RFC/IT specs only) |
| M4 Web UI and acquired data | Operators run jobs and view/edit acquired data in the browser; offline start works | UI framework, reference UI, `server` | none |
| M6 Trust, approval and reprogramming | Signed packages and artifacts, approval levels, ECU reprogramming with preconditions | `shared-crypto`, `vendor-manifest`, `agent`, `server` | ISO 14229-1 clause 16 (held); SAE J3138 (not held) |
| M5 Real-time monitoring | Monitoring at the 10 ms target, interval capture, multiple subscribers | `agent`, worker, `server`, UI | none |
| M7 Standard formats and external API | ODX/PDX and OTX ingestion; SOVD / ExVe compatible endpoints; generic OBD package | `diag-frontend`, `server`; for generic OBD also `agent` (L2), worker L1 (functional requests), `diag-ir` and `sim-ecu` | ISO 22901-1 (ODX), ISO 17978 (SOVD) (held); ISO 13209 (OTX), ISO 20077/20078, SAE J1979 / J1979-2, ISO 15031-5, ISO 15765-4, ISO 27145 (not held) |
| M8 Cloud deployment and field validation | Standard (cloud) deployment, scale measures for size S, validation with real VCIs | `server`, deployment, CI | UNECE R155/R156 as needed (free to obtain) |

Each milestone also implements real-hardware checks (design 13.5, ADR-275) by target class (T0 loopback, T1 stand-in ECU, T2 bench ECU, T3 vehicle) for what it adds (maintainer's decision, 2026-10-15). Implementing them is part of the milestone's scope and exit criteria: each check is written, the harness lists it for a profile of its class, and the parts that need no hardware (profile parsing, the guard, reports, listing) are tested in CI. Running them on real hardware is optional and never an exit criterion, except in M8. A milestone takes only the checks it can implement with the infrastructure it already builds; a check that needs more waits for the milestone that builds it.

M5 and M6 do not depend on each other; M6 comes first (maintainer's decision, 2026-10-06). M7's ODX converter and SOVD endpoints can be developed in parallel with M5/M6; ingesting ODX-derived IR as extension packages waits on M6's package signing, and the OTX and ExVe parts wait on those standards.

## M3 Server and job control (local deployment profile, current milestone)

**Goal.** The server and agent work together on the local deployment profile (design 13): a job created through the Web API reaches the agent, runs, and its result and events are stored.

**Scope.**

- `server`: axum, PostgreSQL via sqlx with the `db/migrations` schema, local users (13.3), job queue with `FOR UPDATE SKIP LOCKED` (12), object storage trait with the local-file implementation.
- Control channel WSS and data channel HTTPS with resumable chunks (5.2); idempotent commands, start deadlines, sequence-numbered outbox and reconnection reconciliation (5.3).
- Agent identity, installation and initial registration (6.1, 6.4, 6.7); job instruction signing and verification in `shared-crypto` (11.1).
- `api/openapi.yaml` and `api/asyncapi.yaml` kept in sync with the implementation.
- The agent's communication log (8.11) uploaded only by explicit operator action, with the scope shown, over the data channel, and kept on the server with the job's result under its retention (8.11, 16.2, ADR-455; the agent deletes nothing locally before the server acknowledges the upload).

**Exit criteria.**

1. In CI, a local-profile server and an agent with the simulators run an acquisition job and a configuration write job end to end, created through the Web API, on a server whose database holds its seed data (roles, the default confirmation level, presets) and gives each per-vehicle lock an expiry derived from the job kind's expected duration (the seed data and lock expiry added at the maintainer's request, 2026-11-12)
2. A network fault test (disconnect, slow link; design 13.4) shows no duplicate execution, no automatic reissue of a write job, and complete event delivery after reconnection (5.3). A restart of an interrupted job runs only the program and documents its instruction names, and is refused before anything is sent when they differ (added at the maintainer's request, 2026-11-12).
3. An agent rejects a job instruction with a bad signature, and one whose operator is not the owner the agent is registered to where the connection restriction requires it (design 6.6). It also refuses a listener on its server's port that is not its own server, and the unsigned commands such a listener sends (cancel, HMI response, seed-key response; design 14.4, 11.1) (the operator and listener checks added at the maintainer's request, 2026-11-12).
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.
5. The fictional test vehicle ("Mikeneko", a gateway and six ECUs) is the vehicle of the end to end: its ECU definitions and its vehicle definition convert, and a configuration write job created through the Web API changes the engine controller's coding on the simulators, saving the current value, writing, reading it back, and writing the saved value back when the read-back check fails (added at the maintainer's request, 2026-11-12).
6. An operator's explicit upload sends a job's communication log over the data channel, showing its scope first; the server stores it with the job's result under its retention, and the agent deletes nothing locally before the server acknowledges the upload (design 8.11, 16.2, ADR-455) (added at the maintainer's request, 2026-11-12).

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- The stand-in ECU for T1: `sim-ecu` served over a Linux SocketCAN interface, testable without hardware on a virtual CAN interface (`vcan`).
- T1 (carried over from M1 and M2): the local end-to-end read of M1 with a real J2534 VCI against the stand-in ECU, and M2's session timing measured the same way, to separate the VCI's own delays from the ECU's.
- T1: an acquisition created through the Web API runs on a real VCI on the local profile.
- Writes through a real VCI wait for M6: the agent lets only the simulated VCIs, `sim-vci` and `iso22900-mock`, receive state-changing requests until the authorization layer replaces that rule (ADR-247, ADR-331).
- T2: the same acquisition against a bench ECU (read only; T2 writes wait for M6, design 13.5).

## M4 Web UI and acquired data

**Goal.** Operators use the system from an ordinary browser (R4): run jobs, view and edit acquired data, and start pre-fetched jobs offline.

**Scope.** Viewing a job's communication log (8.11) in the browser (decided by the maintainer, 2026-11-11). UI framework and reference implementation (9.6); acquired data, record templates and annotations (4.3, 4.3.1, R21); concurrent editing and merge (R7); local-first sync (4.4, R6); PWA caching and the offline local connection (5.7, 5.7.1); confirmation levels for the job types (5.1, 6.3).

**Exit criteria.**

1. A browser end-to-end test (Playwright) runs an acquisition against the simulators, edits a record-template field, and sees the change from a second session. The same test opens the job's communication log in the browser (8.11) (the log view added at the maintainer's request, 2026-11-12).
2. With the server unreachable, a pre-fetched acquisition job starts from the cached UI over the local connection and syncs once the server is back.
3. From the browser, an operator runs an actuator test on the fictional test vehicle's engine controller (an InputOutputControlByIdentifier item, ISO 14229-1 (2026) 12.2, and an actuator routine) at the confirmation level design 5.1 and 6.3 give actuator tests, and every IO control the job started is back under the ECU's control after the job ends, after it is cancelled, and after its agent is killed and restarted (added at the maintainer's request, 2026-11-12).
4. Maintenance records carry a retention derived from an operator setting per tenant and record kind (4.3, 16.2), and deleting an expired maintenance record leaves the write-job audit records of the same vehicle in place (added at the maintainer's request, 2026-11-12).
5. When several variants match an ECU, the operator chooses one in the browser by the attributes its definition gives each variant (ADR-327 item 7) (added at the maintainer's request, 2026-11-12).

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- None new. The browser and the offline local connection change nothing between the agent and the VCI, so M3's acquisition checks cover the hardware side of this milestone.

## M6 Trust, approval and reprogramming

**Goal.** The trust model and the highest-risk job type are complete: three trust layers (11.1), extension packages and VCI profiles (9.3, 9.4), artifact ingestion (11.2), approval levels and two-person approval (6.3, 6.8), journal encryption and agent key protection (5.5, 16.3), ECU reprogramming with preconditions and safety guards (8.9, 8.9.1), seed/key and OEM authentication path (8.10).

**Scope from ISO 14229-1.** The reprogramming job follows the programming process in clause 16: the programming phases and their pre-programming, programming and post-programming steps (16.2, 16.3), with CommunicationControl (9.5) and ControlDTCSetting (9.8) in the pre-programming step and an ECU reset or return to the default session afterwards. Clause 16.4 requires that a server can still be reprogrammed after power loss, ground loss, a communication break or a voltage problem during programming; `sim-ecu`'s boot/application split and the agent's recovery path (design 5.6, 8.9.1) are tested against exactly those four faults. The Authentication service (9.6) and SecuredDataTransmission (15.2) are evaluated as options for the OEM authentication path (design 8.10).

**Exit criteria.**

1. Unsigned or wrongly signed packages, artifacts and job instructions are each rejected, with a test per layer.
2. An ECU reprogramming job runs the ISO 14229-1 clause 16 sequence against `sim-ecu`'s flash state machine, recovers from each of the four fault classes in clause 16.4 per design 5.6, and is refused when a precondition (8.9.1) is not met. The sequence includes a flash session with several memory regions and a memory check against a digest the host computes from the artifact (the regions and the digest added at the maintainer's request, 2026-11-12)).
3. A two-person approval flow blocks reprogramming on an unattended device until the second approval.
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.
5. On the fictional test vehicle, a reprogramming job and a configuration change reach ECUs behind its gateway: the gateway is unlocked first, the engine controller is reprogrammed, and a body controller coding change is checked against the gateway's vehicle configuration record and written back on a mismatch (added at the maintainer's request, 2026-11-12).
6. The write-job journal, the communication log beside it and the local cache are encrypted with a key in the OS credential store and are tamper-evident, so an edited resume count is detected; their bodies are erased after job and sync completion and on agent-key revocation (5.5). The agent key is kept in an OS key store on every supported OS, Linux hosts with systemd older than 250 included (16.3) (added at the maintainer's request, 2026-11-12).
7. A signed VCI profile (9.3) changes what the agent judges: its timing margins and drift bound, and whether a D-PDU library forwards every pending, with a test per field (added at the maintainer's request, 2026-11-12).
8. A configuration write unlocks `sim-ecu` both through seed/key and through an OEM authentication provider whose challenge the server shows to the operator in the browser while the job waits for the grant (8.10) (added at the maintainer's request, 2026-11-12).
9. The server ingests a signed definition package (ECU definitions in CSV + Starlark, and vehicle definitions; 9.4), and refuses within bounded time and memory a package past its size limits or built to cost more than they allow (8.4) (added at the maintainer's request, 2026-11-12).

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T1: the clause 16 reprogramming sequence with a real VCI against the stand-in ECU, with power cut through the host-controlled switch during the transfer.
- T1: on the stand-in ECU serving the fictional test vehicle, an IO control a job holds is returned to the ECU when the VCI is unplugged and when supply power is cut through the host-controlled switch, and a configuration change interrupted by a power cut is detected and written back after the restart.
- T1 (carried over from M1 and M3): the interrupted write of M1 and a configuration write created through the Web API, with a real J2534 VCI against the stand-in ECU, and the VCI-disconnect fault produced by unplugging the VCI when the check asks for it, instead of by `sim-vci`.
- The rest of the guard (design 13.5): on T1, a hardware control passes when the profile lists it with its complete parameters and the request matches them; on T2, state-changing requests and listed hardware controls pass only from an authorized job, checked against what the job declares.
- T2: a configuration write and reprogramming of an ECU the profile declares expendable, once this milestone's approval levels exist, and only after the 8.9.1 checks pass.
- T2: the configuration write above unlocks the bench ECU through the real seed/key or OEM authentication path (8.10), where an algorithm or account is available.

## M5 Real-time monitoring

**Goal.** Monitoring per design 10: WebRTC direct path with server fallback, the 10 ms target measured at the 99th percentile, capture of any interval as acquired data (R18), multiple subscribers (R19).

**Exit criteria.**

1. A monitoring session against the simulators, built from the monitor sets of a CSV + Starlark definition (10.2; the definition source added at the maintainer's request, 2026-11-12)), reports the measured achievable interval and the p99 arrival interval (10.3).
2. A captured interval is stored as acquired data with its metadata (4.6).
3. A second browser subscribes through the server path while the first uses the direct path (10.4).
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T0/T2: the achievable monitoring interval and p99 arrival interval of each VCI at hand, recorded as the VCI profile's minimum monitoring period (9.3).
- T3: a monitoring session on a vehicle.

## M7 Standard formats and external API

**Goal.** ODX/PDX and OTX as the primary vehicle-knowledge formats (R14), and SOVD / ExVe compatible external endpoints (R13, design 8.7). The ODX converter and SOVD endpoints can start now; ingesting ODX-derived IR as extension packages waits on M6's package signing (11.1); OTX and ExVe wait on ISO 13209 and ISO 20077/20078. Generic OBD (SAE J1979, J1979-2, WWH-OBD) ships as a standard vehicle-knowledge package once those standards are held (ADR-237).

**Exit criteria.**

1. A sample PDX converts to the same IR shape as the M2 proprietary-format definition for the same ECU, and runs against `sim-ecu`; a flash description in a PDX converts to a flash session with its restart declaration (the flash description added at the maintainer's request, 2026-11-12)).
2. The SOVD resource mapping (L4) serves data reads for that ECU.
3. The generic OBD package reads the supported data identifiers (PIDs for classic SAE J1979, DIDs for J1979-2 and ISO 27145), current data and stored DTCs against `sim-ecu`, for each generic OBD family whose standard is held.
4. The hardware checks listed below are implemented: the harness lists each one for a profile of its class, a check run without `NGR_HW_PROFILE` fails, and their parts that need no hardware are tested in CI (design 13.5). Running them on hardware is optional.
5. For each of OTX (ISO 13209) and ExVe (ISO 20077/20078) whose standard is held: an OTX procedure converts and runs against `sim-ecu`, and the ExVe compatible endpoints serve that ECU's data (added at the maintainer's request, 2026-11-12).

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T3: the generic OBD package reads supported identifiers, current data and stored DTCs on a vehicle, for each family whose standard is held.
- T2: a PDX for the bench ECU, where one is available, converts and runs.

## M8 Cloud deployment and field validation

**Goal.** The standard deployment (design 14) with OIDC, S3-compatible storage and containers; the size-S measures that apply regardless of scale (15.2); validation with real VCIs against the per-vendor checklist (design 17, "Items to Confirm Early").

**Scope.** Besides the deployment and the validation above: the communication log's attribution of late items across exchanges (8.11 `late_item`, `late_response`, Settlement) is refined against what real VCIs deliver (decided by the maintainer, 2026-11-11). Its invariants are written down and checked by generated-sequence tests, its placement paths are unified under those tests, and the coarse cases are refined only where the real-VCI runs show they leave exchanges `undetermined` often enough to matter.

**Exit criteria.**

1. The server runs as a container with OIDC and S3-compatible storage, and an agent over the internet completes a job (14.4).
2. At least one real J2534 VCI and one D-PDU API VCI complete acquisition and a configuration write on a real ECU or bench.
3. The agent runs as a Windows service and as a systemd service, and writes its operational log to the Windows event log and to journald (6.2, 6.5, 12.2) (added at the maintainer's request, 2026-11-12).
4. The communication log's invariants for late items are written down and checked by generated-sequence tests, with the refinement of the scope above done where the real-VCI runs call for it (8.11) (added at the maintainer's request, 2026-11-12).
5. The design 17 P5 non-functional targets are decided, and the size-S measures (15.2) are measured against them (added at the maintainer's request, 2026-11-12).

This is the one milestone whose exit criteria require real hardware. It runs the hardware checks implemented in earlier milestones (design 13.5) at T2, and answers the design 17 per-vendor items for each VCI.

**Hardware checks to implement (design 13.5; running them on hardware is optional).**

- T0: whether each VCI's library opens and passes loopback frames when the agent runs as a service (design 6.2, 6.5; design 17, "Items to Confirm Early"). It needs the agent's service modes, which this milestone's deployment uses.

## Decisions needed

These change the plan's scope or order; each is a design-17 item or a purchase.


- Standards purchase: SAE J3138 before M6; ISO 13209 (OTX) and ISO 20077/20078 (ExVe) before M7's OTX and ExVe parts start; SAE J1979, J1979-2, ISO 15031-5, ISO 15765-4 and ISO 27145 before M7's generic OBD package. The M2 standards, ISO 22901-1 (ODX) and ISO 17978 (SOVD) are held.
- Design 17 P5 (non-functional targets) before M8.
- Hardware purchase for the optional real-hardware checks (design 13.5): the minimum set is one VCI meeting both the J2534 and the D-PDU API requirements of design 13.5's device table, a SocketCAN USB-CAN adapter, a diagnostic-connector breakout with terminators, and a laboratory power supply (T0 and T1). A bench ECU (T2) and access to a vehicle (T3) add the later checks. Until hardware exists the checks are written but not run.
