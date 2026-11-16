# ADR-482: A Configured DID's Own Security Level Decides Its Write, and the ECU-Wide Flag Gates Only the Built-in Services

**Date:** 2026-11-16
**Status:** Accepted
**Affects:** `crates/sim-ecu` (`EcuConfig::dids`, `DidConfig`, `DidWrite`, `write_configured_did`), `crates/sim-ecu/docs/simulated-ecu.md`, `crates/sim-vci` (state file version); details ADR-241 item 2

## Context

`sim-ecu` gained configurable data identifiers (`EcuConfig::dids`), each with its own write
condition: the session it may be written in and the security level that must be unlocked. The
ECU already had one flag for security, `EcuConfig::require_security_access`, which makes the VIN
write, eraseMemory and the download require the unlocked level. A configured DID's write
condition meets that flag, and the two can disagree: a DID that names a level in an ECU whose
flag is off, or a DID that names none in an ECU whose flag is on. Which of them decides had to be
fixed, since configurations and tests of the coding writes depend on it.

## Decision

1. A configured DID that has a `security_level` always requires the level to be unlocked for its
   write, whatever `require_security_access` says.
2. A configured DID without a `security_level` never checks the lock, whatever the flag says.
3. `require_security_access` keeps gating only the built-in services it gated before: the VIN
   write, eraseMemory and the download.
4. The only valid level is 1, as the simulator has one level; `EcuConfig::validate` refuses
   another value, and a level without a non-default session named (an absent session includes
   the default one), where SecurityAccess is not available.

## Consequences

- One ECU runs two rules: the flag for the built-in services and the DID's own level for the
  configured ones. A test that turns the flag off still sees a configured DID with a level
  refused until the unlock; this is covered in `crates/sim-ecu/src/tests.rs`.
- A configuration states the condition of each coding DID where the DID is declared, so one ECU
  can hold DIDs of different levels of protection without a global switch.
- A built-in DID cannot be configured, so the built-in rule never applies to a configured DID.
