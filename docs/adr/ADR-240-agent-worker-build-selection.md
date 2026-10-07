# ADR-240: Agent Worker-Build Selection by Per-Build Library Resolution

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `crates/agent/src/launch.rs`, `crates/agent/src/main.rs` (`ngr-agent run`), `crates/agent/docs/ngr-agent.md`, design 7.3

## Context

The agent starts a j2534-0404 worker with a VCI's library name only, and the worker resolves
that name itself (design 7.2). To choose which worker build to start (7.3), the agent needs the
library's header, so it must resolve the same name first. The worker's resolution depends on
the build:

- the `config.toml` lookup takes the build's architecture key (`x86` or `x86_x64` on Windows,
  none elsewhere) before the api-level `library_path` entry;
- the registry fallback reads the build's native view, which is the 32-bit view for the x86
  build and the 64-bit view for the x64 build.

A single agent-side lookup (no architecture key, the agent's own native view) misses a VCI
registered only in the 32-bit view, the usual case for J2534 DLLs, and with an
architecture-specific `library_path` entry it can read a different file than the build it
picks will load. Design 7.1 also asks for both registry views to be named explicitly.

## Decision

1. The agent resolves the name once per worker build it can start, with that build's
   architecture key and registry view (both views named explicitly on 64-bit Windows), and
   reads the header of each result.
2. It starts the first build whose resolved library is built for that build's own ABI. A
   build whose lookup fails, or finds a library of another ABI, is skipped, and every skip
   reason is reported when no build matches.
3. Order on 64-bit Windows: the x64 build, then the x86 build. A VCI with a library in both
   views runs on the native x64 build, which needs no WOW64 layer. A 32-bit Windows agent
   tries only the x86 build. On Linux there is one lookup, without an architecture key, and
   the build follows the header's ABI.
4. The agent passes the ABI's default `long_size` (7.1.2) to the worker.

Alternatives not taken:

- Telling the worker which file to load (a path argument): design 7.2 keeps the decision of
  which file is loaded inside the worker, with the build-time-fixed resolver, so that no
  caller can redirect it.
- Refusing a VCI that resolves in both views: vendors that ship both bitnesses register both,
  and either build works; refusing would make those VCIs unusable without a configuration
  step.

## Consequences

- The header that selects the build belongs to the file the started build resolves, as long as
  the agent and the worker read the same `config.toml`. Builds with different `config-root-*`
  features break that; `crates/agent/docs/ngr-agent.md` states the constraint.
- A 32-bit agent on 64-bit Windows does not see VCIs registered only in the 64-bit view,
  because `j2534-0404-registry` exposes explicit views only to 64-bit builds.
- A vendor library whose `long_size` differs from the 7.1.2 default cannot be used until the
  agent reads the registration definition or the VCI profile override.
- The ordering is one constant per platform (`RESOLUTIONS`); the selection logic is unit-tested
  with a fake lookup and header reader.
