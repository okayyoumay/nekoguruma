# ADR-177: SAE J2534-2 Analog Inputs (Phase 15)

**Date:** 2026-08-13
**Status:** Accepted (analog_sample_rate's request-field mechanism superseded by ADR-178 -- re-expressed as the CP_AnalogSampleRate ComParam, staged via SetComParam, resolved at ConnectComLogicalLink time; Consequences' accepted-residual bullet on the seven remaining clause 10.3.3.2 parameters' missing client-facing access path closed by [ADR-216](ADR-216-native-only-comparam-exposure-analog-uart-echo-byte.md); every other Decision item remains in force)
**Affects:** `vci-service-interface` (proto), `j2534-0404-service` (resource table, ComParam-adjacent
connect surface, RPC dispatch), `j2534-0404-mock`

## Context

SAE J2534-2 clause 10 adds 32 independent, read-only analog-acquisition ProtocolIDs,
`ANALOG_IN_1`..`ANALOG_IN_32` (native `0x0000C000`-`0x0000C01F`; Phase 0 already added these
constants, plus `DEVICE_INFO_ANALOG_IN_SUPPORTED`/`_SIMULTANEOUS`, to `j2534_v0404.h` and
regenerated bindings for all 5 targets — no bindgen work needed this phase). Each channel connects
independently; `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`/`PassThruStartMsgFilter` are all
rejected on it, and only `PassThruReadMsgs` works, returning device-queued 4-byte signed
little-endian millivolt readings. No pin-selection mechanics apply (clause 10's channels are
enumerated directly, not `_PS`-qualified), so this phase depends only on Phase 0.

Clause 10.3.3.2 defines five channel-scoped native `SET_CONFIG`/`GET_CONFIG` acquisition
parameters (`ACTIVE_CHANNELS`, `SAMPLE_RATE`, `SAMPLES_PER_READING`, `READINGS_PER_MSG`,
`AVERAGING_METHOD`) plus three read-only capability parameters (`SAMPLE_RESOLUTION`,
`INPUT_RANGE_LOW`, `INPUT_RANGE_HIGH`). None of these eight have any ISO 22900-2 ComParam
equivalent — they are pure native-only concepts, the same shape UART Echo Byte's `UEB_T*` timing
parameters had (Phase 9, ADR-170).

ISO 22900-2 (both the 2009 and 2022 editions, checked directly) defines no resource or ComParam
concept resembling multi-channel analog acquisition — `SET_PROG_VOLTAGE`/`READ_VBATT` are the
only voltage-adjacent concepts, and neither models a 32-channel acquisition resource. This
service's D-PDU resource table (`resources.rs`), shaped entirely by ISO 22900-2, therefore has no
anchor for this feature at all — the plan's own §6 table rates this phase `design-advisor: No` as
a coarse up-front assessment, but the actual client-facing resource-ID/proto shape this phase
introduces is a genuine, first-of-its-kind protocol-interpretation question with no cheaper-tier
precedent directly on point, so — unlike the "No" verdicts Phase 4/6/9/10/11 each resolved
directly — this one was escalated to a `design-advisor` consult per the cost-policy gate (a wrong
choice here is expensive to reverse: it is the client-facing resource surface, not an internal
implementation detail). This ADR records that consult's decision.

## Decision

**32 static `resources.rs` rows (`0x023D`-`0x025C`, the next free range after J1708's `0x023C`),
not an arithmetic `_CHx`-style index mapping.** One new shared `ChannelProtocol::AnalogIn` variant,
each row's `hw_protocol_override` pointing at its own native `PROTOCOL_ANALOG_IN_x`, empty
`dlc_pin_data`/pin table (clause 10 has no pin concept), and a new synthetic bus-type name with no
ISO 22900-2 source (documented as such, matching Phase 9/10/11's precedent of flagging an
editorial/non-spec-grounded naming choice explicitly rather than implying an ISO basis that
doesn't exist).

The `_CHx` Additional Channels machinery (Phase 2b, ADR-156) was rejected as the reuse target: it
is keyed on a *connectable base protocol* (`chx_protocol_id` computes an offset from a base id that
itself is directly connectable at `channel_index == 0`), because clause 7 defines `_CHx` as "N
additional ports of one already-existing protocol." Clause 10 has no such base — its 32 ProtocolIDs
are independent from the start, so reusing `_CHx` would force a mandatory-index behavioral fork
inside machinery whose existing index semantics (0 = base, nonzero = additional channel) don't
apply here. It would also collapse all 32 independent subsystems under one `ResourceId`, breaking
per-subsystem `GetResourceStatus` occupancy and name-based resolution (`"ANALOG_IN_17"` resolves
naturally only with a real per-channel row). The static-row precedent (SWCAN/FT-CAN, ADR-164/168:
N distinct native protocol ids -> N rows differing only by `hw_protocol_override`) needs no new
resource-model mechanism at all; growing the table from ~60 to ~92 rows is a non-cost against that.
A one-row-plus-new-mandatory-index-field alternative was also considered and rejected: it carries
every downside of the `_CHx` reuse option while additionally requiring a new proto field the
32-row approach avoids entirely.

**`SAMPLE_RATE` is exposed as a new, connect-time-required `ResourceData` proto field
(`analog_sample_rate: u32`, native clause-10.3.3.2.2 encoding); the other four acquisition
parameters are left at their native defaults, per Phase 9/ADR-170's deferral precedent — no
D-PDU ComParam vocabulary is minted this phase.** Checking each parameter's own default against
its governing sub-clause (not by analogy to ADR-170 alone) showed these do not deserve uniform
treatment: `SAMPLE_RATE` defaults to zero, and zero means the analog subsystem is disabled — a
pure ADR-170-style deferral would connect successfully and then read nothing, forever, which is a
broken feature, not merely an unconfigurable one. The other four are spec-guaranteed functional at
their defaults (`ACTIVE_CHANNELS` defaults to all-channels-active; `SAMPLES_PER_READING`/
`READINGS_PER_MSG` both default to 1, required-supported even without averaging hardware;
`AVERAGING_METHOD` defaults to a required-supported simple average), and the resulting worst-case
default message (32 channels x 1 reading) stays far under the spec's structure-size cap — so
ADR-170's "run at native defaults, defer minting ComParam vocabulary" reasoning applies to these
four cleanly. An adapter-chosen fixed rate instead of a client-supplied one was rejected: rate
choice governs queue-fill/overflow behavior and some adapters may reject specific rate values, so
it is inherently the client's call, not a value this service can safely default on the client's
behalf. `analog_sample_rate` is required (rejected as zero/unset) on an analog resource and invalid
on any non-analog one (mirroring `channel_index`'s existing exclusivity treatment), applied via
`SET_CONFIG` immediately after `PassThruConnect`; a device rejection (native
`ERR_INVALID_IOCTL_VALUE`) fails the connect.

**Correction (Codex review, PR #66): `analog_sample_rate` is a top-level
`CreateComLogicalLinkRequest` field (sibling to the `resource` oneof), not a `ResourceData`
field as originally decided above.** A `ResourceData` field is reachable only via the `RscData`
`resource` oneof variant — `GetResourceIds` returns a bare `resource_id`, and the normal
discovery flow feeds that straight into `CreateComLogicalLink(Resource::ResourceId(...))` with
no `RscData` involved at all, so the original placement meant a client following that flow could
never supply a rate, making every one of the 32 Analog Input resources unreachable through their
own discoverable ids. Only the undocumented `RscData`-wrapped route (which this phase's own
tests happened to exercise) could ever connect one. Moving the field to the request's top level
(the same place `cll_create_flag` already sits, alongside `resource`) fixes this: it now
applies uniformly regardless of which `resource` variant (`RscData`/`ResourceId`/`ResourceName`)
selects the target. The requirement/exclusivity behavior described above is unchanged, just
checked against the new top-level field instead of the old `ResourceData` one.

## Consequences

- Establishes the first D-PDU resource family with no ISO 22900-2 anchor at all — 32 rows and a
  bus-type name that are this service's own invention, documented as such rather than implied to
  have spec backing.
- Clients get full acquisition-rate control and per-subsystem discovery/status, at the cost of no
  mid-link rate change or pause/resume (changing `SAMPLE_RATE` requires a reconnect — clause
  10.3.3.2.2 itself rejects `SAMPLES_PER_READING`/`READINGS_PER_MSG` changes while a nonzero rate
  is active, so acquisition setup is connect-scoped by the spec's own design, not just this
  service's choice).
- Accepted residual: `ACTIVE_CHANNELS`/`SAMPLES_PER_READING`/`READINGS_PER_MSG`/`AVERAGING_METHOD`
  and the three read-only capability parameters remain unexposed to D-PDU clients this phase,
  tracked as a backlog item in `j2534-0404-service/docs/implementation-notes.md` (a future phase
  minting D-PDU ComParam vocabulary for native-only parameters — first raised as a deferral by
  ADR-170 — would need to cover both UART Echo Byte's and this phase's parameters together, not
  reinvent the mechanism per phase).
- Accepted residual, superseded in part by ADR-185: `DEVICE_INFO_ANALOG_IN_SUPPORTED` is now
  consulted at connect time (ADR-185 Stage 1's `resources::connect_discovery_check`).
  `_SIMULTANEOUS` remains unwired -- no ADR-185 Stage 1 or Stage 2 call site consults it -- tracked
  in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.
- Write/periodic-message/filter rejection on a connected analog channel is enforced adapter-side
  (early rejection at the service layer, not left to a native device-error passthrough) for a
  clearer client-facing error and to avoid depending on every possible adapter actually returning
  `ERR_NOT_SUPPORTED` correctly for these calls.
