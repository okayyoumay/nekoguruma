# Release Scope

What a Nekoguruma release contains, what it leaves out, and which parts are the responsibility
of the framework user or the system operator. This is a summary: each line cites the design
section (`system-architecture.md`) or ADR that decides it, and those stay authoritative. The
grouping of the release artifacts and the place of the simulators are decided in ADR-506.

## Included

### Release artifacts

| Group | Contents | Source |
|---|---|---|
| Product | `ngr-server`, with the reference UI embedded and served when no UI directory is configured | design 9.6, 12 |
| Product | `ngr-agent`, with the Linux device-mode files in `packaging/` (systemd unit, sysusers entry) | `crates/agent/docs/ngr-agent.md` |
| Product | The worker services `j2534-0404-service` and `iso22900-service`, one binary per worker ABI | design 7.3, `worker-crates.md` |
| Local-profile bundle | PostgreSQL; RustFS pinned to one release, fetched and hash-checked by `scripts/fetch-rustfs.sh` | design 12, 13, ADR-491, ADR-492 |
| SDK | The UI framework package (npm) and the types generated from `api/openapi.yaml` and `api/asyncapi.yaml` | design 9.6, 9.8 |
| SDK | Schemas and templates (`schemas/`), the local validation CLI, IR dry runs | design 9.8 |
| SDK | The conformance test suite with the simulated VCI `sim-vci` and the simulated ECUs of `sim-ecu` it serves | design 9.8, 13.4, ADR-506 |

### Platforms

| Platform | Scope | Source |
|---|---|---|
| Windows | x64 and x86 workers, built as `*-pc-windows-gnullvm` | ADR-227 |
| Linux hosts | x86_64 and aarch64; workers for x86_64, i686, arm64 and armhf, on glibc 2.17 or later and kernel 3.2 or later | design 1 "Assumptions", 7.3, ADR-232 |
| Deployments | Local (loopback, OS service) and standard (container, OIDC, S3-compatible storage) | design 13, 14 |

### Vendor library standards

- SAE J2534-1 (DEC2004, v04.04), the API of clause 7, with the optional features of SAE J2534-2
  (DEC2020) clauses 5 to 26 as far as `j2534-2-support-plan.md` records their coverage
- ISO 22900-2 D-PDU API, clause 8 of the 2022 edition (clause 9 of the 2009 edition, which the
  worker crates also cite); library discovery follows the root description file of the 2022 edition's
  clause 8.7, the same chain as the 2009 edition's clause 9.7 (design 1 "Assumptions", 7.1, `worker-crates.md`)

## Not included

### Not shipped

| Item | Source |
|---|---|
| Vendor J2534 and D-PDU API libraries | design 16.2 "Licensing" |
| VCI profiles; they are installed outside this software | design 9.3, ADR-228 |
| The text of the standards the code cites (copyrighted; cited by clause only) | `CLAUDE.md` "Spec references and copyright" |
| ODX/PDX data supplied by OEMs or third parties; its redistribution depends on its licence | design 9.4, 16.2 |
| MinIO; the bundled object server is RustFS | ADR-491, `crates/server/docs/ngr-server.md` |
| The test doubles `iso22900-mock` and `j2534-0404-mock`; they serve the repository's own tests | ADR-506 |
| `sim-vci` in the product: the product installation does not register it as a VCI (it ships in the SDK only) | ADR-506 |

### Platforms out of scope

| Item | Source |
|---|---|
| The `*-pc-windows-msvc` targets | ADR-227 |
| musl-based Linux distributions | ADR-232 |
| Emulating another architecture on Linux | design 7.3 |
| Windows on ARM (on hold) | design 17 P2 |

### Functions out of scope

| Item | Source |
|---|---|
| J2534 v05.00 | `worker-crates.md` "Process Isolation" |
| D-Server (MCD-3D) | design 1 R14 |
| Acting as a J2534 library for a maker's application; TMC RP1210 | design 16.2 "Regulatory Scope", ADR-237 |
| Code extensions on the agent or the worker; extensions there are data only | design 2 principle 5, 9, 9.7 |
| Per-tenant UI bundle uploads | design 9.6 |
| Certification as a regulated inspection tool | design 16.1, 16.2, ADR-237 |

## Responsibility of the framework user or the system operator

| Item | Responsible | Source |
|---|---|---|
| Write and recovery procedures used in production; the bundled ones are a reference implementation | Framework user | design 16.1, ADR-229 |
| Business screens and operational flows; the bundled ones are a reference implementation | Framework user | design 9.6, 16.1 |
| Registering a product with makers' secure gateways | Framework user | design 16.1, ADR-237 |
| Certification, SERMI approval and authorisation, statutory retention periods, EU Data Act recipient duties | System operator | design 16.1, ADR-237 |
| The software update management process (SUMS) | System operator | design 16.1 |
| Correctness of extension package contents | Author | design 16.1 |

## Conditional or undecided

| Item | Condition | Source |
|---|---|---|
| OTX, ExVe compatible endpoints, generic OBD package | The standards are obtained | design 8.7, 16.2, ADR-237 |
| Tenant and contract management | Undecided | design 17 P6 |
| Non-functional targets | Undecided | design 17 P5 |
| State-changing requests from a release-build agent | Refused until the policy layer of design 5.5, 5.6, 6 and 8.9 replaces the rule; only a debug build sends them, and only to the simulators | ADR-247, ADR-331 |
