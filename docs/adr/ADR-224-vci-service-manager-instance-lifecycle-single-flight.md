# ADR-224: `vci-service-manager` Instance Lifecycle Gains a Single-Flight State Machine

**Date:** 2026-09-09
**Status:** Accepted
**Affects:** `vci-service-manager/src/main.rs`

## Context

`AppState.instances: Arc<Mutex<HashMap<String, Arc<Mutex<ManagedInstance>>>>>` holds one
entry per `library_id`, present only once an instance has fully spawned and passed its
startup handshake. `ensure_instance_started` (the `POST /vci-libs/{library_id}` handler's
core) checks the map for an existing live entry, and if none exists, spawns a new instance
and inserts it — but the spawn-and-handshake step (`authenticate_and_wait_until_running`,
bounded by `INSTANCE_START_TIMEOUT = 20s`) runs with **no lock held on `instances`** for
its whole duration. This check-then-act shape, across a ~20s lock-free window, produces
three confirmed, independently reachable defects under ordinary concurrent client
behavior (a browser double-click, a tab reload racing a request, two clients hitting the
same library) — not synthetic races:

1. **A genuinely healthy instance is silently torn down.** Two concurrent `POST`s with no
   existing entry both pass the initial liveness check, both independently spawn and pass
   the handshake (device-exclusivity contention cannot arbitrate between them — neither
   `j2534-0404-service::new` nor `iso22900-service::new` opens the physical device at
   construction time; that happens later, per-module, on demand). Whichever reaches the
   final insert first wins the map slot. If that winner's process then exits for any
   unrelated reason before the loser's own final check runs, the loser sees
   `contains_key == true` (the *dead* winner's entry), concludes it lost, and stops its
   own, actually-healthy instance.
2. **The initial liveness check's dead-entry cleanup (`instances.remove(library_id)`) is
   an unconditional remove-by-key, not an identity-checked removal.** Two requests that
   both observe the same dead entry can interleave so that the first one's cleanup, spawn,
   and successful insert are followed by the *second* one's now-stale cleanup call, which
   removes-by-key and deletes the first request's freshly-inserted **healthy** entry
   instead of the dead one it originally observed.
3. **`DELETE /vci-libs/{library_id}/endpoint` and shutdown (`stop_all_instances`, run from
   `shutdown_signal`) do not participate in any per-`library_id` coordination.** A `DELETE`
   arriving while a start is in flight (spawned, mid-handshake, not yet in the map) finds
   nothing to remove and returns `204` as if it succeeded; the in-flight `POST` completes
   moments later and inserts the instance anyway, silently ignoring the client's explicit
   stop request. `stop_all_instances`'s single `drain()` has the same blind spot against a
   `POST` that inserts after the drain has already run.

`vci-service-manager/docs/WEB_API_IMPROVEMENT_PLAN.md` item 3 (a temporary scratch memo,
not a permanent design document) already established these three defects in detail and
named "makes the second concurrent request await the first's outcome instead of
independently spawning" as the right general direction, explicitly leaving the concrete
data structure and the DELETE/shutdown integration as an unresolved design decision
requiring a `design-advisor` consult before implementation — this is that consult's
result, formalized.

No existing ADR covers this crate's instance lifecycle (`docs/adr/INDEX.md`'s rows for
`vci-service-manager` are ADR-032 (`main.rs` binary resolution), ADR-220 (`endpoints`-based
`get_status` parsing, which is what makes the mid-`Starting` interleaving above practically
reachable at all), and ADR-221 (the shared-listener bearer-token/gRPC-Web work that added
`issue_token`, which also reads the instance map)).

## Decision

Replace the bare `Arc<Mutex<HashMap<String, Arc<Mutex<ManagedInstance>>>>>` with an
explicit per-`library_id` lifecycle state machine, guarded by a registry lock that is
**never held across an `.await`**, with the actual spawn-and-handshake work running in a
single detached task per `library_id` (never inside the HTTP handler's own future, which
hyper can drop on client disconnect — e.g. the tab-reload case — leaving a `Starting` slot
stuck forever if the leader ran there instead).

1. **Data structure.**

   ```rust
   struct Registry {
       shutting_down: bool,
       slots: HashMap<String, Lifecycle>,   // absent key == Idle
   }
   type SharedRegistry = Arc<std::sync::Mutex<Registry>>;

   enum Lifecycle {
       Starting(Arc<InFlight>),
       Running(Arc<tokio::sync::Mutex<ManagedInstance>>),
   }
   struct InFlight {
       cancel: tokio_util::sync::CancellationToken,
       outcome: tokio::sync::watch::Receiver<Option<StartOutcome>>,
   }
   #[derive(Clone)]
   enum StartOutcome { Started, Failed(StatusCode, String) }
   ```

   `Registry` is guarded by a **`std::sync::Mutex`**, deliberately, not a `tokio::Mutex`:
   its guard is `!Send`, so the compiler itself rejects any attempt to hold it across an
   `.await` inside an axum handler or spawned task — the "never held across an await" rule
   is enforced structurally, not by convention. Every registry critical section is a plain
   synchronous block that reads or mutates `slots`/`shutting_down` and returns before any
   `.await` runs. The per-instance `tokio::Mutex` (`ManagedInstance`'s lock, unchanged) and
   the registry lock are acquired in strictly disjoint critical sections, never nested.

2. **Single-flight mechanics.** `ensure_instance_started` becomes a loop: take the registry
   lock, inspect `slots.get(id)`, drop the lock, then act.
   - `None` (Idle): insert `Starting(inflight)` under the same critical section that
     observed the absence, then release the lock and `tokio::spawn` the leader task
     (`run_start`), which owns the spawn + `authenticate_and_wait_until_running` call and
     is the **only** writer of that `Starting` slot's eventual resolution.
   - `Starting(f)`: clone `f`'s `watch::Receiver` and await it (`rx.wait_for(|o|
     o.is_some())`) — no registry lock held while waiting.
   - `Running(inst)`: lock only the instance's own mutex to check `is_alive()`. If alive,
     return `Ok(())`. If dead, take the registry lock again and remove the slot **only if
     the current `Running` value is still the same `Arc`** (`Arc::ptr_eq`) as the one just
     observed dead — a compare-and-remove, not a remove-by-key — then loop again to become
     the new leader.

   A follower that received `Started` does not re-run its own liveness check: `Started`
   means "the leader's own handshake completed", the same contract the leader's own
   success already carries today, and `start_library_instance` already re-reads status for
   its response body regardless. This does not reintroduce defect 1's TOCTOU because a
   follower never performs a destructive act (stop/remove) based on this observation — only
   the identity-checked compare-and-remove above is destructive, and it is scoped to the
   `Running` case, never to a `Starting` one.

   The leader task (`run_start`), sketched:

   ```
   let _guard = LeaderGuard { registry, id, tx };  // Drop: if unresolved, clear the
                                                    // Starting slot and broadcast Failed
   let inst = spawn_instance(&entry).await?;       // Err -> resolve Failed
   if cancel.is_cancelled() { inst.reap().await; resolve(cancelled) }
   let hs = select! { r = authenticate_and_wait_until_running(&mut inst) => r,
                      _ = cancel.cancelled() => Err(cancelled) };
   if hs.is_err() { inst.reap().await; resolve(...) }
   let arc = Arc::new(Mutex::new(inst));
   let cancelled_late = { lock registry;             // atomic wrt DELETE/shutdown's cancel()
       if cancel.is_cancelled() { slots.remove(id); true }
       else { slots.insert(id, Running(arc.clone())); false }
   };
   if cancelled_late { arc.lock().await.stop().await; resolve(cancelled) }
   else { resolve(Started) }
   ```

3. **`DELETE` and shutdown integration.**
   - `stop_library_instance` locks the registry once: `None` → `204`; `Running` → remove,
     release the lock, `stop()`, `204`; `Starting(f)` → call `f.cancel.cancel()`, clone
     `f.outcome`, release the lock, await the outcome, then return `204` unconditionally
     (the leader has already reaped or stopped by the time its outcome resolves). This
     PR's `DELETE` **cancels** an in-flight start rather than waiting for it to finish and
     then stopping the result: `DELETE`'s whole contract on this management API is "make
     sure this is not running when I return", and cancel bounds the wait at the reap
     timeout (a few seconds) instead of up to the full ~20s handshake plus a discarded
     startup's own stop sequence. The cancelled leader's own `POST` (and any followers)
     receive `409 Conflict` ("stop requested during startup").
   - `stop_all_instances` (called from `shutdown_signal`) takes the registry lock once,
     sets `shutting_down = true`, cancels every `Starting` slot (collecting their outcome
     receivers) and removes every `Running` slot, releases the lock, then awaits all
     collected outcomes before stopping the collected `Running` instances. Any `POST` that
     reaches the registry after the flag is set returns `503` without inserting anything;
     any leader already past its own cancellation check sees it via the same
     `cancelled_late` path `DELETE` uses. `shutdown_signal` awaits every outcome before
     returning, so `axum::serve`'s graceful-shutdown future cannot complete while a child
     process is still alive.

4. **Failure handling.** A leader's failure or cancellation resolves the `watch` channel to
   `Failed`/`cancelled` and removes the slot (via `LeaderGuard`'s `Drop` or the explicit
   resolution paths above); every follower observes the same terminal value (a `watch`
   channel retains its last value, so no follower can miss it or hang) and returns the
   corresponding error. No follower retries within its own request — the slot is `Idle`
   again once the leader resolves, so the *next* incoming request (from a retrying client,
   or any other caller) becomes a fresh leader through the same loop. This avoids both a
   lost wakeup and a thundering herd of independent re-spawns racing each other the moment
   a leader fails.

5. **Scope.** This is one PR, not "fix the race" plus a follow-up for DELETE/shutdown: the
   leader's `cancelled_late` check under the registry lock *is* the POST/DELETE
   serialization point, and the `Starting` variant only exists because DELETE and shutdown
   need something to observe and cancel during an in-flight start. Shipping single-flight
   alone would leave defect 3 open with a `Starting` state that has no consumer.

## Consequences

- No duplicate spawns for the same `library_id` under any concurrent request pattern; the
  three defects in Context are structurally eliminated, not patched around.
- `POST /vci-libs/{library_id}/instance` (route renamed from `POST /vci-libs/{library_id}` by
  ADR-225, after this ADR was written) gains two new response codes: `409 Conflict` (a start
  was cancelled by a racing `DELETE`) and `503 Service Unavailable` (the manager is shutting
  down). Existing `200`/error-body semantics are otherwise unchanged.
- `DELETE /vci-libs/{library_id}/instance` (route renamed from
  `DELETE /vci-libs/{library_id}/endpoint` by ADR-225)'s worst-case latency changes from
  "immediate, if nothing is running" to "bounded by the reap timeout" when it arrives during
  an in-flight start — a few seconds, not the up-to-20s handshake window a wait-then-stop
  design would have imposed.
- `GET /vci-libs/{library_id}/instance` (route renamed from `GET /vci-libs/{library_id}` by
  ADR-225; `read_instance_status`), `issue_token`, and
  `live_instance_ids` (the `/healthz` liveness sweep) all continue to treat a `Starting`
  slot as "not running" — wire-compatible with today, where an in-flight instance is
  simply absent from the map.
- `tokio-util`'s `CancellationToken` becomes a direct dependency of `vci-service-manager`
  (previously only pulled in transitively via `tonic`).
- **Accepted residual:** a child process that exits immediately after a successful
  handshake (before any caller's next liveness check) still produces a `200` response
  whose body reports `running: false` — this is not a regression (today's leader-only path
  has the identical property) and is not addressed by this ADR; a client is expected to
  retry on a `running: false` body regardless of which request (leader or follower)
  produced it.
- Two related items are deliberately left out of scope for this PR, as genuinely separable
  follow-ups: applying the same `Arc::ptr_eq` compare-and-remove discipline inside
  `live_instance_ids`'s own dead-entry observation (it does not mutate the map today, so
  this is presently moot, but would need it if that ever changes), and the still-open,
  unrelated management-route-authentication backlog item already tracked in
  `vci-service-manager/docs/implementation-notes.md`'s Prioritized Backlog.
