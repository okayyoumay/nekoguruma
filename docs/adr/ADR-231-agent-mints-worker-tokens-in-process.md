# ADR-231: The Agent Mints Worker Bearer Tokens In-Process

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `worker-host` (`client.rs`), `vci-service-interface` (`token.rs`), `vci-service-launcher` (`lib.rs`), `docs/system-architecture.md` 7.4, `docs/worker-crates.md`, `docs/glossary.md`, `docs/adr/ADR-221-vci-service-shared-listener-auth-and-proxy-removal.md`

## Context

ADR-221 put bearer-token auth on the worker's gRPC listener and split the roles across processes: `vci-service-manager` generates the per-instance key, provisions it to the worker over stdin, and mints short-lived tokens for separate client processes through `POST /vci-libs/{id}/token`. The key is never handed to a client.

In this repository there is no standalone service manager. The agent takes that role (design 3.3, 7.4): `worker-host` launches the worker, generates the key and provisions it. The agent is also the only client that drives the worker. The agent needs a gRPC client, so it has to get tokens somehow, and ADR-221's split does not say how when the key holder and the client are the same process. The token code also lived in `vci-service-launcher`, the service-side crate, which a client should not need to depend on.

## Decision

1. **The key holder mints for itself.** `worker-host`'s client (`client::BearerAuth`, a tonic interceptor) keeps the worker's key in the agent process and mints a fresh token for every call. There is no token-issuing endpoint and no token cache. Minting is one HMAC-SHA256, which is negligible next to a gRPC round trip. Because each token is new, a long-lived client never presents an expired one.
2. **The trust boundary stays where ADR-221 put it.** The key exists only in the process that generated it (the agent) and in the worker it was provisioned to. It crosses no process boundary other than the worker's stdin, and it never travels over the gRPC socket. Another local process still cannot obtain a valid token. ADR-221's rule that the key is never handed to a client still holds, because no separate client process exists. The agent plays both the manager's role and the client's role inside one trust domain.
3. **The token format lives in `vci-service-interface`.** `mint` and `verify` move unchanged from `vci-service-launcher` to `vci_service_interface::token`, the crate that client and service already share for the gRPC interface. `vci_service_launcher::token` re-exports it, so the listener's interceptor and its tests are unchanged. The format still has a single definition.

This supersedes, for the agent, ADR-221's manager-side token minting: the Decision paragraph's "the manager mints short-lived bearer tokens from that key on request" and Mechanism item 6 (`POST /vci-libs/{id}/token`). The listener side (Mechanism items 1 to 4) is unchanged.

## Consequences

- A client outside the agent process (a browser, a separate tool) has no way to obtain a token. If one is ever needed, it needs its own issuing path designed against ADR-221's threat model (DNS rebinding, enrollment of local clients). It must not get the key itself.
- The client copies the key when it connects. Re-provisioning a key over `set_auth_key` (ADR-221's revocation lever) invalidates that client's tokens, so a client must be reconnected with the new key. The agent does not rotate keys today.
- As under ADR-221, the listener checks a `SubscribeEvent` stream's token only when the stream opens. Per-call minting does not change that.
