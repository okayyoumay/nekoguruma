# ADR-052: Loopback gRPC Binding Tolerates a Missing IPv4/IPv6 Address Family

**Date:** 2026-07-03
**Status:** Accepted
**Affects:** `vci-service-launcher/src/vci_server.rs` (`spawn_vci_context`, `bind_loopback_listeners`), `j2534-0404-service/docs/startup-spec.md` (`get_status.endpoints`)

## Context

`spawn_vci_context` binds the gRPC server to both loopback addresses —
`127.0.0.1` and `[::1]`, on the same port — so that clients connecting via
either address family (including `localhost` resolutions that prefer IPv6)
reach the service. Both binds were mandatory: a failure of either aborted
startup with `?`.

On hosts without IPv6 (common for containers and CI sandboxes; the kernel may
lack the address family entirely, so even `/proc/sys/net/ipv6` is absent),
the `[::1]` bind fails with `EAFNOSUPPORT` ("Address family not supported by
protocol", os error 97) and the whole service reports `running: false` even
though IPv4 works fine. This surfaced as three
`iso22900-service/tests/stdio_startup.rs` failures in an IPv4-only container;
the mirror problem exists on IPv6-only hosts, where the `127.0.0.1` bind
fails first and startup never reaches the IPv6 bind.

## Decision

`bind_loopback_listeners` treats a **missing address family** as skippable
and everything else as fatal:

1. Try `127.0.0.1:{requested_port}`. If it fails with a
   family-unavailability error, log a warning and continue; any other error
   (e.g. `EADDRINUSE` on a requested fixed port) still fails startup.
2. Try `[::1]` on the port the IPv4 listener actually got (or the requested
   port when IPv4 was skipped), with the same rule.
3. If **both** families are unavailable, startup fails with a combined
   error; the server never starts with zero listeners.

"Family unavailability" is detected by `is_family_unavailable`:
`ErrorKind::AddrNotAvailable` or `ErrorKind::Unsupported`, plus the raw
`EAFNOSUPPORT` code (97 on Unix, `WSAEAFNOSUPPORT` 10047 on Windows), which
has no stable `ErrorKind` mapping on all platforms.

`VciServerContext.endpoints` accordingly becomes `Vec<SocketAddr>` (1 or 2
entries, never empty) instead of `[SocketAddr; 2]`, and the JSON-RPC
`get_status` result's `endpoints` array now carries one entry per bound
family. When both families are bound they always share one port.

## Alternatives Considered

1. **Keep both binds mandatory** — correct on dual-stack developer machines
   but makes the service unusable on IPv4-only containers/CI and IPv6-only
   hosts. Rejected.
2. **Bind a single dual-stack socket (`[::]` with `IPV6_V6ONLY=0`)** — still
   requires IPv6 to exist, binds non-loopback addresses (widening exposure
   beyond localhost), and dual-stack behavior of `V6ONLY` is
   platform-dependent. Rejected.
3. **Tolerate every IPv6 bind error, not just family unavailability** — an
   `EADDRINUSE` on `[::1]:{port}` while we hold `127.0.0.1:{port}` would then
   silently leave that port answered by *another process* for IPv6 clients
   (e.g. `localhost` resolving to `::1`). Keeping non-family errors fatal
   preserves that protection. Rejected.

## Consequences

- The service starts on IPv4-only and IPv6-only hosts; the previously
  environment-dependent `stdio_startup` tests pass on IPv4-only containers.
- `get_status.endpoints` is no longer always a pair. Clients must treat it
  as a non-empty list and pick an address they can reach, rather than
  indexing a fixed `[v4, v6]` layout (`startup-spec.md` updated).
- On a single-family host, clients resolving `localhost` to the *other*
  family will not connect; the `endpoints` list is authoritative for what is
  actually bound.
- Unit tests in `vci_server.rs` cover the environment-independent
  invariants: at least one listener binds with all listeners sharing one
  port, and a genuine port conflict still fails startup.
