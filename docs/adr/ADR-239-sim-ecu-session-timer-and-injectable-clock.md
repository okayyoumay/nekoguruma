# ADR-239: sim-ecu Session Timer and Injectable Clock

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/sim-ecu` (`Clock`, `EcuConfig::s3_server_ms`, `EcuConfig::security_delay_ms`, timer handling), `crates/sim-vci` (its ECU runs on real time)

## Context

`sim-ecu` had no clock. A non-default session never timed out, and the SecurityAccess delay ended
only when a test called `expire_security_delay()`. The interrupted-write tests (design 5.3 / 5.6,
ADR-229) need a session that expires on its own, and design 13.4 asks for clock substitution in
tests.

ISO 14229-2 (2021) clause 9.5 defines tS3_Server on transport-layer events: the start of a request
stops it, and the end of sending the final response restarts it. `sim-ecu` has no transport
layer. It handles a whole request at once and only reports how long after the request its
response goes out; `sim-vci` applies that delay. So the simulator has to pick points on its own
time line that stand for those events.

## Decision

1. **Injectable clock.** The ECU's timers run on a `Clock`. `SimEcu::new` uses real time;
   `SimEcu::with_clock` takes any other, such as `ManualClock` for tests. Timers are evaluated
   when the next request arrives, or when a test calls `check_timers()`; nothing runs in the
   background.
2. **tS3_Server restart point.** Every request that reaches the ECU restarts tS3_Server,
   whether its service is supported or not (Table 6 bases the timer on received requests, not on
   their outcome). The timer runs from when the response finishes going out, which the simulator
   takes as the request time plus the response delay it reports. A response that is then lost
   (`Fault::DropResponse`) counts as sent, since the server finished sending it. When no response
   is sent (suppressed, or none due), it runs from the request. A request lost on the bus
   (`Fault::BusError`) never reaches the ECU and does not restart it. The default session has no
   timer. Expiry has the effect of a change to the default session: security is relocked and a
   running download is interrupted.
3. **Defaults.** tS3_Server is 5000 ms (clause 9.5, Table 5) unless `EcuConfig::s3_server_ms`
   sets another. The security delay is 10000 ms unless `EcuConfig::security_delay_ms` sets
   another; ISO 14229-1 (2026) clause 9.4.1 leaves its length to the vehicle manufacturer.
4. **Timers across power cycles.** Timers keep running while the ECU is off or silent. A power
   cycle or ECU reset first applies the timers that have run out, then restarts a security delay
   that is still running for its full length.

## Consequences

- `sim-vci`'s ECU runs on real time, so sessions there time out after 5 s without a request, as
  on a vehicle. Tests that hold a non-default session through `sim-vci` send TesterPresent or set
  a longer `s3_server_ms` in `NGR_SIM_ECU_CONFIG`.
- Because the restart point is computed from the reported delay, a response that `sim-vci` later
  discards (a power cycle while it is still delayed) still counts as sent for tS3_Server. The ECU
  has power-cycled by then, which ends the session anyway.
- Clause 9.4.1 also asks a server that supports the delay to start it at power-up after a single
  earlier false key. The simulator does not; this is recorded as a known gap.
