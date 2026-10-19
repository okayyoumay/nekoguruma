# ADR-334: The Agent Runner Is Generic over a Worker Client, and the Harness Guards It

**Date:** 2026-10-19
**Status:** Accepted
**Affects:** `crates/agent` (`client`, `runner`, `host`, `link`, `inputs`), `crates/ngr-hw-checks` (`guard::runner`, `guard::handle`, `docs/hardware-profile.md`), `crates/agent/docs/ngr-agent.md`, design 13.5 ("Guarded VCI access"); ADR-275 item 4, ADR-333, ADR-235

## Context

Design 13.5 and ADR-275 item 4 require a job run through the agent in a hardware check to be
mediated exactly like a direct check: the guard sits between the agent and the worker. ADR-333 gave
the guard's decision and the handle a direct check uses, and left the seam for the agent's job
runner open. The runner, the host and the link set-up called the concrete gRPC client
(`worker_host::client::WorkerClient`), so the harness had no place to put a guard in front of them.
`ngr-hw-checks` already depends on `agent`, so the seam has to be declared in `agent` and
implemented in `ngr-hw-checks`.

Two things in the runner's own link set-up do not fit the guard as it stands. The runner connects
and disconnects the worker's module itself, while the harness opens and matches the device and the
guard refuses `ModuleConnect`. And the runner sets the baud rate, timing, the two identifiers and
nothing else, while the guard refuses a connect until every ComParam the profile pins is set.

## Decision

1. **A trait, with the production client unchanged.** `agent::client::WorkerApi` has one method
   per RPC the runner, the host and the link set-up use (GetModuleIds, ModuleConnect,
   ModuleDisconnect, GetVersion, GetResourceIds, GetObjectId, the four logical-link calls,
   SetComParam, SetUniqueRespIdTable, StartComPrimitive, SubscribeEvent, IoCtl), with the
   generated client's signatures, so a call keeps its `tonic` result and the runner's status
   classification. `WorkerClient` implements it by delegating. The runner entry points, `WorkerHost`,
   the link functions and the input reads are generic over it (static dispatch, no `Box<dyn>`);
   `main` and `launch` stay concrete. `held_module()` is `None` by default: a client that holds the
   worker's module says so, and then the link set-up neither lists, connects nor disconnects it, and
   a failed open never disconnects it.
2. **`GuardedClient` over `GuardedHandle`.** The harness's client implements the trait by calling the
   handle's per-RPC methods, so the guard decides every call and records it in a trace. It holds the
   device (`open_with` / `attach_with` register the device's closing and create no link) and
   reports the module as held, so the device's closing step disconnects it once. A job that asks to
   connect or disconnect a module is refused. It is the one place the seam adds to a request
   rather than passing or refusing it: on `SetUniqueRespIdTable` the profile's other table entries
   (formats, and address bytes when addressed) are appended, their ids resolved first; on
   `ConnectComLogicalLink` every other pinned ComParam is set at the profile's value first. Each added
   request is decided and traced like any other, and the job's own identifiers and ComParams are
   compared with the profile's unchanged, so a mismatch is still refused.
3. **Errors keep the runner's classification.** A guard refusal is `PermissionDenied`; the lost VCI is
   the status the workers use for an offline link, with the lost-communication event, so the host still
   reports it; a status the worker returned is rebuilt from its code (`HandleError::Rpc` and
   `WorkerRefused` carry it); a timeout is `DeadlineExceeded`; anything else `Internal`.
4. **Subscribe only when the handle reads events itself.** A worker keeps one subscription per link,
   so the handle does not subscribe to a link a job created (the job does), and subscribes anew when it
   has to read events, such as the return to the default session after the job. That subscription
   goes through the guard.

## Consequences

- T1 to T3 jobs are mediated; a T0 profile is refused at link creation, since a job's link is not
  RawMode.
- The guarded link is the profile's: the runner cannot connect a link with other identifiers, formats,
  addressing or padding than the profile pins, and the ComParams the runner does not set are the
  profile's rather than the worker's preset.
- The runner's unary deadline (5 s) bounds a guarded call, which can carry several worker requests
  (the link-setup completion, the handle's cancels before a disconnect); the handle's request timeout
  is at least that, each worker request bounded by it.
- The handle cancels the primitives it started and has not seen end before a disconnect or destroy,
  so a job's primitive is cancelled there (the fake and the workers answer a cancel of an ended
  primitive with success).
- The handle does not read a job's events, so it never sees a job's primitive end. It keeps only the
  latest primitive per link and cancels that one before a disconnect or destroy (the link's own
  disconnect ends the rest), so a long job costs one cancel inside the runner's deadline, not one per
  primitive. A cancel that fails with the error detail `PDU_ERR_INVALID_HANDLE` (what
  `j2534-0404-service` answers for a primitive it forgot, and what `iso22900-service` passes on from
  the vendor library as an internal error carrying that detail) is taken as "already ended".
- A job's own return to the default session (`10 01`) is not observed (its answer arrives on the event
  stream the job reads, which the client does not wrap), so the guarded disconnect after it sends
  one more `10 01` to the ECU, within the runner's deadline. Only `send_receive` through the handle
  keeps the pending return in step.
- The decision trace is what the worker received plus the refusals: a call the guard refused is
  recorded and not sent, a module call granted locally is neither recorded nor sent.
- A failed `CreateComLogicalLink` with a held module marks the job's guards unconfirmed (ADR-258)
  unless the worker or the guard refused it (a `PermissionDenied` or a status the workers use for
  refusals); a timeout may have left a link nobody holds a handle for, and the module is not
  disconnected to clean it.
- Identifiers and addressing: the runner's own link set-up refuses 29-bit identifiers
  (`LinkConfig::validate`) and a document-declared link with extended addressing (`declared`); a link
  built by hand with extended addressing goes through, and the client then appends the profile's
  extended format and address bytes to the response table. The runner still sets `CP_RCByteOffset` to 2
  whatever the addressing, which has not been checked against a worker for extended addressing.
