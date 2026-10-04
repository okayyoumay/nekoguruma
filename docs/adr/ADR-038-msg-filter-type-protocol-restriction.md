# ADR-038: Message Filter Type Restricted by Protocol (ISO15765 vs. Others)

**Date:** 2026-07-02
**Status:** Superseded by ADR-039 (for the ISO15765 `FLOW_CONTROL_FILTER` mask/pattern/flow-control
values only — the per-protocol filter-*type* selection decided here still stands)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`install_pass_all_filter`),
             `j2534-0404-service/src/service/rpc_misc.rs` (`rpc_io_ctl`, CLEAR_MSG_FILTERS branch)

## Context

ADR-005 established a pass-all hardware filter strategy: install a zero-mask
`PASS_FILTER` on every newly opened channel, and — for ISO15765 channels only
— *additionally* install a zero-mask `FLOW_CONTROL_FILTER`. ADR-008 mirrors
the same pair when re-installing filters after `CLEAR_MSG_FILTERS`.

Per the SAE J2534 v04.04 spec, `PassThruStartMsgFilter`'s `FilterType` is
constrained by protocol:

- On an **ISO15765** channel, only `FLOW_CONTROL_FILTER` is a valid filter
  type.
- On a **non-ISO15765** channel, only `PASS_FILTER` and `BLOCK_FILTER` are
  valid filter types.

`PASS_FILTER`/`BLOCK_FILTER` and `FLOW_CONTROL_FILTER` are mutually exclusive
per protocol — an ISO15765 channel must never also receive a `PASS_FILTER`.
The previous implementation installed a `PASS_FILTER` unconditionally
(including on ISO15765 channels) and treated the ISO15765
`FLOW_CONTROL_FILTER` install failure as non-fatal, which masked the
spec violation: a spec-conformant J2534 DLL would reject the `PASS_FILTER`
call on an ISO15765 channel with an invalid-filter-type error. The bundled
mock (`j2534-0404-mock`) does not validate filter type against protocol, so
this was not caught by the existing test suite.

## Decision

`install_pass_all_filter` (`rpc_link.rs`) and the `CLEAR_MSG_FILTERS`
re-install logic (`rpc_misc.rs`) now install **exactly one** filter type
per channel, selected by protocol:

- **ISO15765**: a single zero-mask/pattern/flow-control-frame
  `FLOW_CONTROL_FILTER`. Since this is now the *only* filter installed on the
  channel (not a supplement to `PASS_FILTER`), a failure to install it is
  treated as fatal — propagated as an error in `rpc_link.rs`, and logged as a
  `warn!` (consistent with ADR-008's best-effort recovery posture) in
  `rpc_misc.rs`.
- **All other protocols**: a single zero-mask/pattern `PASS_FILTER`, as
  before.

`BLOCK_FILTER` remains unused by the service; the spec permits it for
non-ISO15765 channels, but a pass-all strategy has no use for a block filter.

## Alternatives Considered

1. **Keep installing both filter types on ISO15765 and ignore rejections** —
   Works against the bundled mock and possibly some lenient real adapters,
   but violates the spec and risks outright failure (or undefined behavior)
   against spec-conformant hardware.

2. **Make filter type selection a ComParam-driven policy** — Over-engineered;
   the ISO15765-vs-other split is a fixed protocol constraint, not a
   configurable choice.

## Consequences

- ISO15765 channels no longer attempt a `PASS_FILTER` call; only the
  `FLOW_CONTROL_FILTER` is installed, matching the spec.
- The `FLOW_CONTROL_FILTER` install failure on ISO15765 is now surfaced to
  the caller of `ConnectComLogicalLink` (via `install_pass_all_filter`'s `?`)
  since it is no longer backed up by a `PASS_FILTER`. This is a stricter
  failure mode than before but reflects that the channel would otherwise
  receive no frames at all.
- ADR-005 and ADR-008 are superseded by this ADR for the specific point of
  "which filter type(s) are installed per protocol"; their broader
  decisions (pass-all strategy exists; filters are re-installed after
  `CLEAR_MSG_FILTERS`) still stand.
