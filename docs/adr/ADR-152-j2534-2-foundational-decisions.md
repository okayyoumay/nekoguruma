# ADR-152: J2534-2 Support — Foundational Decisions (Phase 0)

**Date:** 2026-07-29
**Status:** Accepted, superseding ADR-017
**Affects:**
- `docs/adr/ADR-017-j2534-1-protocol-scope.md` (superseded)
- `docs/j2534-2-support-plan.md` (design questions §4.2/§4.3/§4.5/§4.6 resolved here)
- `j2534-0404-sys/src/bindings/j2534_v0404.h` and all 5 committed target bindings
- `j2534-0404/src/error.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`, `rpc_link.rs`, `resources.rs` (future phases)

## Context

`docs/j2534-2-support-plan.md` lays out a phased plan to bring SAE J2534-2
(DEC2020, "Optional Pass-Thru Features") into the `j2534-0404` subsystem,
which ADR-017 currently scopes to SAE J2534-1 (DEC2004, v04.04) only. That
plan's §4 lists six cross-cutting design questions this ADR must answer
before Phase 1 onward can proceed; §4.1 (feature-enablement plumbing) was
already resolved in the plan itself — the existing `pname` config field
(`vci_service_launcher::config::ModuleConfigEntry`, ADR-107) already
threads a `"J2534-2:<device>"` value straight into `PassThruOpen`'s `pName`,
so no new config schema or wrapper path is needed. §4.4 (whether ISO 22900-2
already defines D-PDU resource-ID/ComParam equivalents for each new J2534-2
protocol) is deliberately left open per-phase — it requires re-reading the
ISO 22900-2 spec clause-by-clause for each protocol family as that phase is
implemented, not something this ADR can resolve up front.

This ADR resolves the remaining four questions (§4.2, §4.3, §4.5, §4.6) and
records Phase 0's foundational, non-behavioral groundwork: adding every
J2534-2 constant this subsystem will eventually need to the FFI header in
one pass, and mapping the error codes J2534-2 genuinely introduces.

## Decision

### 1. Advertising policy (§4.2): config-gated static advertising, connect-time enforcement

Resource advertising stays static, driven by the existing `RESOURCE_TABLE`
(ADR-069), gated only by facts already knowable from config at startup: a
module whose `pname` does not carry the `"J2534-2:"` prefix (clause 5's
opt-in convention) never advertises J2534-2-derived resources; an opted-in
module advertises the full static set for whichever J2534-2 features this
service has actually implemented so far. Actual device capability is
enforced at `ConnectComLogicalLink` time — refined by a cached
`GET_DEVICE_INFO`/`GET_PROTOCOL_INFO` result once Phase 1 exists and the
device is open, and by the mapped native connect error otherwise. The
Discovery mechanism (Phase 1) is a connect-time enforcement input, never an
advertising gate.

The alternative — advertising only capabilities confirmed via Discovery —
is not just less convenient, it doesn't fit this service's architecture.
ISO 22900-2 §9.4.29.2 NOTE 1 permits `GetResourceIds` before `ModuleConnect`,
and this service already relies on that: `rpc_link.rs`'s resource resolution
answers for any in-range handle with no device open at all. Combined with
ADR-107's single-open-device model — a second configured module's device
cannot be probed while a different module's device is open — confirmed-only
advertising would make the advertised resource set depend on which module
happens to be open at query time, a stateful advertising surface with no
basis in the spec's resource-discovery model. The static `pname` gate costs
nothing and prevents the one advertising mistake that actually matters:
advertising J2534-2 resources on a module clause 5 says must be assumed
J2534-1-only.

**Consequence:** `docs/j2534-2-support-plan.md`'s Phase 15 (Analog Inputs)
does not need Phase 1 as a dependency after all — its earlier "conditionally
also 1" note is resolved by this Decision (updated in the plan document in
the same commit as this ADR). **Accepted residual:** a caller can see an
advertised resource whose connect subsequently fails on a less-capable
device (e.g. an `ANALOG_IN` channel index beyond what the attached device
actually has) — this surfaces as a mapped `PDU_ERR_*` at
`ConnectComLogicalLink`, refined by cached Discovery data where available,
not as an advertising-time rejection.

### 2. Device-scoped IOCTL routing (§4.3): existing `IoCtl` RPC, module-scoped; no new RPCs

`GET_DEVICE_CONFIG`/`SET_DEVICE_CONFIG` (Phase 14) — and any other
client-visible device-scoped J2534-2 command — become new curated
`PDU_IOCTL_*` command IDs on the existing `IoCtl` RPC (`rpc_misc.rs`),
dispatched as module-scoped ("M") commands exactly like the seven existing
module-scoped ADR-079 commands (`PDU_IOCTL_RESET`/`READ_VBATT`/
`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` etc.): resolved via
`require_module_handle_for_ioctl` to a `module_handle`, then to a `DeviceID`
via `require_connected_device_for`. `GetObjectId(OBJT_IO_CTRL, ...)` gains
the corresponding name-table entries (ADR-079's discovery convention).

`GET_DEVICE_INFO`/`GET_PROTOCOL_INFO` (Phase 1) are consumed **internally**
by this service — cached at device-open time to support Decision 1's
connect-time enforcement — rather than exposed as a new client-visible RPC.
Their D-PDU-facing projection, if any is needed, rides the existing
`GetModuleIds`/`GetResourceIds`/`GetVersion`/`GetStatus` surface.

`vci_service_interface::IoCtlRequest.handle` is already a `oneof` carrying
`CllHandle`/`ModuleHandle`/`SystemHandle` — the module-scoped path this
decision uses is not new plumbing, it is the same resolution every existing
M-command already goes through. A new RPC would fork the proto surface away
from the ISO 22900-2 object model it deliberately mirrors, and nothing in
that model defines a device-config-specific RPC distinct from `IoCtl`.

**Deferred:** whether to also expose Discovery results to clients as a
`PDU_IOCTL_*` pair (rather than purely internal use) is left to Phase 1's
own design work; the default until then is internal-only. The exact
`DataItem` payload shape for `GET_DEVICE_CONFIG`'s `SCONFIG_LIST`-style
response and Discovery's `SPARAM_LIST`-style response is also Phase 1/14
design work, not resolved here.

### 3. Conformance claims (§4.5)

This project claims conformance only to the individual SAE J2534-2 (DEC2020)
feature areas it has actually implemented, tracked phase-by-phase in
`docs/j2534-2-support-plan.md` §7 and each phase's own ADR(s); it makes no
blanket "J2534-2 compliant" claim, and per J2534-2 clause 5 no such blanket
claim is meaningful short of implementing all twenty feature areas plus full
clause-25 Discovery support. Once Phase 1 lands, every implemented feature's
support is reported truthfully through the clause-25 Discovery mechanism:
this adapter never reports a parameter as supported that it, or the attached
J2534 device, does not actually implement. Every per-feature claim is
additionally bounded by the attached device's own capabilities — "this
adapter implements feature X, subject to what the connected device
supports" — never a device-independent guarantee.

### 4. Mock fidelity strategy (§4.6): phase-by-phase, confirmed

`docs/j2534-2-support-plan.md`'s existing recommendation — extend
`j2534-0404-mock` alongside each phase rather than in one upfront rewrite —
is confirmed. Each phase's D-PDU mapping (§4.4) is unresolved until that
phase's own research, so a mock rewrite done ahead of that research would
simulate behavior likely to be reworked. Phase-by-phase mock work also
produces the "device without this J2534-2 feature" test fixture for free:
the mock's default rejection of not-yet-implemented J2534-2 IOCTLs/protocols
**is** the case Decision 1's connect-time enforcement needs test coverage
for.

**Addition to the plan:** starting Phase 1, the mock's rejection of
not-yet-implemented J2534-2 IDs should be a deliberate, spec-shaped response
(`ERR_INVALID_IOCTL_ID`/`ERR_INVALID_PROTOCOL_ID`, matching J2534-1's
existing error semantics) rather than an incidental fallthrough, so that
Decision 1's degradation path has a meaningful mock-backed test from Phase 1
onward.

### 5. Phase 0 foundational groundwork

- **All** J2534-2 constants (new `ProtocolID`s, IOCTL IDs, `SCONFIG`
  parameter IDs, the six genuinely-new error codes, and the new structs
  `REPEAT_MSG_SETUP`/`SBYTE_ARRAY`/`SPARAM_LIST`/`SPARAM`/
  `NDIS_ADAPTER_INFORMATION`) are added to `j2534-0404-sys/src/bindings/
  j2534_v0404.h` in this one pass, per `docs/j2534-2-support-plan.md`
  §2.3/§26's inventory, so later phases reference existing constants rather
  than repeatedly re-touching the FFI layer. Bindings are regenerated for
  all 5 committed target triples (`i686-pc-windows-gnullvm`,
  `x86_64-pc-windows-gnullvm`, `x86_64-pc-windows-msvc`,
  `x86_64-unknown-linux-gnu`, `armv5te-unknown-linux-gnueabi`). Adding a
  constant is not itself new behavior — nothing in the safe wrapper or
  service layers references these new symbols yet, so this phase changes no
  observable behavior.
- The six genuinely-new J2534-2 error codes (`ERR_PIN_IN_USE`,
  `ERR_VOLTAGE_IN_USE`, `ERR_ADDRESS_NOT_CLAIMED`,
  `ERR_NO_CONNECTION_ESTABLISHED`, `ERR_RESOURCE_IN_USE`,
  `ERR_INVALID_IOCTL_PARAM_ID` — `ERR_PIN_INVALID` is **not** new, it
  already exists in the v04.04 header and is already mapped) get a
  `j2534-0404/src/error.rs` `Display` mapping, following the existing
  per-code match-arm convention (ADR-026).

## Consequences

- `ADR-017`'s protocol-scope restriction is superseded: J2534-2 protocols
  are no longer categorically out of scope, but each protocol's actual
  implementation still lands one phase at a time per
  `docs/j2534-2-support-plan.md`, each phase gated by its own D-PDU-mapping
  research (§4.4) and, where warranted, its own ADR.
- `docs/j2534-2-support-plan.md`'s design questions §4.2, §4.3, §4.5, §4.6
  are marked resolved (pointing here) in the same commit as this ADR; §4.4
  remains open by design, tracked per-phase.
- No client-observable behavior changes in this phase: the new FFI constants
  and error-code mapping are inert until a later phase's protocol/IOCTL work
  references them.
- **Deferred, tracked in `docs/j2534-2-support-plan.md`:** the exact
  `DataItem`/proto payload shapes for `GET_DEVICE_CONFIG` (Phase 14) and any
  client-facing Discovery projection (Phase 1) are Phase 1/14 design work,
  not resolved by this ADR.
