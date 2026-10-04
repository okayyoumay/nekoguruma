# ADR-070: SAE_J1850 VPW/PWM Auto-Detect at ConnectComLogicalLink

**Date:** 2026-07-09
**Status:** Accepted
**Affects:**
- `j2534-0404-service/src/service/resources.rs`
- `j2534-0404-service/src/service/protocol.rs`
- `j2534-0404-service/src/service/comparam_defaults.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service.rs`
- `j2534-0404-mock/src/lib.rs`
- `j2534-0404-service/tests/grpc_mock/harness.rs`
- `j2534-0404-service/tests/grpc_mock/j1850_autodetect.rs` (new)
- `j2534-0404-service/tests/grpc_mock/resources.rs`

## Context

ADR-069 introduced the combined J1850 bus type
`SAE_J1850_VPW_and_SAE_J1850_PWM` (bus `0x0307`) and its two bus-agnostic
resources (`ISO_15031_5_on_SAE_J1850` / `SAE_J2190_on_SAE_J1850`), but
declared VPW-vs-PWM auto-detection "not implementable over this API": J2534-1
`PassThruConnect` takes exactly one concrete protocol ID, with no "auto"
value, so the choice appeared to have to be made once, statically, at
table-authoring time (fixed to `J1850VPW`).

A subsequent spec correction revisited this: the combined bus is renamed
`SAE_J1850` (dropping the never-released `SAE_J1850_VPW_and_SAE_J1850_PWM`
name outright, no legacy alias) and gains a third row — resource `0x021A`
(`SAE_J2190_on_SAE_J1850`) moves onto it from the VPW-only bus (`0x0306`),
joining `0x021C`/`0x021D`. This bus is now expected to auto-select VPW vs.
PWM at connect time rather than staying fixed to VPW.

The core constraint from ADR-069 is still real: `PassThruConnect` itself
cannot express "either protocol." A wrong-flavor connect does not fail —
opening a `J1850PWM` channel on a wire that is actually wired for VPW
succeeds at the J2534 level and simply never carries decoded traffic. Since
there is no connect-time signal, the *only* way to tell VPW from PWM is
whether a connection actually carries decodable responses — which requires
transmitting or listening on the bus itself, once per flavor, before
committing to the real connect.

## Decision

`ConnectComLogicalLink` runs a sequential active-probe before the existing
channel_key/shared-channel logic, but only for CLLs whose resource row has
bus_type_id `0x0307` (`ChannelProtocol::needs_j1850_autodetect`, true only
for `SAE_J2190_ON_SAE_J1850` / `ISO_15031_5_ON_SAE_J1850` — the fixed-flavor
`_VPW`/`_PWM`-qualified variants are unaffected). Detection is skipped once a
per-module cache (`J2534Service::j1850_bus_flavor`) already holds a
**conclusive** result (verification-pass amendment — see "Carrying the
result" below for what conclusive means and why an inconclusive one must not
be cached).

**Probe sequence** (`rpc_link.rs::probe_sae_j1850_flavor`), VPW first:

1. `PassThruConnect(J1850VPW, 10_400)`.
2. **Install a pass-all filter on the probe channel (P1 verification-pass
   fix)**, via the same `install_pass_all_filter` the real connect path uses
   (`connect_new_physical_channel`, below) — real J2534 adapters silently
   discard every RX frame until at least one filter is installed on the
   channel, and the probe channel is no exception. Without this step, a
   genuine OBD response on real hardware never reaches `PassThruReadMsgs`
   (step 4), the read always times out, and auto-detect always falls back
   to (or, before the cache-only-conclusive fix, permanently locked onto)
   `J1850VPW` regardless of the bus's actual wiring — this was invisible
   against the mock, whose RX queue delivery does not itself depend on a
   filter being installed. No explicit filter cleanup is needed:
   `PassThruDisconnect` in step 5/6 tears the channel, and every filter on
   it, down before the next candidate (or the real connect) opens its own
   channel. A filter-install failure is treated the same as a
   `PassThruConnect` failure below (logged, "no response" for this
   candidate).
3. For the OBD-active resource (`ISO_15031_5_ON_SAE_J1850`, `0x021C`/
   `0x021D`): transmit a standard OBD-II functional Mode 01 PID 00 request
   (`J1850_OBD_PROBE_VPW` = `68 6A F1 01 00` — format/priority `0x68`,
   functional target `0x6A`, tester source `0xF1`, then the request itself).
   For the J2190 resource (`SAE_J2190_ON_SAE_J1850`, `0x021A`): there is no
   universal SAE J2190 probe request, so this step is skipped — detection is
   passive-only for this resource, and most quiescent buses fall through to
   the VPW default below.
4. `PassThruReadMsgs` for up to the response window
   (`ComParamSet::j1850_autodetect_window_ms`: the Working `CP_P2Max` when
   present and nonzero, else `J1850_AUTODETECT_DEFAULT_WINDOW_MS` = 500 ms).
5. Any decoded RX within the window → that flavor wins outright, **and is
   conclusive** — a genuine response was observed, whether elicited by an
   active probe write or merely overheard passively.
6. No RX → `PassThruDisconnect`, repeat steps 1-5 with
   `PassThruConnect(J1850PWM, 41_600)` and `J1850_OBD_PROBE_PWM` =
   `61 6A F1 01 00` (format/priority `0x61`, otherwise identical).
7. Neither candidate conclusive → default to `J1850VPW` with a logged
   warning; the connect still proceeds (preserves ADR-069's original
   fixed-VPW behavior on a quiescent bus — this feature never turns a
   previously-succeeding connect into a failure). **This outcome is a
   fallback, not conclusive** (verification-pass amendment) — see "Carrying
   the result" below.

Any hardware error during the probe itself (a candidate's `PassThruConnect`
failing, the pass-all filter install failing, a probe write failing, or a
read timing out or returning empty) is treated as "no response" for that
candidate, logged, and does not fail the real connect that follows — a
malfunctioning probe only costs the same VPW-default fallback this bus used
before auto-detect existed. Likewise, a failure to even open the device for
the probe is a fallback, not a conclusive result.

**Carrying the result.** The winning native protocol ID is written into
`LogicalLinkState::hw_protocol_id` — the same field ADR-046 already uses to
let a CLL's actual hardware protocol diverge from
`ChannelProtocol::j2534_protocol_id()` (there, software-ISO-TP mode's raw CAN
channel; here, the auto-detected flavor). `j2534_protocol_id()` itself is
unchanged (`0x0152`/`0x0153` still map to `J1850VPW`); its doc comment now
notes this is only the *initial candidate* for these two values, subject to
override by this probe. On a PWM win, the CLL's Working `ComParamSet` is
also updated in place with the *flavor-dependent subset* of the bus-then-
protocol PWM preset (verification-pass amendment,
`comparam_defaults::sae_j1850_pwm_override_params`): the merged VPW preset
(what `CreateComLogicalLink` seeds Working with) and the merged PWM preset
(same bus-then-protocol layering, PWM-specific) are diffed key by key, and
only the keys whose value actually differs are merged over Working —
`DATA_RATE`/`NETWORK_LINE`/`PARAM_J1850_IFR_CTRL` from the bus preset always
differ and so always change; the protocol preset's header/priority fields
also differ (and change) for both bus-agnostic protocols —
`CP_FuncReqFormatPriorityType` is `0x61` PWM vs. `0x68` VPW for
`SAE_J2190_on_SAE_J1850` *and* `ISO_15031_5_on_SAE_J1850` (matching the
`J1850_OBD_PROBE_PWM`/`_VPW` probe constants), and
`CP_FuncRespFormatPriorityType`/`CP_PhysReqFormatPriorityType`/
`CP_PhysRespFormatPriorityType` differ the same way. (A P1 finding caught
`iso_15031_5_on_sae_j1850_pwm` carrying the VPW byte values verbatim — a
copy-paste bug from its VPW counterpart — which hid these four keys from the
diff entirely and left a PWM-detected `ISO_15031_5_on_SAE_J1850` link
sending VPW-formatted functional OBD requests that a real PWM ECU ignores;
see "PWM preset header bytes" below.) Restricting the merge to this diff
(rather than the full PWM preset, as originally implemented) means a
client's own `SetComParam` staged before `ConnectComLogicalLink` on any
flavor-*independent* key (identical in both presets, e.g. `CP_CyclicRespTimeout`,
`CP_TesterPresentMsg`) survives the override instead of being silently
clobbered back to the preset value right before the Active snapshot. This
means `DATA_RATE` (and the other flavor-dependent keys) are effectively
auto-managed on this bus regardless of client staging; a caller that needs a
specific flavor deterministically should use the fixed-flavor resource rows
(`0x0215`-`0x0219`, `0x021B`) instead, which are unaffected by this feature.

**PWM preset header bytes (P1 verification-pass fix).**
`comparam_defaults::iso_15031_5_on_sae_j1850_pwm` previously set
`CP_FuncReqFormatPriorityType`/`CP_FuncRespFormatPriorityType`/
`CP_PhysReqFormatPriorityType`/`CP_PhysRespFormatPriorityType` to the exact
same values as `iso_15031_5_on_sae_j1850_vpw` (`0x68`/`0x48`/`0x6C`/`0x2C`) —
a copy-paste bug, not a deliberate "these happen to be identical" case like
`PARAM_HEADER_FORMAT_J1850`/`CP_RequestAddrMode` genuinely are. This affected
the fixed-PWM resource `0x0215` (`ISO_15031_5_on_SAE_J1850_PWM`) directly —
it sent VPW-formatted functional requests despite being a fixed-PWM
resource, exactly the mistake this ADR's fixed-flavor rows exist to avoid —
and, via the diff described above, hid the divergence from the auto-detect
override for `ISO_15031_5_on_SAE_J1850` (`0x021C`/`0x021D`). Corrected to the
PWM byte values (`0x61`/`0x41`/`0xC4`/`0xC4`, matching
`sae_j2190_on_sae_j1850_pwm`'s own already-correct values — SAE J1850's
header/priority-byte encoding is a physical-layer artifact of VPW vs. PWM,
not an application-layer choice, so the two protocols share one byte scheme
per flavor). `0x0217` (`SAE_J2190_on_SAE_J1850_PWM`) was already correct and
needed no change.

**Conclusive vs. fallback, and why the cache only stores the former**
(verification-pass amendment). `probe_sae_j1850_flavor` returns a
`J1850ProbeOutcome` (`Conclusive(flavor)` / `Fallback(flavor)`), not a bare
flavor: caching an inconclusive fallback as if it were authoritative would
permanently deny a later CLL on the same module the chance to run its own
probe. Concretely: the J2190 resource's probe is passive-only (no universal
probe request exists), so on a bus that happens to be quiet during that
particular CLL's connect window, its probe is genuinely inconclusive and
falls back to `J1850VPW` — but the OBD-capable resource
(`ISO_15031_5_ON_SAE_J1850`) *can* actively probe the same bus and get a real
answer. If the J2190 CLL's fallback had been written to the cache, the
OBD-capable CLL would skip its own probe entirely (per the Decision
section's cache-hit rule) and inherit the unconfirmed `J1850VPW` guess,
never getting to actively confirm PWM. So: `autodetect_sae_j1850_flavor`
writes `hw_protocol_id` (and, on a PWM result, the ComParam diff above) for
*every* call regardless of conclusiveness — the current CLL always needs a
value to connect with — but only writes the module-wide cache when the
outcome is `Conclusive`. A `Fallback` leaves the cache empty, so the very
next `SAE_J1850` CLL through `ConnectComLogicalLink` runs
`probe_sae_j1850_flavor` again from scratch. Once any CLL's probe *is*
conclusive, the cached value is authoritative for the rest of the module's
lifetime — a later CLL never second-guesses a confirmed result.

The per-module cache is a new `Arc<Mutex<Option<u32>>>` field on
`J2534Service`, held across the probe's `.await` points (mirroring
`probe_can_channel_mode`'s `resolved_can_channel_mode` pattern from ADR-046)
so concurrent connects cannot both probe. Every call to the detection
entry point — whether it runs a fresh probe or hits the cache — still writes
`hw_protocol_id` and, on a PWM result, the ComParam diff, so a second CLL on
a different `SAE_J1850` resource is fully consistent with the first without
repeating the probe (once the cache actually holds a conclusive result).

**Mock support.** `j2534-0404-mock` gained a bus-flavor knob
(`__mock_set_j1850_bus_flavor`): `PROTOCOL_J1850VPW`/`PROTOCOL_J1850PWM`
makes the simulated bus "answer" (queue a canned response frame) only a
connect opened with that exact protocol id, immediately at connect time —
satisfying both the active probe (which reads after writing) and passive
listening (which never writes); unset/other values keep the bus silent, the
default after `__mock_reset`.

## Consequences

- Supersedes ADR-069's Decision-section text asserting VPW/PWM auto-detect
  was not implementable, and its matching Consequences bullet.
- One-time probe latency (up to two response windows) on the first
  `SAE_J1850` CLL per module; every later one reuses the cache.
- The J2190 resource's detection is passive-only and therefore often falls
  through to the VPW default on a quiescent bus — this is inherent to SAE
  J2190 defining no universal probe request, not a gap in this mechanism.
- `LogicalLinkState::hw_protocol_id` now has a second divergence reason
  (alongside ADR-046's software-ISO-TP mode): the auto-detected J1850
  flavor. Any future code branching on "is `hw_protocol_id` overridden"
  must account for both.
- **Only a conclusive probe is cached module-wide (verification-pass fix).**
  The initial implementation cached whatever `probe_sae_j1850_flavor`
  returned, including the inconclusive-fallback `J1850VPW` default — so once
  the J2190 resource's passive-only probe fell through to that default on a
  quiet bus, every later CLL (including an OBD-capable one that *could*
  actively probe and get a real answer) permanently inherited the unconfirmed
  guess instead of getting to probe itself. `probe_sae_j1850_flavor` now
  returns a `J1850ProbeOutcome` (`Conclusive`/`Fallback`) instead of a bare
  flavor, and only a `Conclusive` result is written to the cache; a
  `Fallback` still resolves the current CLL (nothing else to do — it needs
  *some* value to connect with) but leaves the cache empty for the next CLL
  to try its own probe. See the Decision section's "Conclusive vs. fallback"
  paragraph.
- **The PWM ComParam override is now restricted to the flavor-dependent key
  diff, not the full PWM preset (verification-pass fix).** The initial
  implementation unconditionally `extend`ed Working with every key the
  merged PWM preset carries, which silently overwrote a client's own
  `SetComParam` staged before `ConnectComLogicalLink` on any same-named key
  the PWM preset also happens to set — even one with an identical value in
  both the VPW and PWM presets (e.g. `CP_CyclicRespTimeout`, `0` either way).
  `sae_j1850_pwm_override_params` now diffs the merged VPW and PWM presets
  and returns only the keys that actually differ, so a flavor-independent
  client-staged value survives a PWM win; the flavor-dependent keys
  (`DATA_RATE` and friends, described above) still always win with the
  detected value, preserving the "auto-managed on this bus" documentation.
- **`iso_15031_5_on_sae_j1850_pwm` carried the VPW header/priority bytes
  verbatim, a copy-paste bug (P1 finding, verification-pass fix).**
  `CP_FuncReqFormatPriorityType`/`CP_FuncRespFormatPriorityType`/
  `CP_PhysReqFormatPriorityType`/`CP_PhysRespFormatPriorityType` were all set
  to the VPW byte values (`0x68`/`0x48`/`0x6C`/`0x2C`), identical to
  `iso_15031_5_on_sae_j1850_vpw` — so (a) the fixed-PWM resource `0x0215`
  sent VPW-formatted functional OBD requests despite never touching VPW at
  all, and (b) the diff-based PWM override above never surfaced these keys
  for the bus-agnostic `ISO_15031_5_on_SAE_J1850` (`0x021C`/`0x021D`), so a
  PWM-detected link kept sending the VPW format byte (`0x68`) a real PWM ECU
  ignores. Corrected to the PWM byte values (`0x61`/`0x41`/`0xC4`/`0xC4`),
  matching `sae_j2190_on_sae_j1850_pwm`'s already-correct values and the
  `J1850_OBD_PROBE_PWM` probe constant; `0x0217`
  (`SAE_J2190_on_SAE_J1850_PWM`) needed no change. See the Decision section's
  "PWM preset header bytes" paragraph.
- **The probe's temporary candidate channels now install a pass-all filter
  before reading (P1 finding, verification-pass fix).** Real J2534 adapters
  silently discard every RX frame until at least one filter is installed on
  a channel (the same reason `connect_new_physical_channel` installs one on
  the real channel); the probe channels never did, so on real hardware a
  genuine OBD response would never reach `PassThruReadMsgs` and the probe
  would always time out and fall back to VPW regardless of the bus's actual
  wiring. This was invisible against `j2534-0404-mock`, whose RX queue
  delivery is not itself gated on a filter being installed -- making the
  mock enforce that gating was assessed and rejected as too invasive (an
  in-crate mock unit test, `write_and_read_with_loopback`, deliberately
  reads back a loopback echo with no filter installed at all), so the fix
  is instead pinned via the filter-install call count
  (`tests/grpc_mock/j1850_autodetect.rs::probe_installs_a_pass_all_filter_on_each_candidate_channel`,
  which fails without the fix). See the Decision section's probe-sequence
  step 2.
- **A related P2 finding excluding the `SAE_J1850` auto-detect rows from
  `GetResourceIds`'s legacy `bus_type` hw-id fallback is documented in
  ADR-069** (this ADR's rows are what that fallback was leaking): a legacy
  bus alias resolving to `J1850VPW` (e.g. `"j1850_vpw"`) previously matched
  `0x021A`/`0x021C`/`0x021D` too, since their `ChannelProtocol`'s
  `j2534_protocol_id()` is only this ADR's VPW *initial probe candidate*,
  not a fixed connect protocol — see ADR-069's Decision-section paragraph on
  `legacy_bustype_hw_id`.
- **Detected flavor drives hardware-layer checks, not just the connect
  protocol** (PR-review finding after this ADR's initial landing): TX
  message size validation, outgoing message header construction, and
  `SetComParam`/`GetComParam` allowlisting must key off `hw_protocol_id`,
  not `protocol.j2534_protocol_id()` — the latter is permanently `J1850VPW`
  for the two `SAE_J1850` bus-agnostic protocols (the "initial candidate"),
  so before this fix a PWM-detected link was validated with VPW's TX size
  range (1..=4128) and ComParam allowlist instead of PWM's (3..=10;
  `CP_NetworkLine` PWM-only), silently accepting oversized messages and
  rejecting a valid `CP_NetworkLine` set. `resolve_send_recv_tx`/
  `resolve_tester_present` (`rpc_primitive.rs`) and `check_param_allowed`'s
  callers (`rpc_link.rs`) now resolve a `ChannelProtocol::from_raw(hw_protocol_id)`
  view for these checks instead. The one exception is software-ISO-TP mode
  (ADR-046): its ISO15765-family checks (the TX size range for the logical
  pre-segmentation message, and the ADR-055 functional-Single-Frame rule)
  deliberately keep using service-level `protocol`, since `hw_protocol_id`
  there is the raw CAN channel underneath, not a hardware flavor those
  particular checks should follow — this ADR's fix is scoped to divergences
  it and the pin-typing amendment introduce (J1850 VPW/PWM,
  `SAE_J2610_on_SAE_J2610_SCI`'s `hw_protocol_override`), not a re-litigation
  of ADR-046. `handle_update_param`'s (`events.rs`) `CoptUpdateparam`
  SET_CONFIG push had the same bug in a simpler form (reading
  `l.protocol.j2534_protocol_id()` instead of the already-correct
  `l.hw_protocol_id` field directly) and is fixed the same way. ComParam
  class/family predicates (`is_can_family`/`is_kwp_family`/`is_sci_family`)
  and `unique_id_params` were surveyed too but need no change: every
  divergence this service has today (CAN/ISO15765, J1850 VPW/PWM, the four
  SCI native ids) resolves to the same family either way, so switching
  their input has no observable effect -- called out for future reviewers
  rather than left as a silent "why didn't you fix this too."
- Resource identity, `GetResourceIds`, `GetResourceStatus`, and
  `GetConflictingResources` are unaffected: detection is a hardware-layer
  decision resolved at `ConnectComLogicalLink`, not a naming or resource-table
  concern. The renamed bus (`SAE_J1850`) and moved resource (`0x021A`) are
  ordinary table edits, orthogonal to the probe itself.
- K-line 5-baud-vs-fast-init selection remains unimplemented, consistent
  with ADR-017 — this ADR only addresses the J1850 bus.

See ADR-069 (introduced the combined bus and its original limitation text)
and ADR-046 (the `hw_protocol_id` divergence mechanism and the
`probe_can_channel_mode` per-module-cache-under-lock pattern this reuses).
