# ADR-284: The Agent Passes the Registration Definition's `LongSize` to the J2534 Worker

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `crates/agent/src/launch.rs` (`ngr-agent run`), `crates/agent/docs/ngr-agent.md`, design 7.3

## Context

Design 7.1.1 lets a Linux registration definition declare the width of J2534 `unsigned long`
with an optional `LongSize`, and 7.1.2 makes the ABI table's width the default only when the
definition declares none. ADR-240 Decision item 4 had the agent always pass the ABI default,
because the agent did not read the definition at the time. Since ADR-281 the agent resolves
each worker build's library through `ngr-library-resolver`, so the resolved definition, with
its `LongSize`, is already in hand; dropping it makes a library built with a non-default width
unusable (its structures would be laid out with the wrong width).

## Decision

1. `agent::launch` takes the worker's `long_size` from, in order: the caller's
   `LaunchOptions::long_size`, the `LongSize` of the definition the lookup resolved for the
   chosen build, the ABI default (7.1.2). A Windows registry entry declares no width, so on
   Windows the default applies as before.
2. A declared width of 8 for an ABI whose `unsigned long` is 32 bits wide (win-x86, win-x64,
   linux-x86, linux-armhf) fails the launch before the worker starts. Those worker builds
   cannot represent an 8-byte `unsigned long`; starting them would only fail later, or run with
   the wrong width.

This supersedes ADR-240 Decision item 4.

Alternative not taken:

- The worker reading `LongSize` from the definition it resolves itself, with the environment
  variable as a fallback: one source for the value, but it changes the contract between
  `worker-host` and the j2534-0404 sys layer, which reads the width from the environment when
  it loads the library. The definition directory is fixed at build time (ADR-228), so an agent
  and workers built with the same one resolve the same definition, and the agent's value is the
  one that definition declares for the library the worker loads. Builds with different
  directories can disagree on the library and its width alike; `crates/agent/docs/ngr-agent.md`
  states that deployment constraint.

## Consequences

- A vendor library whose width differs from the 7.1.2 default can be used on Linux by
  declaring `LongSize` in its definition.
- A wrongly declared width still shows up only at the first J2534 exchange; a load-time check
  of the width is separate work.
- The VCI profile override of the width (9.3) is not implemented; there are no VCI profiles yet.
- `crates/agent/tests/cli.rs` checks on 64-bit Unix hosts, with a stand-in worker that records
  the variable it was started with, that a declared `LongSize = 4` and the default 8 reach the
  worker.
