# API descriptions

- `openapi.yaml`: the server's Web API (design 12).
- `asyncapi.yaml`: the control channel between the agent and the server, and the browser stream
  (design 5.2).

Both describe the intended API, and an implemented part must match them. The rule:

- An operation (openapi) or a message (asyncapi, under `components.messages`) that the build does
  not implement carries the vendor extension `x-ngr-status: planned`. The only value is
  `planned`. Remove the mark in the change that implements it.
- Every unmarked operation is served, and every route the server serves is in `openapi.yaml`
  unmarked (the control channel's upgrade route is described by the channel's `address` in
  `asyncapi.yaml` instead). A path in `openapi.yaml` is the served path without the `/api` prefix
  that every `servers` URL ends in.
- Every unmarked message is one `shared-proto` defines (by its `type`, and for `agentEvent` by
  its `kind`), with the same property names and the same required ones; every control message
  `shared-proto` defines is in the file unmarked. A browser-stream message has no `type` and
  stays marked until it is implemented.

`crates/server/tests/postgres/api_spec.rs` checks this in CI and lists every mismatch of a run.
The routes come from the router's own mounting (`server::router_with_routes`), so a route added
without going through it fails the test too. The depth is the method and path of a route, and the
name and requiredness of each property of a message; value ranges, schemas of nested objects,
responses and parameters are not compared.
