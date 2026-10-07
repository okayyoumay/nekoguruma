> **TEMPORARY WORKING MATERIAL.** Everything under `work/` is consumable: task lists, open items and status notes that are worked through and then deleted. It is not part of the design or the product.

# Worker crates: backlog and known flaky tests

Open items and known flaky tests of the worker crates (`docs/worker-crates.md`), per crate. Entries cite ADRs in `docs/adr/`. Delete an entry once it is fixed; record lasting decisions in the crate's `docs/implementation-notes.md` or an ADR.

Most priorities here were set under the earlier scale these crates were developed with, where P3 meant "lower priority" rather than "nice to have". Re-check an item against the scale in `work/README.md` when you touch it. Entries under Known Flaky Tests are all P1 (`work/README.md`).

## `iso22900-mock`

### Prioritized Backlog

- **P2**: `iso22900-mock`'s `PDUIoCtl` (`crates/iso22900-mock/src/lib.rs`) takes the IOCTL command ID as `T_PDU_IT`, while the header `crates/iso22900-sys/src/bindings/d_pdu_api_func.h` and the bindings' `DPduApiSys::PDUIoCtl` declare `UNUM32`. Both are 32-bit integers, so calls work, but `crates/iso22900-mock/tests/abi_parity.rs` has to check `PDUIoCtl` by layout instead of by signature. Done when: the mock's parameter is `UNUM32` (converted to `T_PDU_IT` inside where it is compared), `PDUIoCtl` is in that test's signature list and its layout-only test is removed.
- **P2**: Document parallel-test constraints and recommended reset strategy.
- **P2**: Pull-request CI never compiles `crates/iso22900-mock/tests/abi_parity.rs` for a target where the mock's `exported_fn!` picks `extern "stdcall"` (Windows x86): `core-windows` is x86_64 and `worker-check` in `.github/workflows/ci.yml` runs `cargo check` on the services without their tests, so a calling-convention drift between the mock and the `iso22900-sys` bindings there would merge unnoticed (it type-checked locally with `cargo check -p iso22900-mock --tests --target i686-pc-windows-gnullvm` when the test was added). Done when: `worker-check` (or another pull-request job) runs that command for `i686-pc-windows-gnullvm`.

## `iso22900-service`

### Prioritized Backlog

- **P3**: Extend `iso22900-mock`'s error-injection back doors beyond `PDULockResource`/`PDUGetLastError` (currently the only calls with `__mock_set_lock_resource_error`/`__mock_set_last_error` overrides) to the other CLL-scoped native calls (`PDUUnlockResource`, `PDUGetComParam`, `PDUSetComParam`, `PDUCancelComPrimitive`, ...), so a future test can force their specific failure paths through `with_api_for_link`/`map_runtime_error_for_link` the same way `link_scoped_failure_carries_error_event_data_from_last_error` does today.
- **P2**: Add an end-to-end test that actually observes `SubscribeEvent`'s terminal status: `tests/stdio_startup.rs::process_exits_when_stdin_is_closed_with_live_subscribe_event_stream` opens a stream but never polls it (stored as `_event_stream`), triggers only stdin closure (not the `stop` JSON-RPC method), and asserts nothing beyond the child process exiting — so a regression in cancelled-status delivery or in `stop`-triggered termination would go undetected. The related integration test for the client-visible `cancelled` status is a separate item under the cross-crate SubscribeEvent shutdown section (`crates/iso22900-service/docs/subscribe-event-shutdown.md`).
- **P1**: Add full stdio lifecycle integration coverage (ping, get_status, stop, process exit).
- **P1**: Increase structured lifecycle logging for startup/stop/escalation diagnostics.
- **P2**: Add test coverage (unit and/or integration) for `parse_startup_arg`'s unknown-startup-query-parameter-ignoring behavior. Policy is documented in `docs/grpc-instance-spec.md:18` and implemented in `src/config.rs`'s `parse_startup_arg` (the function actually used by service startup), but none of its own tests exercise an unrecognized query key — `parses_library_and_options` only covers the separate `parse_startup_uri` helper, not `parse_startup_arg` (Codex review, PR #109).
- **P2**: Reconcile design-note consistency across lifecycle documents after each refactor.
- **P2** (ADR-204, implementation-time finding): `iso22900-mock`'s `PDUGetEventItem` only ever synthesizes `PDU_IT_RESULT` items, never `PDU_IT_STATUS`, so `service/events.rs`'s `to_proto_event_item` terminal-status branch (evicting a COP's `cop_tags` entry on `PDU_COPST_FINISHED`/`PDU_COPST_CANCELLED`) is unreachable through the existing black-box mock and has no automated test. Extending the mock to emit status events (or another route to a terminal-status event in a test) is needed to close this gap; verified correct today only by direct code reading.
- **P2** (`edge-case-hunter` review, ADR-204/PR #116, pre-existing hazard -- not introduced by ADR-204, only noticed while verifying an ADR-204 fix): `rpc_subscribe_event` (`service/rpc_primitive.rs`) commits its `self.subscriptions` bookkeeping (`self.subscriptions.lock().await.insert(key, stream_tx)` -- which can also evict and cancel a prior live subscriber for the same `(module, cll)` key) *before* the irreversible native `register_event_callback` call, which happens after a separate, later `self.api.lock().await`. If the gRPC request is dropped/cancelled while waiting on that second lock, the subscription slot is left registered with no native callback ever installed -- the CLL's subscription is silently dead (no events will ever arrive) until the spawned finalizer task's `stream_tx.closed()` eventually fires, and even then the finalizer only calls `unregister_event_callback` (a no-op here, since none was ever registered) and never removes the stale entry from `self.subscriptions` itself. This is the mirror image of the cancellation-window bug ADR-204's own `rpc_start_com_primitive` fix closed (bookkeeping-then-native-effect ordering instead of native-effect-then-bookkeeping) -- the same fix shape (commit the bookkeeping only once no further `.await` remains before it, or vice versa: do the native call first) likely applies here too. Not investigated or scoped here.
- **P2** (`edge-case-hunter` review, ADR-204/PR #116): the cancellation-window fix in `rpc_start_com_primitive` (reordering `cop_tags`'s lock acquisition to before the native call, so no `.await` remains between the native side effect and the tag reconcile) has no test isolating its own contribution -- the existing `cop_tag` tests (`start_com_primitive_accepts_max_length_cop_tag`, `start_com_primitive_rejects_oversized_cop_tag`, `subscribe_event_echoes_cop_tag_racing_start_response`, all in `tests/grpc_mock.rs`) exercise size validation and event/response ordering, but none simulates a cancelled/dropped RPC future while `cop_tags` is contended, and none exercises "a reused `(module, cll, cop)` handle echoes a stale prior occupant's tag" -- the fix's correctness rests on direct code reading (confirmed independently, twice), not a fail-without/pass-with test. A targeted regression (hold `cop_tags` locked from a spawned task, drop the `rpc_start_com_primitive` future while it blocks acquiring `cop_tags`, assert the map is left consistent) would close this.

## `iso22900-sys`

### Prioritized Backlog

- **P1**: Add callback/function-pointer ABI sanity checks for supported targets.
- **P2**: Reassess allowlist width and reduce generated surface when safe.

## `iso22900`

### Prioritized Backlog

- **P1**: Add regression checks for target-specific callback/function-pointer ABI handling.
- **P1**: Document borrowed vs owned item lifetime rules with concrete examples.
- **P2**: Consolidate and publish an explicit error mapping table.
- **P3**: `crates/iso22900/tests/callback_lifecycle.rs`'s `unregistering_during_a_delivery_waits_for_the_callback_to_return` can only make it near-certain, not prove, that `DPduApi::unregister_event_callback` (`crates/iso22900/src/lib.rs`) is waiting on the callback registry's lock before the blocked callback returns: the test waits until the unregistering thread reports it is about to call it, then for a fixed window. Forcing the interleaving needs a test-only hook in the wrapper that fires just before the lock is taken (for example behind a test feature), which a new test file alone cannot add. Done when: the test synchronizes on such a hook and releases the callback only after it fired.

## `j2534-0404-service`

### Known Flaky Tests

Load-sensitive tests that can fail spuriously when the full suite runs under
parallel system load. A failure here is **not** a regression unless it also
reproduces in isolation (`cargo test -p j2534-0404-service <test name>`,
several consecutive runs). `cargo-runner` consults this list before
reporting a failure as a regression (see `.claude/agents/cargo-runner.md`);
add a new entry only after confirming the same failure reproduces on a
commit that predates your change.

New timing-sensitive tests should follow the write-time guideline in
`tests/grpc_mock/harness.rs`'s module doc (ADR-149) to avoid adding to
this list in the first place.

Triage note: if widening a suspected timing margin makes a test fail
*more* often (or deterministically) rather than less, that disproves the
margin theory — stop widening and instrument instead (see the
`receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running`
entry below for a worked example).

- **`tests/grpc_mock/j1939.rs`: four tests that failed intermittently in full-module runs, cause not found.** Repeated full-module runs
  (`cargo test -p j2534-0404-service --test grpc_mock -- j1939::`) used to fail a different test about 1 run in 3 (observed:
  `optional_startcomm_message_sends_with_the_claimed_source_address`,
  `repeat_slot_stop_condition_wildcards_pgn_and_destination_exact_matches_source_address`,
  `cancelling_the_optional_startcomm_message_after_a_successful_claim_stops_its_own_repeat_slots`,
  `coptupdateparam_rejects_tester_source_address_off_the_live_claim`), each passing on rerun and in isolation, with 47 tests and
  before ADR-195 made the test config path per-process. On the current tree (79 tests) 18 consecutive runs passed on Linux, 12 of
  them under CPU load, plus 8 as two processes running the module at once. Ruled out by reading the code: a backdoor toggle leaking
  into the next test (`TestServer::try_start_with_extra_config` calls `__mock_reset` first, which resets all of `MockState`), a
  previous test's poll task outliving it (each `#[tokio::test]` drops its runtime, and with it every spawned task, when the test
  ends), and a mock repeat worker writing after a reset (it checks `REPEAT_WORKER_EPOCH` under the same state lock `__mock_reset`
  bumps it under). The one cause found in this file (`claim_retries_the_next_candidate_after_the_first_is_lost` racing the
  global claim-lost toggle) is fixed and does not explain these four. The cross-process config race ADR-195 fixed fits a varying
  failing test but is unconfirmed. Delete this entry if it does not recur; if it does, record the failing test, the panic message
  and whether another test process was running.

See "Resolved" below for this list's history.

#### Resolved (2026-08-07)

- **Process-global test-harness startup race** (a sixth flaky-test root
  cause, distinct in kind from the five ADR-149 timing-margin entries
  below — this one is a genuine concurrent read/write race on shared
  process state, not a wall-clock margin problem). Observed as
  intermittent `RegistryUnsupported` (`"registry lookup is only supported
  on Windows"`) panics from `TestServer::start*` calls during full-suite
  parallel `cargo test -p j2534-0404-service` runs, not reproducible in
  isolation. Root cause: `TestServer::try_start_with_extra_config()`
  (`tests/grpc_mock/harness.rs`) writes a fixed-path temp config file and
  sets the `VCI_CONFIG_PATH` env var, both process-global mutable state,
  then calls `J2534Service::new`, which reads them back synchronously
  (via `vci-service-config`) before its first `.await`. `#[serial]` only
  serializes a test against other `#[serial]`-tagged tests, not against
  the large majority of non-`#[serial]` tests, which run in true parallel
  OS threads under the default `cargo test` harness — so two concurrent
  `start*` calls could interleave their file/env writes and reads with no
  synchronization. `vci-service-config::load_toml_config` treats a
  resulting torn/corrupted read as "nothing configured"
  (`TomlConfig::default()`, warning-only `eprintln!`) rather than
  propagating a parse error, so `library_path` silently resolved to
  `None`, falling through to real Windows-registry auto-discovery, which
  immediately errors on any non-Windows platform. Fixed by adding a
  process-wide `VCI_CONFIG_STARTUP_LOCK: tokio::sync::Mutex<()>` in
  `harness.rs`, held across the file write, env var set, and
  `J2534Service::new(...).await` call in
  `TestServer::try_start_with_extra_config` — the sole entry point all
  `TestServer::start*` helpers funnel through. See that function's own
  doc comment for the full mechanism. This is a test-infrastructure-only
  fix; no production code changed.

  **Correction/supersession (ADR-195):** this fix addressed only the
  intra-process case — `VCI_CONFIG_STARTUP_LOCK` is a per-process Rust
  static, so it did nothing against two separate `cargo test` processes
  racing on the same fixed config file path. A same-day-unrelated
  investigation months later (2026-08-27) found the fix incomplete for
  exactly that reason; see the "Resolved (2026-08-27)" entry below and
  ADR-195 for the cross-process fix.

#### Resolved (2026-08-27)

- **Cross-process test-harness startup race (ADR-195).** The
  2026-08-07 fix above (`VCI_CONFIG_STARTUP_LOCK`) only ever serialized
  concurrent *threads* within a single test process; it left the exact
  same fixed-path temp config file and `VCI_CONFIG_PATH` env var racy
  across two separate `cargo test` *processes* (e.g. two different test
  binaries, or the same binary invoked twice concurrently), since the
  lock is a per-process Rust static with no cross-process visibility.
  Found via a design-advisor investigation triggered by rotating
  flakiness observed across `repeat_message.rs`, `additional_channels.rs`,
  `j1939.rs`, `tp20.rs`, and `response_distribution.rs` during unrelated
  verification work — initially misdiagnosed as two or three separate,
  distinct flakiness clusters, and briefly suspected to be a
  poll-task-shutdown-ordering race in `spawn_channel_poll_task`/
  `TestServer::shutdown()`; that specific hypothesis was investigated and
  empirically refuted via instrumented reproduction before the real
  cross-process cause was found (worth noting here so a future reader
  doesn't re-chase the same dead end). Confirmed by running two
  `cargo test` processes concurrently against the pre-fix tree, which
  reliably reproduced the 2026-08-07 entry's exact failure signature
  (`RegistryUnsupported`/`"registry lookup is only supported on
  Windows"` panics) along with other rotating spurious failures from
  tests silently getting the WRONG config (e.g. missing `modules`/
  `can_channel_mode` entries belonging to the other process's test).
  Fixed by making the temp config path per-process-unique (via
  `std::process::id()`) at all four call sites sharing this pattern
  across the workspace: `tests/grpc_mock/harness.rs`'s
  `TestServer::try_start_with_extra_config` (this file, alongside the
  existing `VCI_CONFIG_STARTUP_LOCK`), `tests/live_grpc_flow.rs`'s
  `resolve_library_name`, and `iso22900-service`'s
  `tests/grpc_mock.rs::TestServer::start` and
  `src/service/rpc.rs::set_mock_library_path_config`. This is a
  test-infrastructure-only change; no production code touched. The
  `tests/grpc_mock/j1939.rs` "full-module flakiness (round 15, not yet
  root-caused)" entry above is a plausible but *unconfirmed* candidate
  for this same mechanism — left as-is, not claimed resolved by this fix.

#### Resolved (2026-07-28)

Both entries below were run down to a specific, confirmed test-side cause
(not a production race) via dynamic reproduction (isolated repeats,
temporary production-code instrumentation later fully reverted) plus a
static read-through of the relevant production mechanism:

- `tests/grpc_mock/stopcomm_data_tx.rs::stopcomm_disconnect_then_reconnect_same_channel_suppresses_stale_final_transmit`
  (originally observed 2026-07-24, ADR-123 Codex-review round-3
  verification pass — failed once in 3 full parallel `cargo test -p
  j2534-0404-service` runs): the production guard this test exercises
  (`handle_stop_comm`'s post-`wait_for_p3_gap` `still_on_this_channel`
  re-check, `events.rs`) was confirmed correct by direct inspection — it
  re-reads `connect_generation` under the same `logical_links` critical
  section as the transmit it guards, a sound check-then-act with no TOCTOU
  window. The flake was purely a test-side timing-margin defect, the same
  class already fixed in the 2026-07-22 entries below: the test seeds a
  300ms `CP_P3Phys` gap, sleeps 150ms (leaving only 150ms of margin), then
  does a disconnect **and** a reconnect — two sequential gRPC round trips
  — before the gap deadline elapses. The sibling test
  `stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_final_transmit`
  uses the identical 150/300ms margin successfully because it does only
  ONE round trip (disconnect only); this test's extra reconnect call
  roughly doubles the round-trip time that has to fit inside the same
  150ms window, which round-trip overhead measured elsewhere in this file
  (up to ~85ms per round trip) can exceed under load. Fixed by widening
  `CP_P3Phys` to 800ms (keeping the initial 150ms sleep, so the remaining
  margin grows from 150ms to 650ms) and the post-reconnect settle sleep
  from 250ms to 700ms. Verified: 5/5 isolated reruns passed (~1.54s each,
  consistent), plus a full `grpc_mock` suite run and the untouched sibling
  test, both green.

- `tests/grpc_mock/cop_ctrl_cycles.rs::receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running`
  (originally observed 2026-07-28, CI on PR #3, after the
  queue-policy-field-ownership refactor, ADR-140 — failed once on CI; a
  50-run isolated rerun separately measured a pre-existing ~4% flake rate
  unrelated to that refactor). An initial attempt to fix this the same way
  as the entry above (widening the wall-clock margins 6x) instead made the
  test fail **deterministically** (5/5), which disproved the margin theory
  and forced a real investigation: temporary instrumentation (`eprintln!`
  in both the test and `reap_expired_cyclic_registrants`/`poll_rx_inner`,
  since fully reverted — `git diff` against `src/` is clean) showed cop_a's
  own registrant never satisfied `is_cyclic_reap_sound` during the run, so
  the "cop_a must not be reaped" assertion was never actually observing
  cop_a's own state. The real bug: this test's `is_finished` predicate was
  unfiltered by `cop_handle` (matches ANY cop's `PduCopstFinished`), and
  `set_unique_resp_table_and_promote` (called before `subscribe`) issues
  its own `CoptUpdateparam` COP that executes and finishes essentially
  immediately — its terminal events sit queued in the CLL's own event
  buffer (populated regardless of subscriber presence) from before
  `subscribe` is ever called, and are replayed as the first events any new
  subscription reads. Delivering that backlog over the gRPC stream measured
  consistently at ~40ms in this environment, so the original 40ms
  checkpoint was unknowingly racing that unrelated COP's own stale terminal
  event, not cop_a's — explaining the historical failure and the 4% flake
  rate as a race against backlog-replay latency, not against the
  exhaustive-drain timing the test is meant to exercise. Fixed by filtering
  every `is_finished` check in this test to cop_a's own `cop_handle`,
  matching the pattern the neighbouring
  `cyclic_reap_defers_until_the_uudt_companion_channels_watermark_also_catches_up`
  test already uses (`is_finished_for_cop_a`). The wall-clock margins were
  also kept widened (6x: 100ms deadline, ~300ms drain window, 200ms/50ms
  checkpoints) as an independent, additional safety margin against the
  original tight-tick-margin concern, now that the predicate bug no longer
  masks whether that margin was ever the deciding factor. Verified: 5/5
  isolated reruns passed (~0.52s each), plus a full `grpc_mock` suite run
  and the companion test, both green.

#### Resolved (2026-07-22)

Three timing-window tests around idle/software tester-present dispatch in
`tests/grpc_mock/tester_present_send_type.rs` were previously listed here.
All three were run down to a specific, confirmed cause (not just "insufficient
margin, unconfirmed") via dynamic reproduction (isolated repeats, full-package
runs under simulated CPU load, and full-*workspace* `cargo test --workspace`
runs with the machine deliberately oversubscribed) plus a static read-through
of the production dispatch code (`events.rs`'s `dispatch_due_tester_present`,
`tester_present_due_reference`, the ADR-093 `TesterPresentState::Armed`
unification, and every `last_bus_activity` write site) confirming no
production race exists — each test's flakiness was a test-side defect:

- `send_type_1_idle_timer_resets_on_prior_bus_traffic`: chained blind
  `tokio::time::sleep(40ms)`/`sleep(35ms)` calls against an 80ms interval left
  only ~5ms of margin before the original arm-to-deadline fire, which ordinary
  RPC/event round-trip overhead (measured elsewhere in this file at up to
  ~85ms) reliably ate into (~27% failure rate, 4/15 isolated runs). Fixed by
  switching to an anchored `tokio::time::sleep_until` deadline pattern (300ms
  interval, reset at 150ms, check at 380ms, 225ms final threshold) mirroring
  `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`'s
  already-stable structure. Verified stable across 40/40 isolated runs and 3
  consecutive full-suite runs (356/356 tests each) under simulated CPU load.
- `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`:
  its one historical failure (2026-07-16, during ADR-093's implementation)
  occurred on a since-superseded test body that had the identical ~5ms-margin
  defect above (confirmed via `git show` of the commit at that time); the
  current body already uses the anchored `sleep_until` pattern and has zero
  reproductions across all dynamic verification, including full-workspace
  runs under heavy CPU oversubscription. No code change needed; the entry
  described a defect that had already been fixed by the time it was written
  down.
- `send_type_1_no_periodic_start_and_fires_one_shot_after_idle`: had a real
  structural race, distinct from the two above — its first assertion
  (`written_count(MOCK_CHANNEL_ID) == 1`, checked right after
  `wait_for_cop_finished`) compared a count against a 50ms idle interval, but
  the client only learns the immediate ADR-084 send happened via an
  event-propagation/gRPC round trip that can itself exceed 50ms (measured
  elsewhere in this file at up to ~85ms) — so the second, idle-triggered
  frame could legitimately have already landed by the time the `== 1` check
  ran. First attempted fix relaxed that check to `>= 1`, but a Codex review
  correctly flagged that this silently defeated the check's actual purpose:
  a regression where `CoptStartcomm` stops sending synchronously (only
  sending on the idle timer instead) would, once `wait_for_cop_finished`
  happened to take longer than the interval, still pass `>= 1` (and the
  payload check) on that delayed frame, no longer distinguishing it from a
  genuine arm-time send. Corrected fix instead widens `CP_TesterPresentTime`
  itself (50ms → 300ms, matching the other two entries' interval) so `== 1`
  has enough margin over the ~85ms of round-trip overhead to stay meaningful
  without racing it; the elapsed bounds were rescaled to match (225ms lower /
  1500ms upper — the upper bound deliberately kept well below the preset's
  un-overridden 2s default, so a "the override didn't take effect"
  regression is still caught). Verified stable across 25/25 isolated runs
  and a full module regression pass (39/39) after the correction.

### Prioritized Backlog

- **P2**: Make `grpc_mock` pass under parallel load on Windows, so CI's `core-windows` job can run
  tests in parallel like Linux instead of one at a time (`ci-windows` nextest profile; serial
  `grpc_mock` alone takes about 400 s there, billed at the Windows rate). With nextest on
  the Windows runner, these failed at 16 test threads (2 runs): `cop_ctrl_cycles::{rc21_chunked_retry_sleep_does_not_prematurely_reap_a_siblings_cyclic_timeout,
  rc21_retry_arm_iteration_defers_cyclic_reap_to_the_next_normal_iteration,
  receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running,
  tier1_wait_recheck_survives_many_deadline_vs_companion_watermark_races_in_one_wait}`,
  `j1939::sibling_cll_skips_an_address_a_live_sibling_already_claimed`,
  `p3_gap::{tester_present_reqrsp_0_due_sends_are_func_gapped_from_each_other,
  tester_present_reqrsp_live_flip_takes_effect_on_next_due_send}`,
  `rc_handling::p2_star_reloads_deadline_on_every_078_occurrence`,
  `stopcomm_data_tx::stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_final_transmit`,
  `tester_present_send_type::{mode_0_own_send_does_not_stamp_last_bus_activity_for_mode_1_sibling,
  send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send,
  send_type_1_sibling_cll_arming_does_not_reset_another_clls_idle_window,
  send_type_1_sibling_fires_during_a_long_copt_delay_not_after_it}`; at 4 threads the two
  `p3_gap` tests still failed, and the run (247 s) was barely faster. Each needs its timing
  dependence found and removed (ADR-149), as was done for the StopComm queue tests that held the
  queue with a `CoptDelay` on their own CLL. Done when the Windows job runs
  tests in parallel and is green.
- **P2**: `j2534-0404-service` lib tests set process-global error injections in the dynamically-loaded mock (`__mock_set_stop_periodic_message_error` in `src/service/rpc_misc.rs`, `rpc_module.rs`, `rpc_primitive.rs`; `__mock_set_stop_filter_error`; `__mock_set_stop_repeat_message_error`). Under `cargo test` (several test threads, one process) any untagged test that issues the same native call while one of these is armed gets the injected error. The setters of `stop_periodic_message_error` share `#[serial(tp20_stop_periodic_call_counter)]`, but nothing keeps untagged callers of `PassThruStopPeriodicMsg` out of that window; the other two setters were not audited. Not observed failing (13 parallel `--test-threads=16` lib runs passed); found while fixing the exact-delta call-counter flakes, which moved to the mock's per-thread counters (`j2534-0404-mock/docs/testing-guide.md`, "Call Counters"). nextest (CI) runs each test in its own process and is unaffected. Likely fix: make the injections per-thread like those counters, after checking that every setter's native call runs on the setting test's thread. Done when: no lib test can observe another test's injected error, and the serial group is renamed or removed to match what it still protects.
- **P2**: `j2534-0404-service` calls the vendor J2534 library synchronously from async tasks on the multi-thread runtime (`src/main.rs` `#[tokio::main]`; e.g. `events.rs::transmit_request_locked` -> `j2534_0404::J2534Api0404::write_messages` under `ctx.api.lock().await`). Only `IOCTL_BECOME_MASTER` is moved off the worker with `spawn_blocking` (`rpc_misc.rs::ioctl_become_master`, ADR-189). A vendor call that blocks (a slow or hung DLL) holds a runtime worker, and can also stall the runtime's timer/IO driver: in a test prototype that blocked `PassThruWriteMsgs` inside the mock on a `multi_thread` `#[tokio::test]` (4 workers), the other workers sat idle and a 10 ms `tokio::time::sleep` in the test never woke (gdb thread dump); the same hold also cannot be used in the `grpc_mock` suite for that reason, which keeps timing-dependent tests such as `p3_gap.rs::tester_present_reqrsp_due_snapshot_reads_live_not_frozen_resolved` on wall-clock margins. Real J2534 calls are expected to return promptly (timeouts of 0), so this is a robustness risk, not an observed failure. Done when: a decision (ADR) says which native calls may block and how the service isolates them (`spawn_blocking`, `block_in_place`, or a dedicated FFI thread), and a test shows a held native call no longer stalls unrelated RPCs or timers.
- **P2**: Re-verify the provisional N_WFTmax reading in ADR-124 (Context and Consequences) and ADR-121 (Consequences) against ISO 15765-2 (2024 edition) and ISO 22900-2:2022, both now in `vehicle-comm-specs`; both ADRs say they were written without the normative text. Check the reading of `CP_CanMaxNumWaitFrames` / N_WFTmax (ISO 15765-2:2024 clauses 9.6.5.1, 9.7 and 9.8.4, where the parameter is named CTL_WFTmax; ISO 22900-2:2022 clause B.5.2, Table B.21): the direction verdict (receiver self-limit, not sender tolerance) and whether the software TX path in `events.rs` (`isotp_send`) should enforce the ComParam. Done when: the reading is checked against the clauses, and the ADRs are either confirmed (their caveat replaced by the clause citations) or superseded by a new ADR with the code changed to match.
- **P2**: Re-verify the provisional CAN FD functional Single Frame payload limit in ADR-169 against ISO 15765-2 (2024 edition), now in `vehicle-comm-specs`; the ADR says it was written without the normative text. Check the limit that `fd_max_sf_payload` applies (ISO 15765-2:2024 clause 9.6.2, Tables 11 and 12, and 9.6.2.2), which decides which functional CAN FD requests are accepted. Done when: the limit is checked against the clauses, and ADR-169 is either confirmed (its caveat replaced by the clause citations) or superseded by a new ADR with the code changed to match.
- **P2**: Check the P2*client reload behaviour cited from ISO 14229-2 clause 7.3 in ADR-018 and ADR-102 (the reload on every response-pending NRC 0x78) against ISO 14229-2 (2021 edition), now in `vehicle-comm-specs`; the ADRs were written without the text available. Also used by `docs/j2534-0404-architecture.md` and `crates/j2534-0404-service/docs/comparam-protocol-support.md`. Done when: the citation is confirmed or corrected, and any behaviour that differs from the clause gets an ADR and a code fix.
- **P2**: Check the 5 s P2*server_max default cited from ISO 14229-2 clause 7.3 in ADR-018 and ADR-102 against ISO 14229-2 (2021 edition), now in `vehicle-comm-specs`; the ADRs were written without the text available. Also used by `docs/j2534-0404-architecture.md` and `crates/j2534-0404-service/docs/comparam-protocol-support.md`. Done when: the default is confirmed or corrected, and any value that differs from the clause gets an ADR and a code fix.
- **P3** (`design-advisor` audit, ADR-222 round 4, PR #141 — pre-existing, not introduced or
  worsened by that ADR): if a dual-channel-mode CLL's own ADR-046 companion channel fails to open
  (or the FD-under-DualChannel case), `LogicalLinkState::uudt_channel_id` stays `None` while
  `rpc_link.rs:5141`'s own filter-installation logic still skips installing a UUDT
  `PASS_FILTER`/`FLOW_CONTROL_FILTER` on the primary channel (an ADR-046/ADR-158 gap). ADR-222's own
  `uudt_on_companion` field (`RxEntryKind::Hardware`, `l.uudt_channel_id.is_some()`) reads `false`
  in this state, so `matched`'s UUDT tier stays live on the primary — but since no UUDT filter was
  ever installed there either, no real UUDT-tagged traffic can reach it regardless; the tier being
  live is dead weight, not a live misattribution risk. Not investigated further or fixed here —
  worth confirming (and, if `uudt_on_companion` should instead track "dual-channel mode was
  intended" rather than "the companion channel is currently open", fixing) the next time this
  companion-open-failure path is touched.
- **P2** (Codex review finding, ADR-222 round 6, PR #141): CAN-ID-width contention detection
  (`build_cll_rx_entries`, `events_rx_routing.rs`) treats a table entry's USDT/UUDT field as a
  genuine width observation whenever `point_to_point_filter_eligibility` says its ComParams WOULD
  cause `install_point_to_point_fc_filters` to attempt a filter install — not whether that specific
  attempt actually succeeded. A real install failure (`install_point_to_point_fc_filter` returns
  `Err`, e.g. the adapter has exhausted its own filter resources) is logged and the entry's filter
  id is simply omitted from the returned `Vec<MessageFilterId>`; `LogicalLinkState::
  unique_resp_filter_ids` stores only that flat, unkeyed list for later removal, with no per-entry/
  per-field record of which install actually succeeded. A contended id where one eligible sibling's
  filter failed to install while another's succeeded is still treated as genuinely contended, giving
  the surviving entry a strict width gate that can drop its own frames on an adapter that ALSO
  omits/misreports `RxStatus` bit 8 — a double adapter-side fault (filter-install failure AND
  bit-8 unreliability) that pre-ADR-222 numeric-only matching could not regress on. Not fixed —
  see ADR-222's own Consequences section for the full reasoning and why a proper fix needs new
  per-entry/per-field install-success tracking threaded from connect time into the per-poll-pass
  contention computation, not attempted here as disproportionate to how rarely this compounds.
- **P3** (found while writing `examples/grpc_tp2_0.rs`, stage 3 of the per-protocol gRPC example
  coverage effort): TP2.0's five mandatory active-connection ComParams (`CP_TP20ChannelSetupCanId`/
  `CP_TP20DestinationAddress`/`CP_TP20TxIdProposal`/`CP_TP20RxIdProposal`/`CP_TP20ApplicationType`,
  `service_params.rs`'s `PARAM_TP20_*` constants, `0x80C9`-`0x80CD`) have NO `GetObjectId`/name-based
  resolution at all -- confirmed by grepping `names.rs`'s `map_comparam_name_native`/
  `_service_timing`/`_transport`/`_physical` functions for any `"cp_tp20*"` entry (none exists,
  unlike every other minted ComParam in this codebase, e.g. `CP_AnalogSampleRate`/
  `CP_NdisPinOption`, both of which ARE registered). `tests/grpc_mock/tp20.rs` already works around
  this by addressing them via raw numeric `ComParamId`s rather than `GetObjectId`/`ParamName`;
  `examples/grpc_tp2_0.rs` does the same, with a doc comment explaining why. Not fixed here (a
  purely additive `names.rs` change, out of scope for an examples-only change) -- add the five
  `"cp_tp20*"` entries to `map_comparam_name_transport` (or wherever the other minted-ComParam
  entries live) the next time `names.rs`/TP2.0 ComParam mapping is touched.
- **P3** (`docs/j2534-2-support-plan.md` §4 design question 1, never formally decided by ADR-152
  or any later phase — Codex review, PR #114): `config.rs`'s startup parsing has no `"J2534-2:"`
  prefix validation for a module's `pname` — any string is accepted as-is. Two open sub-questions:
  whether to validate the prefix format at config-load time, and how a `pname` that doesn't start
  with `"J2534-2:"` should be treated now that SAE J2534-2 clause 5's "no assumption can be made"
  case applies to it. Non-blocking — every shipped J2534-2 phase's own `pname` opt-in reads
  correctly regardless.
  Done when: the decision is recorded in `docs/j2534-2-support-plan.md` or an ADR and `config.rs`
  follows it, with a test for a `pname` without the prefix. Blocked on: the maintainer's decision on
  whether `config.rs` validates the `"J2534-2:"` `pname` prefix at load time and how a `pname`
  without it is treated (`docs/j2534-2-support-plan.md` section 4 design question 1, never decided
  by ADR-152).
- **P3** (`edge-case-hunter` finding, PR #106 round 2 verification pass, test-coverage gap, not a
  live defect): `ioctl_clear_tx_queue`'s regression test
  (`ioctl_clear_tx_queue_skips_the_native_call_for_a_hard_errored_cll`, `rpc_misc.rs`) builds its
  `LogicalLinkState` already `connected: false` before the RPC call starts, so it cannot
  distinguish the round-2 fix (re-reading `channel_key`/`connected` fresh, immediately before
  resolving `channel_id`, under a continuously-held `shared_channels` guard -- closing a Codex
  review finding that the round-1 fix's `connected` check used a snapshot taken at function
  entry, before several `.await` points, rather than the current state) from the round-1 bug it
  replaces: confirmed empirically (`edge-case-hunter`, PR #106 round 2) that this same test still
  passes against the reverted, pre-round-2 code. A genuinely discriminating test needs a state
  mutation to land between the function's entry and its `shared_channels` acquisition -- this
  crate has no test-only synchronization hook to force that interleaving deterministically, the
  same class of infeasibility this file's own backlog and "Known Flaky Tests" section document
  repeatedly for comparable races elsewhere (e.g. this section's own `PR #97`/`PR #101`
  test-coverage-gap entries). Not fixed here; the fix's own correctness rests on code review
  (two independent `edge-case-hunter` passes) and reuse of `resolve_live_legacy_link`'s
  already-established "resolve entirely under one continuously-held `shared_channels` guard"
  discipline, not on this test alone.
- **P3** (`edge-case-hunter` finding, PR #104 close-out, maintainability observation, not a defect):
  `names.rs`'s `parse_protocol_id_from_resource` gates the table-row-matched
  (`RscData::ProtocolName`/bare `ResourceName`) route's opt-in check via a hand-copied,
  protocol-specific `matched_row_is_*` boolean (`matched_row_is_ethernet_ndis`,
  `matched_row_is_analog_in`) added to the branch condition alongside `matched_row_needs_pin_
  selection`, one per protocol excluded from `row_needs_dynamic_pin_selection`. Unlike `row_needs_
  dynamic_pin_selection` itself (whose own doc comment notes it is shared by two call sites
  specifically so they cannot drift apart), nothing structurally forces a future J2534-2 protocol
  added to that same exclusion list to also get a `matched_row_is_*` exception here -- this exact
  bug class was independently rediscovered and fixed twice already (Ethernet_NDIS, PR #102;
  Analog Inputs, PR #104), evidence the pattern itself is fragile. Not fixed here: a structural fix
  (e.g. a single `row_is_pinless_j2534_2_feature`-style predicate, or restructuring so the
  table-row-matched branch always funnels through `resolve_pin_selection`'s opt-in gate regardless
  of protocol identity) would be cheaper than a third one-off patch, but is a refactor beyond a
  single-bug-fix PR's scope; revisit if a third protocol needs the same treatment.
- **P3** (ADR-195, design-advisor investigation, accepted residual, not fixed here): while ruling
  out the poll-task-shutdown-ordering hypothesis during the ADR-195 cross-process race
  investigation (see "Resolved (2026-08-27)" above), the mock cdylib's `spawn_repeat_worker`
  (`j2534-0404-mock/src/lib.rs`, around line 1250) was found to have no protection against the
  cdylib itself being `dlclose`'d while its detached, `std::thread::spawn`-launched repeat-message
  worker thread is still running — plausibly consistent with a prior one-off, never-reproduced
  SIGSEGV report already noted in this file's "Known Flaky Tests" section (see the
  `discovery.rs`'s `discovery_mock_call_counter` serial group entry, "PR #101 round 10,
  `cargo-runner` triage after an initial SIGSEGV report that itself did not reproduce across 5+
  attempts"). Not fixed here since it is speculative and unreproduced; worth tracking if a similar
  crash is ever seen again.
- **P3** (Codex review fix, P1, PR #101, round 6, test-coverage gap): `J2534Service::terminate_tp20_
  broadcast_periodic_for_suspension` now takes `channel_id: Option<ChannelId>` as a parameter,
  captured by each of its four callers from the SAME `LogicalLinkState` reference they take
  `periodic` from -- fixing a bug where a fresh `self.logical_links` lookup at call time could
  race a disconnect/reconnect on `cll_handle`, silently skipping the native stop (`None`) or
  targeting the wrong channel (a reconnect). The fix itself is verified by direct code reading
  (all four call sites read `link.channel_id`/`l.channel_id` in the identical critical section as
  their own `take()`, confirmed by the compiler forcing every call site to supply the new
  parameter) and is not itself in doubt, but no automated regression test exists for it: this
  crate's lib unit tests load the mock J2534 library via `J2534Api0404::from_path` (a genuine
  `dlopen`), which is a SEPARATE compiled instance from this crate's own statically-linked
  `j2534_0404_mock` dev-dependency -- its exported call-count/state backdoors (`mock_get_stop_
  periodic_count`, `mock_get_periodic_msg_count`, etc.) observe only the statically-linked
  instance, never calls made through `self.api`, so they cannot verify this fix's effect from a
  lib unit test. The mock's own `PassThruStopPeriodicMsg` also returns success unconditionally
  for an already-stopped or unknown id (no `ERR_INVALID_MSG_ID` on a redundant stop), so a second
  manual stop call cannot distinguish "already stopped by the fix" from "never stopped" either.
  Proper coverage needs either a `tests/grpc_mock` integration test (whose harness opens its own
  `libloading::Library` handle to the SAME `.so` path specifically to work around this) driving a
  real disconnect-races-suspension scenario, or a new mock backdoor that reports whether a given
  id was ever actually removed (not just whether the call returned success) -- out of scope for
  this fix.
- **P3** (Codex review round 15 Fix 2 follow-up, PR #101, ADR-193, `edge-case-hunter` finding,
  accepted test-coverage gap): the fence's STRICT commit ordering — the real `message_id` is
  written into `logical_links` BEFORE `rpc_start_com_primitive` drops `self.api`, not after — is
  asserted only by a direct call to `finalize_or_orphan_broadcast_periodic_start_locked`
  (`rpc_primitive.rs::tp20_broadcast_periodic_api_fence_tests::
  commit_happens_before_the_api_guard_is_released`), never through the real RPC. Reverting the
  production caller to run the resolution after `drop(api)` therefore still passes the suite.
  Observing the difference needs a task to run inside the guard's own window: this crate has no
  test-only hook to pause a native call mid-flight, and on the `current_thread` test runtime a
  task queued on the same mutex is woken on release but not polled until the releasing task
  yields, which it does not do before committing. Same class of infeasibility
  `rollback_stop_comm_pending_tests` documents. Available substitutes already in place: the
  direct test above, plus the four production-path fence tests in the same module (see this
  file's round-15 Fix 2 test notes). Closing it properly needs either a test-only yield hook
  around the native start or a multi-threaded-runtime harness with deterministic handoff.
- **P3** (Codex review fix, P2, PR #101, ADR-192/Phase 7 Stage 7c Fix 1, accepted residual, not
  fixed): `rpc_start_com_primitive`'s own post-native-call completion
  (`J2534Service::finalize_or_orphan_broadcast_periodic_start_locked`, since ADR-193 running
  under the same `self.api` guard as the native start itself) issues a best-effort
  `stop_periodic_message` for a broadcast-periodic COP whose `None`-sentinel
  reservation was taken or cleared by a concurrent `CoptCancel`/`DisconnectComLogicalLink`/
  `DestroyComLogicalLink`/a TX-dispatch-suspension-triggered termination/`CLEAR_PERIODIC_MSGS`
  WHILE its native `PassThruStartPeriodicMsg` call was still in flight (round 4 Fix 1), rather than
  leaving it untracked with nothing pointing to it anywhere -- a still-in-flight `None`-sentinel
  reservation is still taken/finalized by every racing site listed above, EXCEPT
  `CLEAR_PERIODIC_MSGS`, which (as of the sentinel deferral fix, edge-case-hunter finding,
  design-advisor-approved, same ADR-192 Decision item 2) now defers instead of taking: it stamps
  `Tp20BroadcastPeriodic::pending_clear_generation` and leaves the sentinel tracked, letting
  `finalize_or_orphan_broadcast_periodic_start_locked` resolve it once `started_epoch` is known.
  (As of ADR-193, including its round-16 amendment (Codex review, P1, PR #101) that fenced the
  hard-error dead-channel sweep the same way, every reservation-taking site listed above --
  including the hard-error sweep -- takes it only under the `self.api` fence, so most of those
  interleavings now resolve as a real committed id rather than reaching this orphan path at all;
  the orphan path remains reachable via `DestroyComLogicalLink`'s early map removal and the
  hard-error sweep.) The
  residual this leaves open is narrower: if THAT best-effort stop call itself fails, the message is
  a genuine double-fault leak
  with nothing tracking it anywhere -- this code path has no `shared_channels` lock in scope at
  that point to push the id onto a channel's `leaked_periodic_message_ids` the way every other
  stop-failure site in this mechanism does, and adding that lock-order dependency here was judged
  out of scope for this fix. This crate still has no test-only synchronization hook to
  deterministically reproduce the in-flight-native-call interleaving itself (the same class of gap
  several `PR #97` entries below cite for their own concurrency fixes).
- **P3** (`edge-case-hunter` finding, PR #101 round 10 verification, mitigated here, residual
  accepted): `restore_or_leak_track_broadcast_periodic`'s leak-track fallback (round 10's
  generation-mismatch case, and, as of round 13, also the same-session-with-slot-reclaimed case --
  both now share this one fallback, see ADR-192 Decision item 2's round-13 update) gates onto the
  live `SharedChannel`'s `channel_id` still matching the captured one before pushing onto
  `leaked_periodic_message_ids`, closing the practical case where a channel close followed by an
  unrelated fresh connection at the same `ChannelKey` would otherwise misattribute the leak onto a
  channel that never ran the failed message. This is not airtight:
  `next_connect_generation`'s own doc comment (`service.rs`) already notes a `channel_id` can
  coincidentally repeat on a shared channel, so a real adapter that reissues the same numeric
  handle after a close+reopen could still slip through this gate. A fully airtight check would
  compare `SharedChannel::occupancy_epoch` (ADR-161) the way `ioctl_reset`'s Phase 1 does, but that
  field is only ever read/stamped while holding `shared_channels`, and capturing it at the same
  time as `periodic`/`channel_id`/`connect_generation` (currently captured while only
  `logical_links` is held, at all 4 `terminate_tp20_broadcast_periodic_for_suspension` call sites)
  would require acquiring `shared_channels` before `logical_links` at each of those 4 sites --
  itself performance-sensitive code (the suspend ioctl, error-suspension paths, and two poll-loop
  deadline hooks). Judged a larger lock-ordering restructuring than warranted for this residual's
  severity; a `design-advisor` consult is the right next step if this is pursued, not a unilateral
  change to lock ordering across 4 call sites.
- **P3** (found by `edge-case-hunter`'s round-26 pass, PR #97, open spec question, not fixed
  here): a client's own explicit `SetMsgFilter`/`PDU_IOCTL_START_MSG_FILTER` against a connected
  TP2.0 CLL has no protocol gate at all in `ioctl_start_msg_filter` (`rpc_misc.rs`) -- that
  function already has a service-level rejection PAIR to mirror (ISO15765's `PduErrValueNotSupported`
  and, right after it, Analog Input's own `PduErrIdNotSupported`, both returned before any native
  call), but no third arm for TP2.0. The mock's own `PassThruStartMsgFilter` handler also only
  special-cases Analog Input, not TP2.0, so a client-driven filter request against a TP2.0 CLL
  would additionally succeed on the mock even if the service-level gate were bypassed. Whether
  clause 19 forbids ALL filters against a TP2.0 channel (the same blanket rejection Analog Input
  already gets) or only this service's own automatic pass-all baseline (round 26's Fix BB fixes
  only the latter, at the connect-time and `CLEAR_MSG_FILTERS` call sites) is a spec-interpretation
  question ADR-188 §1 does not explicitly settle either way. Pre-existing before round 26, not
  widened in blast radius by it. Needs a `design-advisor` consult if pursued, not a unilateral
  guess.
  Done when: the reading is recorded in an ADR (or an ADR-188 amendment) and
  `ioctl_start_msg_filter` and the mock follow it for TP2.0, with a test. Blocked on: the maintainer's
  decision on whether SAE J2534-2 clause 19 forbids all filters on a TP2.0 channel or only the
  service's automatic pass-all filter (ADR-188 section 1 does not settle it).
- **P3** (Codex review fix, PR #97, ADR-188 Fix Z, 23rd round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end regression test proves `reconcile_established_tp20_loss`'s own round-23
  race fix -- that a concurrent `CoptStopcomm`/`Disconnect`/`Destroy` winning the race against this
  function's scan (taking the CLL's `tp20_connection` first and inserting an `abandoned` quarantine
  entry) gets that entry released by this SAME `Lost` indication's own no-live-match fallback path.
  Reproducing this needs deterministic control over task scheduling at a specific point inside two
  concurrently-running async functions (this function's own `logical_links` scan vs. a teardown
  call's own `.take()`), which this crate has no test-only synchronization hook for. Not attempted
  here as disproportionate to this fix's own scope; the fix's own correctness (acquiring
  `shared_channels` first and holding it for the whole function, making the scan-then-flip one
  unbroken step, and releasing any `abandoned` entry found when the scan finds no live match) rests
  on code review and reuses the same outermost-lock discipline `handle_stop_comm`/
  `DestroyComLogicalLink`/`DisconnectComLogicalLink` already use for their own teardown+quarantine
  sequences.
- **P3** (Codex review fix, PR #97, ADR-188 Fix W, 18th round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end regression test proves `rpc_misc.rs::ioctl_start_repeat_message`'s own
  duplicate `TX_EXTENDED_ID` derivation (mirroring `apply_resolved_tx_flags`'s fix, immediately
  after its own J1939 branch) actually reaches the wire on the actually-transmitted
  `RepeatMsgData[0]` message. The mock does not validate TX-flag/CAN-ID-width consistency for
  TP2.0 writes at all (unlike its J1939-oriented `RX_FLAG_CAN_29BIT_ID` bookkeeping), so no
  end-to-end assertion can distinguish a correctly-flagged repeat-message transmission from an
  incorrectly-flagged one via this harness. Not attempted here as disproportionate to this fix's
  own scope; the fix's own correctness rests on code review and mirrors the already-unit-tested
  `resolve_send_recv_tx_tp20_extended_id_tests` derivation exactly (`rpc_primitive.rs`).
- **P3** (Codex review fix, PR #97, ADR-188 Fix T, 16th round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end regression test proves Fix Q's own `already_started` rejection branch
  (`handle_start_comm`'s TP2.0 arm) correctly reverts a racing `temp_param_update = 1` request's
  own hardware changes via `revert_hardware_to_live_active`. Constructing this exact interaction
  (the round-13 concurrent-`CoptStartcomm` race, plus `temp_param_update = 1` on the losing
  attempt, plus an observable hardware-state check proving the revert ran) compounds two
  already-narrow-window regressions into one test; the call itself reuses an already-thoroughly-
  exercised primitive called identically by three other arms in the same function, each
  independently covered by this codebase's existing `temp_param_update` test suite for their own
  protocols.
- **P3** (Codex review fix, PR #97, ADR-188 Fix R, 14th round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end regression test proves `DisconnectComLogicalLink`'s or
  `DestroyComLogicalLink`'s own new `quarantine_tp20_connection_for_orphaned_write_back` call
  (`rpc_link.rs`) actually blocks an immediate same-`rx_id` retry the way `stopcomm_quarantines_
  its_rx_id_against_an_immediate_retry` proves for `CoptStopcomm`. Both call sites reuse the
  identical, already-tested primitive and mirror `handle_stop_comm`'s own call shape exactly; a
  `Disconnect`/`Destroy`-triggered equivalent construction (establish, arm `__mock_set_tp20_no_
  indication`, disconnect/destroy the CLL, immediately reconnect a new one proposing the same
  `rx_id`) would need its own per-call-site harness work not attempted here.
- **P3** (Codex review fix, PR #97, ADR-188 Fix Q, 13th round, flagged for a future look, not
  investigated further): the same design shape Fix Q closed for TP2.0's own "fresh attempt" block
  (`comm_started` set only at the very end of `handle_start_comm`'s overall processing, checked
  only at RPC-accept time in `rpc_primitive.rs`, never rechecked at dispatch time before a
  protocol-specific arm overwrites its own connection/claim state) plausibly affects the SAE J1939
  arm's own analogous "fresh attempt" reset in `events.rs` (the block Fix Q's own comment cites as
  "mirroring the J1939 branch's own 'fresh attempt' reset above") the identical way: two concurrent
  `CoptStartcomm` RPCs on the same CLL could race past `comm_started`, both queue as separate
  `TxItem::StartComm` entries, and the second dispatch could clobber the first's own successfully
  claimed J1939 address state. Not investigated or fixed here -- out of scope for the TP2.0-specific
  finding that surfaced this class of bug -- but worth a dedicated look before assuming J1939 is
  safe by omission.
- **P3** (Codex review fix, PR #97, ADR-188 Fix O, 11th round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end (or mock-level) regression test can prove `deliver_tp20_connection_
  indication`'s new re-teardown-on-late-`Established` behavior actually issues a second native
  `IOCTL_TEARDOWN_CONNECTION` call. Every reachable abandonment path's own FIRST `best_effort_
  teardown_on_abandon` call already synchronously frees the mock's real `tp20_connections` slot for
  `rx_id` (the mock has no "device still processing the request" window to hold a genuine slot open
  past that first teardown call, and this crate has no `IOCTL_TEARDOWN_CONNECTION` call-count
  backdoor either -- `j1939_cancel_error`'s doc comment in `j2534-0404-mock/src/lib.rs` is the
  nearest existing precedent for the kind of force-this-call-to-fail backdoor that would let a test
  hold a slot open long enough to observe a genuine second removal). Closing this would need a new
  narrow mock backdoor mirroring that shape; not attempted here as disproportionate to this fix's
  own scope. The fix's own correctness (re-teardown gated on `outcome`, `Lost` needing no
  follow-up) rests on code review and reuse of the already-tested `best_effort_teardown_on_abandon`
  helper.
- **P3** (ADR-188, Phase 7 Stage 7a, deliberate scope exclusion this stage, not investigated
  further): `run_tp20_connection_request` (`events_tp20_connection.rs`) does not check
  `LogicalLinkState::cancelled_cops`/`stop_comm_pending` mid-wait, unlike the identical mechanism
  `run_j1939_claim_loop` implements for its own claim wait (ADR-180 Decisions 21/24) -- a
  `CancelComPrimitive` or `PDU_COPT_STOPCOMM` racing an in-flight `IOCTL_REQUEST_CONNECTION` wait
  still resolves only once the bounded (~2s) wait completes, rather than aborting early. Not fixed
  this stage -- would need to port the same `(was_cancelled, is_stale, stop_comm_pending)` tuple
  check `run_j1939_claim_loop`'s own per-iteration critical section uses.
- **P3** (concurrency-bug fix to `run_tp20_connection_request`'s pre-insert availability check,
  test-coverage gap, not fixed): no `grpc_mock` end-to-end regression test can exercise
  `tp20_rx_id_unavailable_for`'s (renamed from `tp20_rx_id_owned_by_a_live_sibling`, Fix J, 7th
  round -- see this file's TP2.0 narrative section above) LIVE-SIBLING rejection reason
  (`Tp20ConnectionRequestOutcome::RxIdInUse` via a DIFFERENT, still-PENDING `cll_handle`) through
  this crate's `tests/grpc_mock` harness, for two compounding reasons. First,
  `SharedChannel::tp20_connections` removes an attempt's own entry as soon as it resolves either
  way (unlike J1939's `j1939_claims`, which retains a successful claim for the CLL's whole session)
  -- so the live-sibling check can only ever observe a live sibling's own still-PENDING request,
  never an already-established one; `tp20.rs`'s
  `second_cll_reusing_an_established_siblings_rx_id_is_rejected_and_does_not_disturb_it` collides
  against an already-ESTABLISHED sibling instead (the only reachable end-to-end construction) and
  so does not exercise this decision at all -- that collision is now correctly rejected too (Codex
  review fix, PR #97, round 11, closing the mock-fidelity gap a previous P3 entry here tracked: the
  mock's `IOCTL_REQUEST_CONNECTION` handler now simulates clause 19.3.3.2's native `ERR_NOT_UNIQUE`
  for a duplicate already-established RX-ID), but via that DIFFERENT native-rejection path
  (`Tp20ConnectionRequestOutcome::Failed`), not via this fix's own local `RxIdInUse` check -- so it
  still isn't proof of THIS fix's own scope. Second,
  even the genuinely still-PENDING-vs-PENDING race the live-sibling check targets is itself
  unreachable via any client-driven RPC sequence in this crate's current architecture: each
  physical channel has exactly one poll task (`events::spawn_channel_poll_task`), which dequeues
  and processes every queued `TxItem::StartComm` -- including the entire bounded
  `run_tp20_connection_request` wait -- to completion before it can dequeue the next one, so two
  sibling CLLs sharing one physical channel can never have overlapping "both still pending" windows
  via the ordinary client-driven RPC path this harness drives (the same "not currently reachable,
  kept as explicit defense-in-depth" status `events_j1939_claim.rs`'s own cleanup-gate check
  documents for its analogous race -- see this file's narrative section above and
  `events_tp20_connection.rs`'s own module doc comment). The live-sibling decision's own correctness
  IS unit-tested directly (`events_tp20_connection.rs`'s `#[cfg(test)] mod tests`, exercising
  `tp20_rx_id_unavailable_for` in isolation); closing the end-to-end gap for THIS reason specifically
  still needs decoupling per-CLL `StartComm` processing from the single serialized poll task -- a
  larger architectural change, not attempted here. `Tp20ConnectionRequestOutcome::RxIdInUse` itself
  IS now reachable end-to-end, via `tp20_rx_id_unavailable_for`'s OTHER rejection reason instead
  (an `abandoned`, quarantined `rx_id` -- `tp20.rs`'s `abandoned_connection_request_quarantines_
  its_rx_id_against_an_immediate_retry`, Fix J, 7th round); only the live-sibling reason specifically
  remains untestable end-to-end for the architectural reason above.
- **P3** (Codex review fix, PR #97, ADR-188 Fix B, test-coverage gap, not fixed): no `grpc_mock`
  end-to-end regression test can arm a REAL receive-only monitor on a non-`Established` TP2.0 CLL
  and observe it correctly receive nothing when a sibling's established traffic is injected --
  `tp20.rs`'s `non_established_sibling_does_not_disrupt_an_established_peers_delivery` pins the
  unaffected end-to-end behavior around the same `CoptStopcomm` transition instead, and the routing
  fix itself is unit-tested directly (`events_build_cll_rx_entries_tests.rs`'s
  `tp20_non_established_cll_gets_an_unmatchable_sentinel_entry_not_an_empty_table`). Two compounding
  reasons close every RPC-surface construction: (1) `tx_header::build_tx_message`'s TP2.0 arm (Fix
  A, same PR) rejects `CoptSendrecv` -- including a receive-only monitor's own required TX-header
  resolution -- on ANY non-`Established` TP2.0 CLL, so a monitor can only ever be armed while the
  CLL is still `Established`; (2) `CoptStopcomm` (`rpc_primitive.rs`'s own `cops_to_cancel` "cancel
  all queued primitives for this link" sweep) unconditionally cancels every OTHER COP on the CLL,
  including an already-detached tier-2 (`RegistrantTier::ReceiveOnly`) receive-only registrant --
  unlike `PDU_IOCTL_CLEAR_TX_QUEUE` (`rpc_misc.rs::ioctl_clear_tx_queue`), which explicitly excludes
  a live tier-2 registrant from its own equivalent sweep via `detached_tier2_cops`. So the one RPC
  that can move a TP2.0 CLL out of `Established` without a full disconnect (`CoptStopcomm`) always
  takes any registrant armed on it down too. Closing this would need either widening
  `CoptStopcomm`'s own cancel sweep to exclude a live tier-2 registrant (a real behavior change,
  needing its own design consult -- unclear whether that's even correct for every other protocol
  `CoptStopcomm` already serves) or a new mock-side/backdoor mechanism to force a TP2.0 connection
  spontaneously `Lost` while bypassing the RPC surface entirely; neither attempted here.
- **P3** (Codex review finding, PR #97, ADR-188 Fix J, 7th round, test-coverage residual, not
  fixed): `tp20.rs`'s `abandoned_connection_request_quarantines_its_rx_id_against_an_immediate_
  retry` proves the quarantine actually BLOCKS a same-`rx_id` retry while an abandoned attempt is
  still outstanding, and (by fail-then-restore against a temporarily reverted fix) that the
  quarantine mechanism itself is what makes the difference -- but it cannot prove the ORIGINAL
  Codex finding's own exact misattribution scenario end-to-end: a delayed `CONNECTION_ESTABLISHED`/
  `_LOST` indication landing on a RETRY's own wait instead of the abandoned attempt it actually
  belongs to. With the fix in place this is structurally true BY CONSTRUCTION (the retry is
  rejected before ever registering a wait of its own, so there is no live retry wait left for a
  stale indication to land on) -- the misattribution itself is only observable PRE-fix, which the
  fail-then-restore verification already covers directly (reverting `abandoned = true` reproduces
  the retry succeeding unrejected, the observable symptom of the underlying misattribution risk).
- **P3** (Codex review fix, PR #97, ADR-188 Fix F, test-coverage gap, not fixed): no `grpc_mock`
  end-to-end regression test can exercise `run_tp20_connection_request`'s pre-issue `self_still_
  live` recheck (Fix F, see this file's TP2.0 narrative section above) actually rejecting a stale
  CLL BEFORE the native `IOCTL_REQUEST_CONNECTION` call is issued, unlike its own sibling Fix G
  (mid-wait staleness, now covered -- see the entry two above). The two checks sit on opposite
  sides of the one genuine yield point this wait loop has: Fix G's own per-tick `tokio::time::
  sleep(POLL_INTERVAL_MS)` inside the bounded wait is what makes a concurrent disconnect
  constructible at all (the same "no natural preemption point in this harness" reasoning
  `cop_ctrl_cycles.rs`'s `delay_dispatched_item_goes_stale_after_reconnect_on_same_channel` doc
  comment makes from the other direction, and the identical residual `j1939.rs`'s own ADR-180
  Decision 21 text already accepts for its structurally identical claim-loop case). Fix F's own
  check runs BEFORE any such yield point: `handle_start_comm`'s "Fresh attempt" `Requested`-state
  write and `run_tp20_connection_request`'s own first `logical_links` lock acquisition a few lines
  later are both synchronous, uncontended lock acquisitions with no intervening real `.await`, and
  every `#[tokio::test]` in this suite runs on tokio's default single-threaded (current-thread)
  runtime, so no concurrent `DisconnectComLogicalLink` RPC handler task can ever be scheduled to
  run inside that gap. The `PDU_IOCTL_SUSPEND_TX_QUEUE`-then-disconnect technique this suite uses
  elsewhere cannot substitute either: any item sitting in the requesting CLL's own `tx_held` (or
  still merely enqueued, never dispatched) is unconditionally drained and cancelled by `cancel_
  link_cops` the instant that CLL disconnects, so `should_skip_cancelled_item`'s own implicit-cancel
  branch skips it on the next dequeue before `handle_start_comm` -- let alone `run_tp20_connection_
  request` -- ever runs; a test built that way would pass whether Fix F is present or not, not
  actually exercising it. See `tp20.rs`'s own doc comment on `disconnect_mid_wait_after_native_
  issue_tears_down_the_leaked_slot` for the full reasoning. The fix itself was verified by direct
  code review instead (the `self_still_live` condition shape matches `still_on_this_channel`'s
  identical check in the wait loop below it exactly, and is checked strictly before the native
  `api.tp20_request_connection(...)` call). Not attempted here -- would need either a genuine
  multi-threaded-runtime race (inherently non-deterministic, rejected per this suite's own
  determinism discipline, ADR-149) or a new synchronization primitive this codebase does not have
  today (e.g. a way to pause the poll task mid-critical-section from outside it).
- **P3** (Codex review fix, PR #97, ADR-188 Fix I, test-coverage gap, not fixed): no `grpc_mock`
  end-to-end regression test can deterministically force the exact interleaving Fix I's
  `shared_channels`-outermost restructure of `handle_stop_comm`'s TP2.0 arm closes (see this file's
  TP2.0 narrative section above) -- a `PDU_IOCTL_START_REPEAT_MESSAGE` landing its own
  `repeat_message_ids` registration strictly between `CoptStopcomm`'s connection-clear and its own
  repeat-slot sweep. Unlike Fix G's own mid-wait staleness gap, there is no `PDU_IOCTL_SUSPEND_TX_
  QUEUE`-siphon or `__mock_set_tp20_no_indication`-style hook available: `PDU_IOCTL_START_REPEAT_
  MESSAGE` is a synchronous IoCtl RPC handled directly by its own request-handler task, never queued
  through `tx_held`, so nothing in this harness can pause it mid-flight inside the narrow window the
  fix closes -- the same "no natural preemption point in this harness" residual Fix F's own entry
  above records for the structurally analogous `self_still_live` recheck. `tp20.rs`'s `stopcomm_
  serializes_against_a_racing_repeat_message_start` issues both RPCs concurrently (`tokio::join!`
  over cloned client handles) and asserts the correctness OUTCOME (no repeat slot survives with a
  stale TX-ID, regardless of which side wins whatever interleaving actually occurs), but confirmed
  by direct experiment (40 consecutive runs against a deliberately reverted, pre-fix `handle_stop_
  comm`) that this concurrent-issuance technique alone never actually lands the specific
  interleaving the fix closes -- the test passed on every one of those 40 runs even without the fix,
  so it does not fail-then-pass and is not proof the race window is closed. The fix itself was
  verified by direct code review instead (both `handle_stop_comm` and `ioctl_start_repeat_message`
  acquire `shared_channels` before `logical_links`/`api`, never the reverse, and `shared_channels` is
  held continuously across `handle_stop_comm`'s own clear-connection + sweep + native-teardown span,
  mirroring `ioctl_start_repeat_message`'s identical span). Closing this test-coverage gap would need
  either a genuine multi-threaded-runtime race (rejected per this suite's determinism discipline,
  ADR-149) or a new synchronization primitive this codebase does not have today (e.g. a way to pause
  one RPC handler task mid-critical-section from a test, or a lock-contention observation backdoor).
- **P3** (Codex review fix, PR #97, ADR-188 Fix K, 8th round, test-coverage gap, not fixed): no
  `grpc_mock` end-to-end regression test can land a `Destroy`/`DisconnectComLogicalLink` in the exact
  gap between `run_tp20_connection_request` returning `Established` and `events.rs`'s own write-back
  running (see this file's TP2.0 narrative section above for the fix itself). Unlike Fix G's own
  mid-wait staleness race, this specific span contains no genuine yield point at all: every lock
  acquisition from the loop's own `Established` break through `run_tp20_connection_request`'s
  post-loop cleanup and back into the write-back is an uncontended `tokio::sync::Mutex` acquisition,
  which resolves without ever suspending, and this suite's `#[tokio::test]`s all run on tokio's
  default single-threaded runtime -- the same "no natural preemption point in this harness" residual
  Fix F's own entry above records, and `events_j1939_claim.rs`'s own `ctx_with_a_send_recv_cop` drain-
  loop race documents independently. Proven instead by two direct unit tests against the new
  `quarantine_tp20_connection_for_orphaned_write_back` helper (`events_tp20_connection.rs`'s own
  `tests` module): a vacant `rx_id` gets a fresh `abandoned` entry, and an already-registered entry
  (a genuine new registration racing into the gap) is left untouched. Closing the end-to-end gap
  would need the same kind of new synchronization primitive Fix F's/Fix I's own entries describe --
  not attempted here.
- **P3** (ADR-184, deliberate scope exclusion, not investigated further): `CP_J1939SourceName`
  (`PARAM_J1939_SOURCE_NAME`) is not `PDU_PC_UNIQUE_ID` class for J1939 (unlike the sibling
  `CP_J1939SourceAddress` fix, ADR-184) and RX frames are never matched against it --
  `SetUniqueRespIdTable` rejects it for a J1939 CLL, and `UniqueRespIdKey`/`route_frame` have no
  NAME-keyed matching mode. It remains a plain settable ComParam via `is_j1939_param`'s existing
  allowlist, unaffected. Wiring real NAME-based matching would need to correlate a received frame's
  claimed source address against this codebase's own J1939 address-claim/defend state machine's
  NAME<->address bookkeeping (ADR-180, `events_j1939_claim.rs`) -- which does not currently retain
  enough history to answer "which NAME currently owns address X" for an arbitrary received frame,
  only this CLL's own claim state. Not fixed here -- a materially different, unscoped mechanism from
  ADR-184's source-address matching.
- **P3** (ADR-179/Phase 5 Consequences, ADR-156 Decision, and ADR-175/Phase 11 Consequences, all
  claimed this was already tracked here -- Codex review, PR #114; three distinct, still-open
  J1939/J1708/Phase 2 residuals, none a duplicate of the `CP_J1939SourceName` entry above):
  (1) `CP_J1939TargetName`-based target resolution (`PARAM_J1939_TARGET_NAME`) -- like
  `CP_J1939SourceName`, it remains a plain settable ComParam with no TX-addressing effect;
  `CP_J1939TargetAddress`-only operation is the spec-sanctioned fallback ADR-179 Decision 3 already
  implements, but resolving a target by NAME instead needs the same bus-wide Name<->Address
  network-management table the `CP_J1939SourceName` entry above describes (not yet built) plus new
  TX-side lookup logic; the two ComParams could share that underlying table once built, but neither
  is implemented today. (2) `CONFIG_J1939_PINS` (the SAE J1939-13 connector) and (3)
  `CONFIG_J1708_PINS` (the dedicated SAE J1708 connector) -- both deferred since Phase 2 (ADR-156
  Decision, "Accepted residual, deferred: `J1939_PINS`/`J1708_PINS`") and reaffirmed by their own
  later phases (ADR-179 Consequences for (2); ADR-175 Consequences item 10 for (3), which also notes
  the clause-6 connector-validity queries remain unwired for either connector); J1962 remains the
  only wired connector for both J1939 and J1708 CLLs. Not investigated or scoped here -- all three
  need their own design pass (a Name<->Address table's storage/eviction/staleness model for (1);
  `PinData`/`ChannelKey` structural work for the two dedicated connectors for (2)/(3), the same class
  of work Phase 11's own code-scout investigation found disproportionate to attempt without a
  dedicated design pass when it scoped that phase to J1962 only).
- **P3** (ADR-188 §4/ADR-192 Consequences, both claimed this was tracked "in the backlog" without a
  self-contained entry existing -- Codex review, PR #114): nine of Table 77's ten TP2.0 timing
  parameters remain unwired -- only `CP_TP20BroadcastInterval` (`TP2_0_T_BR_INT`) was wired, by
  Stage 7c/ADR-192. The other nine govern the device's own autonomous connection-establishment
  machine (clause 19.3.1's maintenance/retry timing), out of every TP2.0 stage's scope to date
  (Stage 7a/ADR-188, Stage 7b/ADR-190, and Stage 7c/ADR-192 each explicitly left this deferral
  untouched). Not investigated or scoped here -- wiring them needs a dedicated design pass over
  which parameters (if any) are safe to expose as ComParams versus purely device-autonomous, the
  same class of question Stage 7a's own `design-advisor` round already worked through for the
  connection-count/passive-slot/broadcast surfaces this phase did choose to expose.
- **P3** (ADR-158/Phase 3 Stage 3a Consequences, claimed this was tracked "in the backlog" without a
  self-contained entry existing -- Codex review, PR #114): software-ISO-TP FD-sized segmentation
  remains unimplemented -- `apply_fd_mode` rejects the combination of an FD-substituted link with
  `link.software_isotp` outright rather than extending this codebase's own software-dispatched
  ISO-TP engine to segment messages larger than Classic CAN's 8-byte frame. Distinct from Stage
  3b/ADR-159's `FD_ISO15765_PS` substitution, whose real ISO15765 segmentation/flow-control state
  machine runs on the native adapter, not this service -- this residual is specifically about the
  software-ISO-TP path (`link.software_isotp`), which stays FD-incapable. Not investigated or
  scoped here -- would need a design pass over the software ISO-TP engine's own frame-size
  assumptions before attempting FD support.
- **P3** (ADR-165/Phase 12 Consequences, claimed this was tracked "in the backlog" without a
  self-contained entry existing -- Codex review, PR #114): device-autonomous repeat TX
  (`START_REPEAT_MESSAGE`) does not stamp `last_bus_activity` (`events.rs:2843`) -- only the
  resulting RX response does, when the repeat pattern actually triggers one. A mode-1
  idle-triggered tester-present could in principle fire mid-repeat-sequence on a quiet bus if no
  matching response arrives to stamp activity in the meantime. Accepted residual, not fixed in
  Phase 12 -- no existing mechanism reaches into device-internal TX timing to stamp this, and doing
  so would require the software-timer machinery ADR-165's own Decision explicitly avoided (the
  device, not this service, owns repeat retransmission timing). Not investigated or scoped here.
- **P3** (ADR-179/Phase 5, two genuine interpretations not directly specified by the ADR, flagged in
  `events_j1939_claim.rs::run_j1939_claim_loop`'s own doc comment for review; interpretation (1)'s
  own choice is UNCHANGED by ADR-180, but its consequence is now mitigated -- ADR-180 Decision 2's
  round-20/21 corrections retain a timed-out candidate's claim-tracking entry when its cancel fails,
  instead of dropping it and risking a sibling CLL claiming the same still-defended address; a failed
  cancel now also ends the whole attempt rather than advancing further): (1) a per-candidate wait that
  times out with no `RX_FLAG_J1939_ADDRESS_CLAIMED`/`_LOST`
  indication at all (neither outcome) is treated the same as an explicit `_LOST` -- advance the cursor
  and retry the next candidate -- the more forgiving reading, and consistent with every other failure
  outcome in this loop using the same retry-next-candidate path, but a fail-outright reading is
  equally defensible and was not ruled out by ADR-179. (2) A synchronous `protect_j1939_addr` issue
  failure (e.g. the device rejects a malformed candidate outright) is treated the same way -- this
  candidate's attempt failed, advance and retry -- rather than failing the whole claim loop. Neither
  was chased further here; worth settling explicitly if this mechanism is revisited.
  Done when: an ADR (or an ADR-179 amendment) states which behaviour applies to each of the two
  cases and a test pins each one. Blocked on: the maintainer's decision on whether a J1939 claim candidate
  that times out or fails to issue advances to the next candidate or fails the whole claim loop
  (ADR-179 left both readings open).
- **P3** (test-coverage gap, preserved from a completed backlog entry removed during the 2026-08-18
  backlog cleanup; Codex review finding, PR #79): `run_j1939_claim_loop`'s `timed_out_without_indication`
  path (Decision 2's original mechanism, plus its round-20/21 corrections) still has no direct
  regression test, even though the two mock backdoors that would make it reachable now both exist
  (`__mock_set_j1939_claim_no_indication`, `__mock_set_j1939_cancel_error`) -- they were added for
  Decisions 21/22's own tests, not as a retrofit onto this gap, and no existing test combines them to
  exercise this path. Now actionable if this mechanism is revisited.
- **P3** (ADR-180 round-3 correction, test-coverage gap, preserved from a completed backlog entry
  removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79): the atomic
  `shared_channels`-held critical section `run_j1939_claim_loop` uses for claim issue + registration
  (folding the sibling-ownership check, the native `protect_j1939_addr` issue, and the
  `SharedChannel::j1939_claims` registration write into one section so a claim attempt can no
  longer be registered after teardown already ran), and the best-effort native cancel a
  no-indication timeout now issues before removing its local routing entry, both have no dedicated
  regression test -- the same infeasible-to-construct-deterministically class as this file's other
  documented disconnect-race windows; verified by code inspection and the full J1939 suite passing
  unchanged. If claim registration is later moved outside this critical section, a stale attempt
  could again be registered after teardown, undetected by any existing test. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) for this mechanism's
  current governing design.
- **P3** (ADR-180 Decision 9, test-coverage gap, preserved from a completed backlog entry removed
  during the 2026-08-18 backlog cleanup; Codex review finding, PR #79):
  `cancel_j1939_claim_after_failed_startcomm`'s regression test
  (`cancelling_the_optional_startcomm_message_after_a_successful_claim_frees_it_for_a_sibling`)
  exercises only one of its four `handle_start_comm` call sites (`ReceivePhaseOutcome::Terminal`);
  the other three (`P3GapOutcome::Cancelled`, `Err(TxFailure::Event(...))`,
  `Err(TxFailure::Cancelled)`) remain untested. All four invoke the identical helper with identical
  arguments, so this is not independently-varied logic, and each was confirmed
  structurally unreachable/infeasible to construct deterministically for a J1939 CLL via this test
  harness's available surface (`CP_P3Phys`/`CP_P3Func` are not in J1939's `is_j1939_param`
  allowlist, so `wait_for_p3_gap` always returns `Ready` immediately; a genuine
  `TxFailure::Event`/`Cancelled` needs either a mock "fail the next write" backdoor this suite
  lacks, or software-ISO-TP framing J1939 never uses). See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) Decision 9's own
  Consequences for the full reasoning.
- **P3** (ADR-180 Decision 9, accepted residual, preserved from a completed backlog entry removed
  during the 2026-08-18 backlog cleanup; Codex review finding, PR #79):
  `cancel_j1939_claim_after_failed_startcomm` clears the cancelled claim's bookkeeping but does not
  revert the `NODE_ADDRESS` Working/Active write-back the claim's own earlier success already
  performed (matching Decision 4's own precedent for the same reason). A `CoptSendrecv` issued on
  the still-not-`comm_started` CLL after this cancellation frames with a now-undefended source
  address until the next claim-requesting `CoptStartcomm` overwrites it. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) Decision 9's own Accepted
  residual note.
- **P3** (test-coverage gap, preserved from a completed backlog entry removed during the 2026-08-18
  backlog cleanup; Codex review finding, PR #79): `deliver_j1939_claim_indication`'s final
  re-comparison of the live `SharedChannel::j1939_claims` entry against its own earlier snapshot,
  immediately before writing `j1939_claim_results`/`j1939_reclaim_pending` back -- added to close a
  `DisconnectComLogicalLink`/`DestroyComLogicalLink` landing inside this function's own
  multi-acquisition gap (a plain disconnect does not bump `connect_generation`, so the earlier
  `connect_generation`-only check alone would miss it) resurrecting a claim-result/reclaim-pending
  entry for a CLL that just tore down -- has no dedicated regression test: the same
  infeasible-to-construct-deterministically class this file's other documented disconnect-race
  windows use, since no test-only scheduling hook exists to land a `Disconnect` precisely inside
  this function's own lock-acquisition gap. Verified by code inspection only. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) for this mechanism's
  current governing design.
- **P3** (ADR-180 Decision 13, test-coverage gap, preserved from a completed backlog entry removed
  during the 2026-08-18 backlog cleanup; Codex review finding, PR #79): a SAE J2534-2 clause 14
  repeat-message `START` that passes `ioctl_start_repeat_message`'s J1939 enqueue-time gate can
  still race a live claim attempt for `shared_channels` against the `Claimed`/`Exhausted`
  terminal-sweep's own `stop_repeat_slots_for_cll` call -- the specific TOCTOU window this
  Decision's two mechanisms together close -- with no dedicated regression test; the four surviving
  Decision 13 tests cover only steady before-claim, after-exhaustion, non-negotiated, and
  after-success states, not this interleaving. Deterministic coverage would need a claim-loop pause
  hook that does not currently exist; verified by code inspection instead. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) Decision 13's own
  Consequences entry for the full reasoning.
- **P3** (ADR-180 round-16/21/22 corrections, test-coverage gap, preserved from a completed backlog
  entry removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79): the
  `connect_generation` guards `cancel_j1939_claims_for_cll`, `stop_repeat_slots_for_cll`, and
  `cancel_send_recv_cops_for_cll` each gained (rounds 16, 21, and 22 respectively, closing the
  identical bug class across all three sibling helpers) -- filtering their `LogicalLinkState`
  lookup on `connect_generation` in addition to `cll_handle`, so a disconnect+reconnect reusing the
  same `cll_handle` and landing inside one of these helpers' `.await` gaps cannot wrongly cancel the
  NEW generation's claim, repeat slots, or send/receive COPs -- have no dedicated regression test
  for any of the three: the same infeasible-to-construct-deterministically class this file's other
  documented disconnect-race windows use; verified by code inspection and the full J1939/crate
  suites passing unchanged for each. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md)'s Consequences section.
- **P3** (ADR-180 Decision 14 Part B, accepted residual, preserved from a completed backlog entry
  removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79): `handle_send_recv`'s
  per-cycle transmit-time re-check of a `CoptSendrecv`'s resolved J1939 source address
  (`resolve_send_recv_tx`'s `j1939_tx_source`) against the CLL's current `j1939_claimed_address` has
  no dedicated regression test for its own outcome-arm race -- a claim spontaneously lost, or still
  pending, in the narrow window between `wait_for_p3_gap` returning `Ready` and `transmit_request`
  actually writing. Deterministically racing a live claim wait against a queued cyclic send's
  per-cycle dispatch is the same unobservable/uncontrollable-timing class this crate's other
  documented test-infeasibility notes already use; verified by code inspection instead. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) Decision 14's own Accepted
  residual note. Decision 14 Part A's own enqueue-time gate IS independently tested.
- **P3** (ADR-180 Decision 15 round-17 correction, accepted residual (1) of 2, preserved from a
  completed backlog entry removed during the 2026-08-18 backlog cleanup; Codex review finding, PR
  #79): `apply_params_to_hardware_locked`'s `!all_ok` adapter-fault case (at least one `SET_CONFIG`
  call in a `CoptUpdateparam`'s multi-key `hw_set` fails) can leave the adapter holding a partial
  subset of `hw_set`'s keys applied and others not, with no revert -- an inherent property of
  pushing a multi-key `hw_set` via a sequence of individual `SET_CONFIG` calls, orthogonal to and
  not addressed by Decision 15's own execution-time-recheck relocation. See
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md) Decision 15's round-17
  correction for the full analysis (accepted residual (2) of 2, the U2 stale-generation-bail
  hardware-write note, is pre-existing ADR-086 text and remains intact in this file's own
  implementation-summary narrative above the Prioritized Backlog).
- **P3** (ADR-180 Decision 22 round-23 correction, accepted residual, preserved from a completed
  backlog entry removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79):
  `reconcile_leaked_j1939_claims`'s channel-wide batch retry holds `shared_channels` (with `api`
  nested under it) across every currently-leaked address's sequential native cancel call, not the
  single thin call the ADR-080 "thin, non-blocking native primitive" precedent was measured
  against -- `SharedChannel::leaked_j1939_claims` is channel-wide and bounded only by the live SAE
  J1939 address space (≤253 entries), so a channel with many accumulated leaks could hold the
  lock across a correspondingly long native-call sequence, stalling unrelated CLLs on the same
  physical channel. Accepted when found because the list is only ever non-empty while the adapter
  is already in a degraded state (a native cancel has already failed at least once) and is pruned
  wholesale at `ref_count == 0`, so it cannot grow without bound; see
  [ADR-180](../docs/adr/ADR-180-j1939-claim-registration-atomicity.md)'s round-23 correction to
  Decision 22 for the full analysis. Revisit if a future round finds this lock-hold duration causing
  an observed cross-channel stall, rather than a theoretical one.
- **P3** (ADR-180 Decision 22 round-23 correction, accepted residual, preserved from a completed
  backlog entry removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79):
  `reconcile_leaked_j1939_claims`'s partial-batch behavior is untested -- when the leaked set holds
  more than one address and only some of their native cancels succeed, the `retain` closure's
  actual per-address split is never exercised, because `__mock_set_j1939_cancel_error` (Decision
  22's own backdoor) is a single global toggle and can only force the retry to wholesale-succeed or
  wholesale-fail, never a mixed outcome. Accepted when found for the same reason this crate's other
  documented test-infeasibility notes were (Decision 6, Decision 8): no per-address cancel-failure
  backdoor exists to construct the scenario. Revisit if such a backdoor is ever added for another
  reason.
- **P3** (`edge-case-hunter`, same verification pass, spec-citation nuance, not a functional bug):
  `tx_header::j1939_header_bytes`'s PF>=240-vs-&lt;240 PDU1/PDU2 addressing-byte split is correct (the
  well-known SAE J1939-21 convention, verified at the PF=239/240 boundary by this file's own tests)
  but its doc comment over-attributes the threshold itself to SAE J2534-2 clause 16.4.3/16.4.4, which
  only define the raw 5-byte layout and explicitly defer packing correctness to SAE J1939-21 --a spec
  this workspace's `vehicle-comm-specs` inventory does not include, so the 240 threshold itself
  cannot be independently verified against primary text in-repo. Not chased further; the behavior
  itself is standard, well-established J1939 convention, just not verifiable from a spec text this
  workspace actually has a copy of.
- **P3** (design consult finding, scoped out of the fix that added
  `resources::bustype_default_name_for_hw_protocol_id`/generalized
  `rpc_create_com_logical_link`'s Honda DIAG-H/SAE J1708 bustype-default
  fallback to SWCAN, FT-CAN, UART Echo Byte, and SAE J1939): the identical
  caller-supplied-`bus_type_name`-wins gap still exists for a directly-named
  BASE-family `_PS`/`_CHx` hardware protocol id (e.g. `CAN_PS`,
  `ISO15765_PS`) with no resource-table row match -- deliberately left out of
  that fix's scope, since `DATA_RATE` genuinely is `SetComParam`-configurable
  on that legacy raw-id route and callers historically supply `bus_type_name`
  for it, so changing it would be a behavior change, not a straightforward
  bug fix. Revisit if this route's own missing/mismatched-`bus_type_name`
  behavior ever needs to match the six standalone `_PS`-only families' new
  protocol-identity-wins precedent.
- **P3** (ADR-175/Phase 11, accepted residual, tracked per the ADR's own Consequences section):
  `resources.rs`'s `PINS_SAE_J1708` (`0x023C`'s default J1962 pins, 3/11) has no textual basis in
  SAE J2534-2 clause 17 or clause 6.3.3.2 -- neither documents any valid pin at all for SAE J1708
  on the J1962 connector (clause 6.3.3.2's own pin table describes only the dedicated SAE J1708
  connector's numbering, which this phase does not use). The chosen pins are an editorial
  convenience default matching a commonly-cited real-world heavy-truck scan-tool wiring
  convention, the least spec-grounded default this codebase has picked so far. Revisit if a
  primary SAE J1708 electrical-layer reference (a wiring standard, OEM service documentation, or
  equivalent) becomes available in this workspace.
  Done when: the J1708 default pins in `resources.rs` cite a primary J1708 reference, or are
  changed to match one. Blocked on: a primary SAE J1708 electrical-layer reference (a wiring standard or OEM service
  documentation), which is not available.
- **P3** (`edge-case-hunter`, ADR-172 PR, pre-existing question surfaced while verifying the
  SW-CAN speed-transition withhold, not introduced by that fix): whether `can_channel_mode =
  "native-mixed"` (ADR-160) combined with an SW-CAN or FT-CAN resource could ever let a genuinely
  conforming device report a frame's native `ProtocolID` as something other than
  `SW_CAN_PS`/`SW_ISO15765_PS`, in a way that would let ADR-172's withhold check
  (`resources::is_sw_protocol_id(frame_protocol_id)`) miss a speed-transition frame it should
  catch. No test in this codebase exercises `native-mixed` combined with SW/FT at all today (SW/FT
  links are always pin-selected, taking a different per-CLL routing path than native-mixed's own
  flag targets). Not investigated — see ADR-172's Consequences for the full framing; a
  `design-advisor` consult is the right next step if this combination ever becomes something this
  codebase actually needs to support.
- **P3** (split out once `install_point_to_point_fc_filters`/`can_connect_flags`/
  `find_native_mixed_uudt_usdt_collision`'s raw, unfiltered `CP_CanRespUUDTId`/`CP_CanRespUSDTId`
  reads were fixed to route through `uudt_resp_id`/`usdt_resp_id`'s `0xFFFFFFFF`-sentinel filtering
  -- now fixed and out of this backlog -- not a defect):
  `events_rx_routing.rs`/`rpc_primitive.rs` still read `CP_CanRespUUDTId`/`CP_CanRespUSDTId` via raw,
  unfiltered lookups in a few places, but remain functionally inert — `0xFFFFFFFF` can never equal a
  real received frame's CAN ID, so a sentinel-valued entry is just a dead/no-op list entry there, not
  a source of wrong behavior. Worth mirroring `uudt_resp_id`/`usdt_resp_id` at these sites too for
  consistency, once touched for an unrelated reason — not urgent enough to justify a dedicated pass.
- **P3** (edge-case-hunter, ADR-167 PR): [ADR-167](../docs/adr/ADR-167-kwp-rx-parser-carb-address-mode-support.md)'s
  fix (`events.rs`'s `kwp_header_and_payload_len` now also recognizes the CARB/ISO9141-2
  exception-addressing format, `format & 0xC0 == 0x40`, splitting off a fixed 3-byte header with no
  footer) proves the header/payload *split* is correct (`ResultData.data_bytes`/`extra_info`), but no test exercises the two downstream
  consumers ADR-167's own Context section names as the actual motivation for getting that split
  right: `ExpectedResponseData` pattern/mask matching and `detect_pending_rc`/`RcHandlingConfig`
  (`service.rs`) RC-byte-offset detection, for a CARB-format frame specifically. A first attempt at a
  pattern-matching test (a real `CoptSendrecv` physically-addressed exchange on ISO9141) hung
  indefinitely in CI verification — this codebase has no existing KWP/ISO9141/ISO14230
  physically-addressed-`UniqueRespIdTable` test to model the correct ADR-100 tier-1 binding setup
  against (`rc_handling.rs`, the dedicated RC-byte-offset suite, is CAN/ISO15765-only; no sibling KWP
  file exists), so building one correctly needs more investigation than this PR's narrower scope
  justifies. Not fixed here: edge-case-hunter traced both `detect_pending_rc` and the pattern-match
  path (`bind_registrant`) and confirmed both are protocol-agnostic, operating purely on the
  already-split payload slice with no header-length-specific logic — so this is a coverage gap, not a
  suspected bug. A future pass should first build a minimal KWP/ISO9141 physically-addressed
  `CoptSendrecv` test fixture (establishing the correct `UniqueRespIdTable` physical-addressing setup
  for the K-line family, which no test in this codebase currently does), then use it to cover both the
  general KWP gap in `rc_handling.rs` and this ADR's narrower CARB-specific case in one pass.
- **P3** (design-advisor consult, PR #46, latent observation -- not reachable via any shipped preset
  or spec default, not fixed here): `format & 0xC0 == 0x00` (unaddressed) with a *nonzero* configured
  low 6 bits (e.g. `0x01`) is treated inconsistently across this codebase's own mechanisms --
  `response_header_bytes`/`kwp_header_bytes` (`tx_header.rs`) compose it as format-byte-verbatim plus
  an unconditional trailing length byte (ADR-166's own accepted Decision for the `0x00` case, which
  does not vary on the low 6 bits), while `events.rs::kwp_header_and_payload_len` (the RX-direction
  parser, ADR-167) would read that same wire byte as a 1-byte embedded-length header instead. Not a
  reachable bug today (every spec-default preset uses an addressed format), so not fixed -- flagged
  for a future look if an unaddressed-with-nonzero-low6 configuration ever becomes reachable.
- **P2** (edge-case-hunter, Codex review round 21, ADR-165 PR #42): no test (in either
  `j2534-0404-mock` or `j2534-0404-service`) exercises this round's actual motivating property for
  `spawn_repeat_worker`'s `Condition == 0` branch (`j2534-0404-mock/src/lib.rs`) -- that a worker's
  OS thread wakes and exits within roughly `POLL_STEP_MS` of `IOCTL_STOP_REPEAT_MESSAGE`/disconnect/
  `__mock_reset`, rather than sleeping up to the rest of `TimeInterval`. The mock has no thread-
  count/liveness introspection hook, so a regression here (e.g. someone reverting the round-21 fix
  back to a single blocking sleep, or reintroducing the missing under-lock epoch re-check the same
  round-21 edge-case-hunter pass also caught and fixed) would not be caught by CI. Not fixed here --
  would need a new test-only thread-liveness accessor, which is a disproportionate addition for this
  P2-severity resource-hygiene property; flagged for a future look if this class of bug recurs.
- **P3** (test-coverage gap, deferred from the `handle_channel_hard_error`/`was_primary` fix, PR #47): no test discriminates `was_primary == true` from `== false` for `handle_channel_hard_error`'s (`events.rs`) `repeat_message_ids` drain gate -- proving the distinction requires a dual-channel-mode (ADR-046) CLL with a hard error forced onto only one of its two channel ids (primary vs. UUDT companion), a heavier test-setup lift than existing precedents provide. Should be picked up in a future pass touching this mechanism again.
- **P3** (edge-case-hunter, same PR, pre-existing gap the fix above merely surfaced a payload for --
  not introduced or fixed here): `finalize_connected_link` (`rpc_link.rs`) unconditionally overwrites
  `LogicalLinkState::channel_key` on every successful connect, including a reconnect of an
  already-disconnected/hard-errored CLL, without first checking whether the link already held a
  *different* `channel_key` and releasing that entry's `ref_count` share (mirroring
  `DisconnectComLogicalLink`'s own `channel_key.take()` + ref-count decrement). Confirmed via a grep
  sweep: `channel_key` is written in exactly one place (`finalize_connected_link`) and cleared in
  exactly two (this file's `Disconnect` teardown, and now `handle_channel_hard_error`'s own state
  reset -- which deliberately leaves `channel_key` itself untouched, by design, so a later
  Disconnect/Destroy can still find and release it). A reconnect that computes the SAME `ChannelKey`
  as before is already correctly rejected while the old entry is `dead` (ADR-134); the gap is a
  reconnect to a genuinely *different* key, which orphans the old `SharedChannel` entry's `ref_count`
  above zero forever (no reaper/sweep over `dead` entries exists anywhere in this codebase). Not fixed
  here -- would need `finalize_connected_link` to release any pre-existing `channel_key` the same way
  `Disconnect`'s teardown does, verified against every one of its own call sites; a future pass should
  also update `SharedChannel::leaked_repeat_message_ids`'s doc comment (`service.rs`) once closed, since
  that comment currently documents this fix's own accepted-residual exception to its "no unbounded
  growth" invariant.
- **P3** (refactoring review, PR #33, deliberately deferred, not a defect): a full-subsystem
  refactoring survey of `j2534-0404-service` (2026-08-06) found several further candidates beyond
  the low-risk `events.rs` file-splits PR #33 implemented. Deferred, in the survey's own priority
  order:
  - `events.rs`'s three oversized functions — `handle_start_comm` (~1,333 lines), `poll_rx_inner`
    (~955 lines), `handle_update_param` (~834 lines) — mix several distinct phases (guard checks,
    hardware apply, protocol init, tester-present arm/re-arm) in one body each. The tester-present
    state machine specifically is spread across these three plus `dispatch_due_tester_present`;
    consolidating it into a dedicated controller type is a genuine design decision (state-machine
    behavior), not a mechanical move — needs a `design-advisor` pass before attempting, per this
    doc's own precedent for ADR-100/147/148-adjacent logic.
  - `rpc_link.rs`/`rpc_misc.rs`/`rpc_primitive.rs`: a repeated lock→call→map-error boilerplate
    pattern (~20+ occurrences) is a shared-helper-extraction candidate; `rpc_connect_com_logical_link`
    (~2,057 lines) and `rpc_start_com_primitive` (~1,137 lines) are decomposition candidates.
  - `names.rs` (4,675 lines, ~70% test code): protocol/bustype/pintype/ioctl name-mapping data
    tables, resource/protocol lookup logic, and ComParam shortname mapping are mixed in one file —
    module-split candidate.
  - `j2534-0404-mock/lib.rs` (3,822 lines): sound but monolithic; a per-PassThru-function-family
    submodule split (e.g. connect/message/filter families) would help navigation. Lower priority
    than the above.
  - Surveyed and found healthy, no action needed: `comparam_support.rs`/`comparam_defaults.rs`/
    `comparam_id.rs`/`service_params.rs` (data-table form judged preferable to a generated/macro
    alternative here), `resources.rs`, `protocol.rs`, `can_mode.rs`, `tx_header.rs`, `isotp.rs`,
    `j2534-0404/src/lib.rs`.
- **P3** ([ADR-215](../docs/adr/ADR-215-tester-present-message-length-validation.md) Decision
  item 5): `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp` carry the identical ISO
  22900-2 `ParamMaxLen = 12` declaration `CP_TesterPresentMessage` does, but ADR-215's
  `SetComParam`-time length check is scoped to `CP_TesterPresentMessage` only — these two remain
  unvalidated.
- **P3** ([ADR-215](../docs/adr/ADR-215-tester-present-message-length-validation.md) Decision
  item 5): `CP_J1939Name`'s own silent-truncation-to-8-bytes policy (`events_j1939_claim.rs`)
  remains inconsistent with the reject-outright precedent ADR-215 extends for
  `CP_TesterPresentMessage` (matching `CP_UartConfig`/ADR-071, `CP_Parity`/ADR-027,
  `CP_BlockSizeOverride`); changing an already-shipped ComParam's behavior was judged a separate,
  riskier decision than adding a new check to a previously-unvalidated one.
- **P2** (design-advisor, Phase 3 Stage 3b, ADR-159 Decision 6, not a code defect): `HS_CAN_TERMINATION`
  (SAE J2534-2 Table 97, bus termination for CAN FD/ISO15765-on-CAN-FD) is not forwarded to any
  native `SET_CONFIG`. `CP_TerminationType` (ISO 22900-2) could map its `0`/`3` values cleanly, but
  this ComParam is stored-not-forwarded for every protocol today, its `1`/`2`/`4` values have no
  native encoding, and nothing in clause 21/22's connect sequencing depends on it — wiring only the
  FD case would be a narrower inconsistency than the value it adds.
- **P3** (design-advisor, Phase 3 Stage 3b, ADR-159 Decision 7, matches ADR-153 precedent; narrowed
  by [ADR-213](../docs/adr/ADR-213-can-fd-additional-channels.md) — edge-case-hunter finding, PR
  review, this entry was stale and needed updating rather than left as a blanket claim): `DEVICE_INFO_
  FD_CAN_SUPPORTED`/`DEVICE_INFO_FD_ISO15765_SUPPORTED` now DO have discovery-cache wiring —
  `chx_device_info_supported_parameter`/`connect_discovery_check` both gained arms for these flags as
  part of wiring the `_CHx` channel-count cap (ADR-213 Decision item 2), which also fulfills ADR-159
  Decision item 7's own deferral for that dimension. Only the `_SIMULTANEOUS`/`_PS_J1962` companion
  bits remain unwired for both families — no Stage 1/Stage 2/ADR-213 call site consults either, the
  same accepted residual every other family's own `_SIMULTANEOUS` companion bit already carries. An
  adapter that doesn't support this feature still gets a correct `ERR_NOT_SUPPORTED` connect failure
  (clause 21/22.3.2.1) for those two, just not a pre-emptive discovery-time one.
- **P2** (ADR-160 Consequences, `CAN_MIXED_FORMAT_ALL_FRAMES` itself closed by ADR-217): TX-side
  `ProtocolID` selection (a client sending a raw CAN-tagged message on a native-mixed channel) is
  out of scope for both native-mixed sub-modes — RX-only (ADR-217 Decision item 7 confirms
  `CAN_MIXED_FORMAT_ALL_FRAMES` did not change this). Auto-detection of device clause-8 support (a
  probe-and-fall-back the way `Auto` mode probes dual-channel capability) is not implemented —
  `Auto` never resolves to `NativeMixed`/`NativeMixedAllFrames`, and a device lacking support
  simply fails the connect with `ERR_NOT_SUPPORTED`. Discovery-cache wiring for
  `CAN_MIXED_FORMAT_SUPPORTED` advertisement is deferred for both sub-modes, matching every prior
  Phase 3 stage's identical deferral (ADR-153 precedent; ADR-217 Consequences).
- **P2** (design-advisor, Phase 3 Stage 3c, ADR-160 Consequences): native-mixed mode's interaction
  with an FD-substituted link (`FD_ISO15765_PS`) is out of scope — clause 22.2.2.d requires
  `CAN_MIXED_FORMAT` support on an FD-connected ISO15765 link too, but this stage excludes any
  FD-substituted link from `NativeMixed` entirely (`native_mixed_format`'s own
  `!is_fd_protocol_id(...)` gate), keeping ADR-159's existing `FLOW_CONTROL_FILTER` fallback for
  such a link regardless of the module's `can_channel_mode` setting.
- **P2** (reclassified from P1; ADR-162, investigated 2026-08-06/07): the client-`PASS_FILTER`/
  `BLOCK_FILTER`-mistagging scenario this entry used to describe (`install_client_message_filters`
  tagging a native-mixed client filter with the plain ISO15765 `hw_protocol_id` instead of the
  clause 8.1 paired raw-CAN `ProtocolID`) turned out to be dead code, not a live bug: the
  pre-existing ADR-038 guard in `rpc_misc.rs`'s `ioctl_start_msg_filter` (predating native-mixed
  mode) already rejects any client `PASS_FILTER`/`BLOCK_FILTER` on an ISO15765-family link
  unconditionally, before `install_client_message_filters` is ever reached, both pre- and
  post-connect. What remained was a documentation gap only (ADR-038's rejection rationale no
  longer fully explained itself under native-mixed mode, since clause 8.2.2.4 does make a
  correctly-tagged client filter spec-legitimate there) — fixed by correcting the doc comments and
  client-facing error text in `rpc_misc.rs`, and adding an ADR-162 invariant note plus
  `debug_assert!` to `install_client_message_filters` (`rpc_link.rs`) recording the
  never-reachable-with-ISO15765 assumption so a future relaxation can't silently violate it.
  Actually supporting a native-mixed client `PASS_FILTER`/`BLOCK_FILTER` end-to-end remains
  unimplemented — it would need RX delivery-routing work (ADR-160 Decision 4 only forwards a
  CAN-tagged frame that matches a `CP_CanRespUUDTId` table entry, not an arbitrary client filter)
  plus the translation/lock-ordering cost ADR-162 Decision 1 declines as disproportionate for now.
  Tracked as a P2 (client `BLOCK_FILTER` support) / P3 (client `PASS_FILTER`, moot under the
  existing pass-all-baseline rejection, ADR-079) follow-up if ever requested.
- **P3** (edge-case-hunter, Phase 3 Stage 3c verification pass, no bug found, coverage gap only):
  no test connects two CLLs sharing one native-mixed physical channel to assert
  `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` fires exactly once (creator-only, never re-issued for a
  joiner) — the mechanism reuses Stage 3a's already-proven `fd_data_phase_rate` creator-only
  connect-step pattern verbatim, not a new invariant, so this is a coverage gap rather than an
  unverified mechanism.
- **P3** (edge-case-hunter, Phase 3 Stage 3c verification pass, no bug found, coverage gap only):
  `native_mixed_mode_excludes_fd_substituted_link` asserts only that `SET_CONFIG(CAN_MIXED_FORMAT)`
  never fires for an FD-substituted link, not that its UUDT id still gets ADR-159's
  `FLOW_CONTROL_FILTER` fallback specifically with the fallback's own literal `ProtocolID` — exercised
  indirectly by `native_mixed_mode_qualified_link_still_uses_fc_filter_fallback`'s identical
  mechanism, and structurally guaranteed since an FD-substituted link's `native_mixed` boolean is
  unconditionally `false`, but not pinned by a dedicated test staging a UUDT id on an FD link itself.
- **P3** (edge-case-hunter, Phase 3 Stage 3b verification pass, no bug found, coverage gap only):
  none of `fd_iso15765.rs`'s tests exercise `probe_can_channel_mode`'s `!is_fd_protocol_id(...)`
  guard (ADR-159 Decision 4) — that gate only matters under `can_channel_mode = "auto"`, and the
  one relevant test uses `"dual-channel"` directly. A regression dropping just that guard (letting
  an FD-substituted ISO15765 link participate in the module-wide auto-probe) would go undetected.
  Mirrors a pre-existing gap: Stage 3a's `fd_can.rs` and Phase 2a's `pin_selection.rs` don't test
  this same gate for `FD_CAN_PS`/pin-selected links either — not a novel regression, but still open.
- **P3** (edge-case-hunter, PR #20, ADR-147 eighth-amendment review, pre-existing, not fixed):
  `finalize_connected_link` (`rpc_link.rs`) resets `tx_suspended_by_error = false` on every
  (re)connect but never resets `tx_suspended_by_ioctl`. Since `PDU_IOCTL_SUSPEND_TX_QUEUE` has no
  connected-state guard (ISO 22900-2 (2022) §8.5.4/§8.5.5 carry no CLL-not-connected return code
  for it) and disconnect DOES clear `tx_suspended_by_ioctl` but a subsequent connect does not
  re-clear it, a suspend issued in the disconnected window leaves a freshly reconnected session
  starting life already suspended for a suspension no live action targeted. Independent of ADR-147;
  fix by adding the same reset `finalize_connected_link` already does for `tx_suspended_by_error`,
  or document as permanently accepted if intentional.
- **P3** (edge-case-hunter, PR #20, ADR-147 seventh-amendment review, pre-existing/broader,
  not fixed): `poll_rx_inner`'s frame delivery (`deliver_or_enqueue`) is not re-checked against
  live `LogicalLinkState::connect_generation` for most registrant shapes -- only the narrow
  eager-cyclic-deadline-confirm path (`events.rs`) does that re-check -- and `rx_buf` is the same
  `Arc` reused across a reconnect on the same `cll_handle` (`finalize_connected_link` does not
  allocate a fresh one). A frame snapshotted before a reconnect could in principle still be
  delivered into the new session's event stream. This is a general property of `poll_rx_inner`'s
  delivery path, not specific to `CP_SuspendQueueOnError` classification, and does not affect
  ADR-147's own `tx_suspended_by_error` gating either way. Not chased further as part of ADR-147;
  worth a `design-advisor` pass if it proves to matter in practice.
- **P3** (ADR-151, accepted residual, not fixed): `RxEntryKind::SoftwareIsoTp`
  links (ADR-046) never produce SOM/TxDone indications even when
  `CP_StartMsgIndEnable`/`CP_TransmitIndEnable` is enabled — raw CAN
  hardware never emits the `RX_START_OF_MESSAGE`/`RX_TX_INDICATION`
  `RxStatus` bits for a software-reassembled/segmented link, and
  `process_frame_for_entry`'s deliveries inherit the outer frame's
  `rx_status_flags` verbatim (there is nothing to inherit from for a
  synthesized indication). Synthesizing these indications for the software
  path — deciding at which point in reassembly/segmentation an SOM/TxDone
  herald would need to be manufactured, and with what timestamp — is a
  design question out of scope for ADR-151; needs its own follow-up if a
  client actually depends on these indications over a software-ISO-TP link.
- **P3** (Codex review, PR #145, ADR-132/A2-25 fix, mock-limitation test gap,
  not fixed): `rpc_module_disconnect`'s emit-`PDU_INFO_MODULE_LIST_CHG`-on-a-
  failed-close fix (`rpc_module.rs`) has no dedicated regression test,
  because `j2534-0404-mock`'s `PassThruClose` (`j2534-0404-mock/src/lib.rs`)
  unconditionally returns success and has no error-injection hook (unlike
  e.g. `__mock_set_stop_filter_error`/`__mock_set_fast_init_error`). The fix
  itself was verified by code inspection (the notification now sits before
  `close_result?`, so it fires unconditionally whether or not the close
  succeeds) and by the full existing suite showing no regression on the
  ordinary success path. Closing this would need a new
  `__mock_set_close_error` hook mirroring the existing per-call error-
  injection convention — deferred as mock-infrastructure work, not folded
  into the ADR-132 PR.
- **P3** (design-advisor + Codex review round 1, ADR-128/A2-23 fix, accepted
  residual, not fixed): `J2534Service::terminal_cops`'s backing
  `TerminalCopsLedger` has two unbounded-growth components. `entries` is
  purged only per-CLL, at `DestroyComLogicalLink`/`ModuleDisconnect` time —
  a long-running CLL that starts many COPs without ever being destroyed
  accumulates one entry per COP indefinitely. `destroyed_clls` (added by the
  Codex-review-round-1 amendment to close a record-vs-purge resurrection
  race) never shrinks at all — every CLL this service ever destroys leaves a
  permanent 4-byte marker for the lifetime of the process. Not fixed: ADR-128
  explains why a timer-based or read-tracking eviction was rejected for
  `entries` in favor of destroy-only, and why `destroyed_clls` must be
  permanent rather than time-bounded (a time-bounded marker would reopen the
  exact resurrection race it exists to close). If either ever proves to
  matter in practice (a very long-lived CLL with very high COP throughput,
  or a module session that creates/destroys an extremely large number of
  CLLs over its lifetime), a bounded per-CLL FIFO cap for `entries` (e.g.
  1024, evicting the oldest) would close the first without reintroducing a
  spurious `PDU_ERR_INVALID_HANDLE` window for any COP still plausibly being
  queried; `destroyed_clls` would need a different mechanism entirely (e.g.
  a monotonic per-CLL-handle generation counter instead of a marker set) to
  bound without reopening the race.
- **P3** (design-advisor latent-gap flag, ADR-126 round-2 `connect_in_flight` design, explicitly
  deferred): a client cancelling a `ConnectComLogicalLink` RPC after
  `spawn_new_shared_channel`/the shared-channel `ref_count` bump (`rpc_link.rs:1625-1633`) but
  before `finalize_connected_link` runs leaks that channel reference — no CLL is left owning it,
  so nothing ever decrements `ref_count` back down or tears the physical channel down. Pre-existing,
  unrelated to the round-2 `connect_in_flight` token fix itself (the token self-heals correctly on
  this same cancellation; the channel ref-count bump is a separate piece of state with no such
  self-healing). Not fixed here — closing it would need either a drop-guard around the ref-count
  bump that undoes it if `finalize_connected_link` never runs, or moving the bump to happen
  atomically with `finalize_connected_link`'s own publication instead of earlier in the function.
- **P3** (design-advisor latent-gap flag, ADR-126 round-2 `connect_in_flight` design, explicitly
  deferred, possible future conformance-audit item): a `SetComParam` call landing between
  `rpc_connect_com_logical_link`'s Working-set snapshot (`rpc_link.rs:1538`, `working_snapshot`)
  and `finalize_connected_link` applying it to hardware is silently absent from the connect this
  is currently in progress for — the client's new value takes effect only on some later event
  (e.g. a subsequent `CoptUpdateparam`), not on the Connect already underway, with no error or
  signal that the value was missed. Spec line 4895 (`vehicle-comm-specs/iso-22900-2/
  ISO_22900-2_2009(E)-Character_PDF_document.md`) maps `PDUSetComParam` during an active connect
  sequence to `PDU_ERR_CLL_CONNECTED` in a related context; whether the same reasoning should
  apply here, and what "connected" should mean for `SetComParam` specifically during this
  window, deserves its own conformance-audit reading rather than a ride-along fix discovered
  while designing an unrelated IOCTL's guard. Not fixed here.
- **P3** (design-advisor latent-gap flag, ADR-114 B21 fix, explicitly deferred):
  Table 55 (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:3587`)
  explicitly reserves `PDU_ERR_INVALID_PARAMETERS` for "the Filter Number is
  invalid", but `ioctl_stop_msg_filter`'s `client_filters.get(&filter_number)`
  lookup failure (`rpc_misc.rs`) currently returns `unknown_handle_status` →
  `PDU_ERR_INVALID_HANDLE` for an unknown `FilterNumber`, which Table 55
  reserves for an invalid CLL handle, not an invalid `FilterNumber`. Not
  fixed in the ADR-114 PR — flagged as a separate backlog item.
- **P3** (`edge-case-hunter` finding, ADR-114 B21 fix, explicitly deferred, mock
  limitation not a code defect): the ADR-114 regression tests
  (`tests/grpc_mock/pdu_ioctl.rs`,
  `stop_msg_filter_reports_fct_failed_and_keeps_filter_tracked_on_native_stop_failure`/
  `clear_msg_filter_reports_fct_failed_and_keeps_filters_tracked_on_native_stop_failure`)
  only exercise "every installed filter fails to stop" — genuine partial
  failure (some filter ids/`FilterNumber`s fail, others succeed in the same
  call) is untested, because the mock's `__mock_set_stop_filter_error`
  (`j2534-0404-mock/src/lib.rs`) is a blanket per-call override with no
  per-filter-id targeting. The production code (`ioctl_stop_msg_filter`/
  `ioctl_clear_msg_filter`, `rpc_misc.rs`) was verified correct for the
  partial-failure case by code inspection (`failed`/`failed_by_filter_number`
  only ever collect the ids that actually errored) but has no dedicated
  regression test pinning it. Closing this would need per-filter-id fault
  injection added to the mock (e.g. a settable `HashSet<MessageFilterId>` of
  filter ids to fail, checked in `PassThruStopMsgFilter` before the blanket
  override) — deferred as a mock-infrastructure change, not folded into the
  ADR-114 PR.
- **P3** (ADR-110 design-advisor latent-gap flag, A1-3 conformance-audit fix,
  explicitly deferred): `SetUniqueRespIdTable`'s own synchronous
  `LOCK_PHYSICAL_COM_PARAMS` rejection (ADR-043, amended by ADR-068) is the
  same shape as the `SetComParam` rejection ADR-110 removed — both are
  working-buffer-only writes with no hardware I/O of their own, promoted
  later at `CoptUpdateparam` execution time. `PDU_PC_UNIQUE_ID` is not
  `PDU_PC_BUSTYPE`, though, so whether ADR-110's "no spec-legal synchronous
  rejection for a Working-only write" reasoning actually transfers to
  `SetUniqueRespIdTable` deserves its own reading against §9.3.3's
  UniqueRespIdTable-specific text, not a ride-along fix in the BUSTYPE-scoped
  PR that happened to notice the parallel. Not fixed here.
- **P2** (test-coverage gap, not a design residual; Codex review, PR #116, P1 finding
  — the fix itself is applied and verified correct, only the regression test
  is missing): `handle_update_param`'s `still_on_this_channel_for_lock` recheck
  (`events.rs`, gating the `PDU_ERR_EVT_RSC_LOCKED` emission on a fresh
  channel/generation read, mirroring the `!all_ok` branch's own
  `still_on_this_channel` idiom) has no dedicated regression test pinning the
  disconnect+reconnect-during-hardware-apply race it closes. Investigated and
  found genuinely infeasible with the current harness, not just untried:
  `apply_params_to_hardware_locked` → `J2534Api0404::set_config` → the mock's
  `IOCTL_SET_CONFIG` arm are all synchronous Rust with zero internal
  `.await`/yield points, and nothing between the U1 `live_ctx` read and this
  new gate suspends either — this crate's single-threaded `current_thread`
  `#[tokio::test]` runtime can only interleave two tasks at a genuine
  suspension point, and none exists in this span (unlike the `CoptDelay`
  reconnect precedent in `cop_ctrl_cycles.rs`, which works because
  `handle_delay` awaits a real `tokio::time::sleep` per tick). Closing this
  would need a new SET_CONFIG-specific hold hook in `j2534-0404-mock`
  (mirroring `arm_write_rx_injection`'s pattern) plus wrapping the hardware
  call in `spawn_blocking` so a hold doesn't freeze the single-threaded
  runtime — a cross-cutting test-infrastructure change, not a mechanical
  addition, so deferred rather than folded into the P1 fix itself. The fix's
  correctness was verified by code inspection (identical shape to the
  already-tested `!all_ok`-branch sibling check) and by the full existing
  suite passing unchanged.
- **P3** (`edge-case-hunter` finding while verifying the P1 fix above, PR #116 —
  plausible-unverified, pre-existing, NOT a regression from that fix or this
  PR; still open after ADR-205 Decision item 1's `send_error_event`/
  `send_error_event_with_tag` merge, which is a *different* gap -- see that
  ADR's Consequences section): `send_error_event` (`events_event_senders.rs`,
  line numbers shift with every edit to that function, so cite the function
  by name rather than a specific range) only checks that `cll_handle`
  still exists in `logical_links` when it internally re-locks to send —
  it never re-verifies `channel_id`/`connect_generation` itself, unlike every
  caller-side staleness check that precedes it (including the new
  `still_on_this_channel_for_lock` gate this PR adds, and the `!all_ok`
  branch it mirrors). Every `send_error_event` call site relies solely on
  that caller-side check with no `.await` between the
  check and the call — on the multi-threaded Tokio runtime this service
  actually runs under (`#[tokio::main]`, not `current_thread` — confirmed
  only this crate's *tests* pin `current_thread`), a genuinely concurrent OS
  thread executing a disconnect+reconnect could in principle win the
  `logical_links` mutex in the gap between a caller's check and
  `send_error_event`'s own re-lock, delivering the event/`last_error` write
  to the newly-reconnected generation instead of being suppressed. Reachability
  is uncertain (Tokio's mutex fairness/waiter-queue semantics weren't traced
  to a conclusion) and this pattern predates this PR entirely (shared by every
  `send_error_event` call site in the file, not introduced here) — not
  fixed or further investigated in this PR. If pursued, route as a
  Rust-concurrency question to `design-advisor` (not a protocol/spec
  question), with `send_error_event` (`events_event_senders.rs`) and its call
  sites as the evidence set.
  TOCTOU fix, not fixed): `rpc_module_connect`'s `was_open` snapshot
  (`self.device_id.lock().await.is_some()`, `rpc_module.rs`) is read in a
  separate lock acquisition from the actual check-and-open done immediately
  after inside `ensure_open_device_inner`'s own lock acquisition. Two
  concurrent `ModuleConnect` calls can both read `was_open == false` before
  either has opened anything, so both send (or, depending on interleaving,
  neither is guaranteed to send) `ModuleListChg`, producing a duplicate or
  suppressed system-info event. Benign (an extra/missing event, not a
  correctness bug on `device_id` itself, unlike the guard race
  `lock_device_for` closes) — not fixed here; closing it would mean folding
  the `was_open` read into the same `lock_device_for`/`ensure_open_device_for`
  critical section `ModuleDisconnect`/`PDU_IOCTL_RESET` now use. Reconfirmed
  present and still benign by the ADR-107 addendum (h) holistic audit
  (epoch-tagged probe-cache redesign) -- unrelated to and unaffected by that
  redesign.
- **P3** (ADR-107 addendum (h), test-coverage gap, not fixed):
  `autodetect_sae_j1850_flavor`'s write-back gate (`rpc_link.rs`,
  `slot.is_some() && logical_links.contains_key(&handle)`) has no isolated
  regression test for its second conjunct (device open, but the probing
  CLL torn down by a concurrent `ModuleDisconnect`+reopen before the
  write-back runs) -- unlike its sibling in `probe_can_channel_mode`, which
  now has one (`probe_can_channel_mode_does_not_cache_for_a_dead_handle_with_a_device_open`).
  Manual trace confirms the guard is correct: `probe_sae_j1850_flavor`'s own
  internal `ensure_open_device(handle)` already re-validates CLL liveness as
  its last step before any synchronous (non-`.await`) candidate probing, so
  `Conclusive` is only reachable when `handle` was live at that point --
  closing this specific race window deterministically requires a delay/
  synchronization-injection hook this crate's `current_thread` test runtime
  doesn't have (same limitation as the untested deadlock/orphan-CLL fixes
  below). Not fixed here; would need either a production test seam or
  accepting a timing-dependent test.
- **P3** (ADR-105, edge-case review follow-up, test-coverage gap, not fixed):
  `rpc_link.rs`'s `ensure_uudt_companion_channel`-path reciprocal
  client-filter rejection (only reachable via ISO15765 dual-channel mode's
  UUDT-companion-channel join, unlike its three sibling
  `PDU_ERR_FCT_FAILED`/`state_guard_status` sites, which `pdu_ioctl.rs`
  covers) has no test asserting its behavior or `ErrorDetail`/message text
  at all — a pre-existing gap independent of ADR-105, surfaced because that
  change touched the message text at all four sibling sites. Closing it
  needs a dual-channel-mode CAN test fixture with two CLLs racing to join
  the same physical channel's UUDT companion, out of scope for ADR-105
  itself.
- **P3** (edge-case-hunter finding while verifying the `tests/stdio_startup.rs` SubscribeEvent-termination fix, now closed and out of this backlog, test-coverage gap, not fixed): both new/extended stdio-lifecycle tests only exercise `SubscribeEventRequest::Handle::CllHandle` subscriptions; the `ModuleHandle`/`SystemHandle` variants (`vci-service-interface`'s three-variant `Handle` oneof) are untested for stop/stdin-close termination. Low current risk since `spawn_shutdown_task`'s teardown (`std::mem::take` on the whole `subscriptions` map) is key-type-agnostic, but a future regression that special-cases CLL-keyed entries (e.g. extending `rpc_subscribe_event`'s CLL-specific `queue_before`/reconciliation logic) would not be caught by either test.
- **P3** (ADR-093, accepted residual, preserved from a completed backlog entry removed during the
  2026-08-18 backlog cleanup; Codex review finding, PR #79): `dispatch_due_tester_present`'s
  due-snapshot `expects_response` read (used for the `wait_for_p3_gap` call's `num_receive_cycles`
  argument) and the later, independently fresh `still_due` recheck's `expects_response` read (used
  for the post-send `TxGapState` stamp) can observe different `CP_TesterPresentReqRsp` values if a
  `CoptUpdateparam` changes it in between -- including during `wait_for_p3_gap`'s own wait, which
  can span up to the full configured P3 gap. Bounded to a single wait/send and self-healing on the
  very next send; see [ADR-093](../docs/adr/ADR-093-tester-present-unified-software-dispatch.md)'s
  Consequences section for the full analysis and the single-read alternatives considered and
  rejected.
- **P3** (ADR-102, edge-case review follow-up, test-coverage gap, not fixed): the RC21/RC23 branch's `match_reset_ceiling` handling in `wait_for_expected_response_inner` (`events.rs`) has four defensive checks -- a pre-sleep `skip_retransmit = ceiling_already_elapsed()` init, an in-loop per-chunk break, a post-loop unconditional recheck (catches the ceiling elapsing during the *last* chunk's own sleep, which the in-loop check alone would miss since the loop then exits via its `while` condition rather than the `break`), and a tail-`deadline` clamp -- but only the in-loop break and the tail clamp are exercised by `tests/grpc_mock/stopcomm_data_tx.rs::stopcomm_is_multiple_rc21_request_time_bounded_by_match_reset_ceiling` (confirmed by reverting each of the four individually and rerunning the suite). The pre-sleep init and post-loop recheck both require an RC21/23 occurrence landing within roughly one `POLL_INTERVAL_MS` (10ms) of the ceiling elapsing -- a genuine sub-poll-interval race, the same class of infeasible-to-construct-deterministically scenario already documented elsewhere in this file (see the `handle_stop_comm` S3-guard note above) given this crate's single-threaded `current_thread` `#[tokio::test]` runtime has no natural preemption point to land such a race on reliably. Both checks were verified correct by code inspection (structurally identical to the already-tested in-loop check and RC78 branch's analogous logic) rather than by a dedicated test.
- **P3** (ADR-102, accepted limitation, not fixed): tester-present's own response-discard path (`poll_rx_inner`'s `tester_present_discard` check, ADR-088/ADR-099) never runs the RC-handling/pending-response extension `CoptSendrecv`'s `wait_for_expected_response_inner` uses -- a `7F 3E 78` reply to a tester-present send is simply discarded (content case (a), same as any other matched reply) without extending the discard window the way a `CoptSendrecv` COP would extend its own deadline on 0x78, potentially letting the ECU's eventual real reply arrive after the window has already closed and leak through undiscarded. Wiring RC78 reload semantics into the discard path is a distinct design question (tester-present has no COP/ComPrimitive of its own to extend a deadline on) deferred out of this ADR's scope.
- **P2**: Expand ComParam support for CP_SamplesPerBit (still unsupported; no J2534-1 equivalent). CP_UartConfig now decodes into `DATA_BITS`+`PARITY` for the 6 J2534 v04.04-representable values (ADR-071); 2-stop-bit and 9-data-bit encodings remain unsupported (`SetComParam` rejects them — no J2534-1 equivalent exists).
- **P3** (accepted test-coverage gap, found while fixing the J1850 addressing allow-list gap, not chased further, low risk): the wire-level tests for J1850 addressing overrides (`j1850vpw_set_com_param_overrides_physical_addressing_on_wire`/`j1850pwm_set_com_param_overrides_functional_addressing_on_wire`) only cover VPW-physical and PWM-functional end-to-end; VPW-functional and PWM-physical are untested at the wire level (both are covered at the unit allow-list level by `j1850_addressing_params_allowed_for_both_j1850_variants`).
- **P2**: `CP_ExtendedTiming` (0x8050) is get/settable but never applied to hardware. ISO 22900-2 defines extended timing for ISO 14230-2 (key-byte-gated timing set), so adapter-layer support is in scope (revising ADR-076's out-of-scope rationale); implementation deferred pending additional information — key-byte inspection to detect extended-timing support, and the mapping of extended timing values onto J2534 SET_CONFIG timing parameters. Done when: key-byte detection selects the extended timing set and `CP_ExtendedTiming` is applied through the J2534 timing parameters, with tests. Blocked on: ISO 14230-2, which is not available, to define key-byte detection; and the maintainer's decision on how the extended timing values map onto J2534 `SET_CONFIG` parameters, which J2534-1 does not define (ADR-076, revised 2026-07-10).
- **P2** (ADR-099, found while writing that ADR's regression tests, not fixed there): `route_frame` (`events.rs`) only routes an incoming frame to a CLL when its CAN ID matches a configured `UniqueRespIdTable` entry's `CP_CanRespUSDTId`/`CP_CanRespUUDTId`, or unconditionally when the table is empty (ADR-007's no-table wildcard mode). Tester-present's own TX CAN ID (`CP_CanPhysReqId`/`CP_CanFuncReqId`-derived, a *request* identifier) is never itself a table entry for the common physically-addressed case, so ADR-099's TX-side discard (`tx_can_id`) is reachable in practice only when the table is empty or the TX CAN ID happens to coincide with a configured response ID — a genuine TX_DONE/CONFIG_LOOPBACK echo on a physically-addressed CLL with a non-empty table is already dropped by routing before it reaches the discard check at all, so no leak survives either way, but the protection ADR-099 added is narrower than `tx_can_id`'s existence alone suggests. Closing this would mean teaching `route_frame` to also match `CP_CanPhysReqId`/`CP_CanFuncReqId`, a routing-layer design question (would it also change delivery of *content* frames on those IDs, not just indication frames?) out of scope for a discard-mechanism fix.
- **P3** (ADR-139, accepted residual, not fixed): `ioctl_reset`'s (`rpc_misc.rs`) per-target `cancel_held_tx_items` loop lacks the same `shared_channels`-holding protection against a concurrent Disconnect+Connect race that ADR-139 closed for `handle_channel_hard_error`/`rpc_module_disconnect`, but needs a fully independent, precisely-timed concurrent Disconnect+Connect to matter (`PDU_IOCTL_RESET`'s own client-issued-reset semantics narrow the reachable window). Deferred pending a concrete repro or a future audit pass.
- **P3** (edge-case-hunter finding against ADR-139's diff, pre-existing, NOT introduced or fixed by ADR-139 -- the loop body it flags is byte-for-byte unchanged, only `chans`'s hold duration around it was widened): a CLL with both a primary channel and a UUDT companion channel (ADR-046) has two independent per-physical-channel poll tasks; if each independently detects a hard error close together, two serialized `handle_channel_hard_error` invocations both match the same `cll_handle` -- the first via `l.channel_id`, the second via the still-unset `l.uudt_channel_id` (or vice versa; each invocation only clears the one field matching its own `channel_id`, per the function's own comment). The widened `shared_channels` hold from this ADR correctly serializes the two invocations, and `cancel_link_cops` itself is idempotent for COP cancellation (the second invocation's scan finds nothing left in `primitives`, no duplicate `PduCopstCancelled`). But `send_error_event`/`send_cll_status` are unconditional and the second invocation still fires a duplicate `PDU_ERR_EVT_LOST_COMM_TO_VCI` + `PDU_CLLST_OFFLINE` pair for a CLL that is already offline; `send_module_status(PduModstNotAvail)`/`send_system_info(ModuleListChg)` at the tail likewise re-fire redundantly at the module level. Not fixed here (a distinct bug from the one ADR-139 closes, out of scope for that PR) and not covered by any existing test (nothing exercises independent primary+companion hard errors racing on one CLL). Recorded here per this codebase's own convention rather than left only in review history.
- **P3** (ADR-100 round 11, test-coverage gap, accepted as infeasible — same class as every prior round's in-handler-guard gaps, recorded here per this file's own backlog-durability convention after being rescued from a stray mis-nested sub-bullet under a now-closed, unrelated RC21/RC23 entry): `handle_delay` is the first in-handler guard in this ADR's entire history to get a genuine regression test, since its per-tick `tokio::time::sleep` is a natural preemption point this crate's single-threaded mock harness can exploit (every prior in-handler guard across rounds 4/5/6/8/9, now closed and out of this backlog, lacked one for the same "no genuine `Pending`-yielding preemption point" reason the next sentence restates for this round's own new sites). `cop_ctrl_cycles.rs::delay_dispatched_item_goes_stale_after_reconnect_on_same_channel` starts a `CoptDelay` on a physical channel shared with a kept-alive sibling CLL, disconnects and reconnects the SAME `cll_handle` at the SAME `DATA_RATE` while the delay is already dispatched and mid-sleep (rejoining the identical `ChannelId` with a freshly-bumped `connect_generation` — the specific scenario a bare `channel_id` check cannot catch), and asserts `cancel_link_cops`'s own `PduCopstCancelled` fires for the still-executing COP but `handle_delay`'s own staleness detection does not also emit a status for the same `cop_handle`. Verified to fail (a duplicate status event observed) with the per-tick `is_stale` short-circuited to `false` and the terminal recheck hardcoded to `true`, confirming it is a genuine regression test. Every other new/changed site in this round (`handle_start_comm`'s failure-path snapshot and Guard A2, `handle_restore_param`, R1-R3) faces the same infeasibility class as every prior round's in-handler guards — no genuine `Pending`-yielding preemption point exists between the relevant dispatch and check under this crate's single-threaded `current_thread` test runtime. Not fixed here; verified instead that the full existing crate test suite (233 tests, including the new `handle_delay` regression test) passes unchanged with every round-11 fix in place.
- **P2** (ADR-100, "Stage 2" — deferred, evidence-driven, explicitly open per the ADR's own Out-of-scope section): de-blocking bounded holds within the current model. Parking `CoptDelay` and the RC21/23 `request_time_ms` sleeps on the existing `CycleContinuation`-style scheduling would shrink `dispatch_due_tester_present`'s seven blocking hooks (`events.rs`, at the call sites cited by ADR-100's Context section: the `CoptDelay` per-tick sleep, the RC21/23 pending-response sleep, and TX-side ISO-TP pacing waits) — deferred because it changes wire timing, which must not ride an attribution ADR. Also bundled here: correcting `send_tester_present_once`'s `no_response_required` stamp to `true` unconditionally (ADR-093's derivation conflated "the ECU replies" with "a wait paces the bus" — see ADR-100's Context, "a contributing gap").
- **P2** (ADR-100, "Stage 3" — deferred, evidence-driven, explicitly open per the ADR's own Out-of-scope section): detaching finite/IS-MULTIPLE receive *execution* (as opposed to *binding*, which ADR-100 already requires and ships) for cross-CLL responsiveness — i.e. a same-CLL/sibling-CLL COP still waits behind a long finite `CoptSendrecv`/IS-MULTIPLE receive phase's blocking `wait_for_expected_response_inner` loop, unlike an IS-CYCLIC COP (which does detach, ADR-100 Decision §2, S5). Revisit only if cross-CLL finite-wait latency proves to matter in real deployments; the spec mandates multi-candidate *binding* for finite COPs, not detached *execution* (ADR-100's own scope boundary, per its Out-of-scope section).
- **P3** (ADR-100, found by the post-S8 full-cumulative-diff edge-case-hunter review, not empirically reproduced): `insert_cop_registrant` (`events.rs`) re-checks only that the target CLL still exists in `logical_links` at insertion time, not `connect_generation`/liveness beyond that. For the S6/S8 created-receive-only immediate-detach path, there is no `.await` between `dispatch_tx_send_cycle`'s own pre-dispatch staleness check and this insertion other than the lock acquisitions themselves, so a `Disconnect`'s `cancel_link_cops` (which clears `registrants` under a separate, later lock acquisition, per its own doc comment) could in principle interleave such that a fresh registrant lands on an already-disconnected CLL after `cancel_link_cops` already ran for it. Such a registrant would carry a stale `connect_generation`, so it can never bind a live frame (every `bind_registrant` candidate is generation-gated) and is invisible to `GetStatus`/`CLEAR_TX_QUEUE` (both `primitives`-gated, and this COP was never inserted into `primitives` on this path either) — self-healing on the CLL's next disconnect/destroy, which clears `registrants` wholesale again. Not constructed as a repro (depends on Tokio scheduling this crate's established test techniques cannot force deterministically, same infeasibility class as several entries above); code-inspection-confirmed as a theoretical gap, not an empirically verified race. Rated P3 (lower than the hard-error/`shared_channels` gap above, which has a real "spurious cancel/flag corruption" effect if reachable) since the worst case here is an inert, self-healing orphan registrant, not a client-visible correctness defect.
- **P3** (ADR-100, found by the post-S8 full-cumulative-diff edge-case-hunter review; not a bug, a documentation gap): no test covers two simultaneously-live tier-2 registrants on the *same* CLL with genuinely *overlapping* `expected` descriptors (as opposed to `receive_only_monitors_do_not_wedge_sibling_cops_or_each_other`'s two monitors, each restricted to a disjoint routing table). Per §9.2.6.3.4's own "first match wins, no further matching continued" rule, the earlier-registered (lower `registration_seq`) monitor legitimately claims a frame both would have matched — correct, spec-mandated behavior, not a bug — but it was never called out anywhere a reader would find it before hitting the surprise in practice. Recorded here per this file's own backlog-durability convention; no code change scheduled unless real-world usage shows this needs a docs callout too.
- **P3** (edge-case-hunter review of the round-9 fix, ADR-100 Decision §2/ADR-101 Decision §A, now closed and out of this backlog, that keyed a created-receive-only COP's `tier`/`rc_cfg`/`request_sid` classification on `created_receive_only` alone instead of the narrower `-1`-only gating it replaced): that classification correction covers three `created_receive_only` subtypes by its own scope (finite `N`, `-1`, `-2`/IS-MULTIPLE), but the new regression tests only exercise finite `N` — the two pre-existing `-2` tests in `cop_ctrl_cycles.rs` both use a non-created-receive-only `send_cycles_remaining = 1` setup and never touch this path. Low-risk: the fix itself keys `tier`/`rc_cfg`/`request_sid` on `wait.created_receive_only` alone with no discrimination between the finite-`N`/`-2` subtypes, so the tested finite-`N` path and the untested `-2` path go through byte-identical code. Recorded per this file's backlog-durability convention rather than chased with a rushed test; trivial to close with a `-2` variant of `receive_only_finite_n_created_cop_is_tier_two_registration_order_precedence` if this mechanism is touched again.
- **P3** (ADR-100 Decision §3's round-5 addendum, Codex review of PR #103, "Flagged, not fixed" — not confirmed reachable): `ExpectedResponse::matches`'s `cmp_len = mask.len().min(pattern.len()).min(data.len())` computation is zero against ANY descriptor whenever `data` itself is zero-length, regardless of that descriptor's own mask/pattern length — so a zero-length content payload would vacuously match even a genuinely non-vacuous (specific) descriptor at step 2 (`Tier1NonVacuous`), the same capture shape `is_vacuous()` closes for an empty descriptor, but via an empty *payload* instead of an empty *descriptor*. SOM (start-of-message) frames are already excluded from this matching path entirely (`poll_rx_inner` never calls `matches` for one, ADR-097's carve-out) — the frame class most likely to have an empty post-header-split payload — so this residual is scoped to whatever OTHER path, if any, can still deliver a zero-length content payload to `bind_frame`. Not identified as reachable during the round-5 addendum's review; recorded here per this file's own backlog-durability convention rather than chased further.
- **P3** (conformance-audit fix A2-6, design-advisor decision, explicitly out of scope for this fix): `ioctl_set_event_queue_properties`'s immediate-trim-on-cap-lowering path (`rpc_misc.rs`, the `while buf.len() > new_cap { buf.pop_front(); }` loop, currently lines 1681-1683) silently discards buffered `rx_buf` entries with **no** `PDU_EVT_DATA_LOST` signal at all — not even to a live `SubscribeEvent` subscriber, unlike every `push_cll_event` drop (ADR-115's corrected delivery rule). Deliberately not folded into the ADR-115 fix: this is a synchronous, client-commanded resize whose own success return is the signal the client already has, not a "buffer overrun" under ISO 22900-2 §D.1.8 — a different failure mode than `push_cll_event`'s overflow-on-insert case, which is what ADR-115 scopes `Lost` to. Recorded here per this file's backlog-durability convention rather than folded into the ADR-115 PR.
- **P3** (ADR-115 round 5, `edge-case-hunter` test-rigor findings on the round-4 atomicity fix, test-coverage limitation not a design residual — the fix itself is verified correct by independent induction proof; still applicable after round 6's redesign, since the atomic-critical-section shape these tests exercise is unchanged — only what gets written into it, and hence what the tests assert, changed): `rpc_primitive.rs::rpc_subscribe_event_live_sender_tests`'s two concurrent agreement tests (`concurrent_subscribe_for_existing_cll_keeps_map_and_queue_live_sender_in_agreement` and `concurrent_subscribe_and_create_for_the_same_not_yet_existing_cll_handle_keeps_map_and_queue_live_sender_in_agreement`, renamed round 6 from the `..._generation_in_agreement` names this note originally cited) force genuine concurrent execution of the RPCs they cover, but neither reliably detects a reintroduced non-atomic split of the critical section it guards — empirically confirmed by manually reverting each fix and observing the corresponding test still pass 5/5 isolated runs. Root cause: this crate's single-threaded `current_thread` test runtime never preempts a task mid-poll except at a genuine `Pending` suspension, so once both tasks are queued on the externally-held lock these tests use to force overlap, the first-queued waiter always runs its entire remaining sequence — including the operation whose ordering-vs-the-other-task actually matters — to completion before the second task is polled again at all, collapsing the intended race into a deterministic, bug-shape-insensitive ordering. Both tests still close the literal "no test exercises this interleaving at all" gap they were added for and remain meaningful regression coverage for the CURRENT, correct code. Closing the deeper gap would need a test-only pause/gate hook inside `rpc_subscribe_event`/`rpc_create_com_logical_link` themselves (the same class of infrastructure this file's other infeasibility notes describe) — judged not worth the invasiveness; see each test's own doc comment for the full empirical trace.
- **P3** (ADR-115 round 7, `edge-case-hunter` finding on round 6's `terminate_all_subscriptions` fix, test-coverage gap not a design residual — the fix itself, `rpc_module_disconnect` capturing every CLL's queue `Arc` before clearing `logical_links` and passing it to `terminate_all_subscriptions`, mirrors the verified-correct `terminate_subscription`/`rpc_destroy_com_logical_link` fix exactly, just for the multi-CLL case): no end-to-end test drives `live_sender` clearing through the real `ModuleDisconnect` RPC path the way `rpc_subscribe_event_live_sender_tests::destroy_com_logical_link_clears_live_sender_via_the_real_call_path` does for `DestroyComLogicalLink`. Setting one up needs an open device slot (`rpc_module_disconnect` calls `self.lock_device_for(requested_handle)` first, which `service_with_one_link`'s minimal test harness doesn't currently provide), a heavier fixture than this file's other `service_with_one_link`-based unit tests use. Not chased here given the fix's structural identity to the already-verified single-CLL case; trivial to add if `rpc_module_disconnect` is touched again and a device-slot test fixture already exists or is worth building for another reason.
- **P3** (ADR-117's `CopEntry` refactor, design-advisor review, PR #124 — pre-existing, NOT introduced by that refactor): `cancelled_cops` (unlike the now-retired `dispatched_cops` side-set ADR-117 originally added and then abandoned in favor of folding `dispatched` into `primitives`'s own `CopEntry` value) is not cleaned up in `schedule_continuation`'s requeue-failure branch (`events.rs`, the `tx_requeue.send(cont.item).is_err()` arm) or in `poll_channel_events`'s final teardown drains (the `parked` loop and the `tx_rx.try_recv()` loop) when a cyclic COP is finalized there — the same permanent per-CLL `HashSet<u32>` leak shape `dispatched_cops` had before this refactor, since `LogicalLinkState` is never dropped from `logical_links` for a CLL's lifetime. No correctness impact (`cancelled_cops` is never consulted independently of `primitives`/`registrants` state, and each of these sites already correctly removes the `primitives`/`CopEntry` entry), just an unbounded-but-harmless leaked `u32` per COP finalized through either path. Discovered during ADR-117's design-advisor review but out of scope for that fix, since `cancelled_cops` is a separate, older mechanism than the `dispatched_cops`/`CopEntry` question that ADR revisits. Not chased here; trivial to close by mirroring the `cancelled_cops.remove`/`.clear()` calls already present at every other finalization site if either function is touched again.
- **P3** (design-advisor latent-gap flag, A2-11 fix, explicitly deferred): ISO 22900-2's
  `CP_CanMaxNumWaitFrames` range is `[0, 1027]`
  (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:5992`), but
  the native J2534 `PassThruIoctl(SET_CONFIG)` IOCTL only accepts an 8-bit value
  (`0x0`-`0xFF` per J2534-1 Table B.20,
  `vehicle-comm-specs/j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru
  Vehicle Programming.md:1264`). A client setting a value above 255 on a *hardware* channel
  is only bounded by whatever the native library/device does with an out-of-range `SCONFIG`
  value (untested here). As of ADR-124 (superseding ADR-121), the software-ISO-TP path no
  longer reads this comparam at all — its former ADR-121 enforcement was in the wrong
  direction and was replaced with a fixed internal constant unrelated to
  `CP_CanMaxNumWaitFrames`'s configured value — so this native `SET_CONFIG`-forwarding path
  is now this comparam's sole remaining point of relevance in this codebase, and the
  8-bit-cap-vs-`[0,1027]` divergence described above is unchanged and still unresolved. Not
  fixed here — recorded as a latent native-path gap, out of scope for A2-11.
- **P3** (design-advisor round-6-candidate flag, ADR-123 Codex review round 5, Finding I fix, explicitly out of scope for this round): `handle_channel_hard_error` deliberately preserves a hard-errored CLL's `channel_key`/`uudt_channel_key` so a later Disconnect/Destroy can still release the `shared_channels` ref-count -- but the dead `SharedChannel` entry itself stays in `shared_channels` (with its poll task already exited) until that later Disconnect/Destroy runs. A new CLL connecting in the meantime with identical `(protocol_id, baud_rate)` (`ChannelKey`) could theoretically match that stale key via the normal ref-count-bump path in `rpc_connect_com_logical_link`/`finalize_connected_link` and enqueue `TxItem`s to a `tx_queue` nobody is draining. Not fixed or verified this round -- worth checking whether `rpc_connect_com_logical_link`'s channel-matching logic already guards against reusing a hard-errored channel's key (e.g. by validating the channel is still live before matching), or whether this is a latent, separate gap. Recorded here so a future audit doesn't need to rediscover it.
- **P3** (conformance-audit fix A2-26, ADR-120, accepted residual, not fixed): `events::module_timestamp_us()`'s wraparound behavior at 2^32 µs (~71.6 min) has no test coverage — forcing a real wraparound deterministically would need a test-only injectable start `Instant` (a seam the function does not currently have — it owns its own process-lifetime `OnceLock`) or an unacceptably slow real-time test. Its monotonicity contract (repeated calls within one process are non-decreasing) is now covered by `events_timestamp.rs`'s own `module_timestamp_us_is_monotonic_across_back_to_back_calls` unit test. Trivial to close by giving `module_timestamp_us()` an injectable-start test seam if wraparound coverage is ever judged worth the added complexity.
- **P3** (conformance-audit fix A2-7, ADR-129, explicitly accepted residual; corrected 2026-07-24 per an `edge-case-hunter` review of the PR #139 concurrency fix, which caught that this bullet described code that doesn't exist): `LogicalLinkState::pending_client_filters` (filters configured via `PDU_IOCTL_START_MSG_FILTER` before `PDUConnect`) is *not* touched by `rpc_disconnect_com_logical_link` at all (it only drains `client_filters`, `rpc_link.rs`) — `pending_client_filters` is simply always already empty by the time a disconnect runs, since `ConnectComLogicalLink` unconditionally drains it (into `client_filters` on success, or leaves it untouched if the connect itself is rejected) before `connected` ever becomes `true`, and nothing else can repopulate it while connected. The net effect is the same one this bullet originally described, though: ISO 22900-2 §9.5.13's own text ties filter *deletion* only to `PDUDestroyComLogicalLink`, not `PDUDisconnect`, so a client that pre-configures filters, connects, disconnects, and reconnects the same `cll_handle` today must re-issue `START_MSG_FILTER` after every reconnect (its earlier hardware-installed filters were stopped and discarded at disconnect, per existing pre-A2-7 policy) rather than having them persist and reinstall automatically. Not implemented in the A2-7/ADR-129 work — would need disconnect to capture the stopped `client_filters` back into `pending_client_filters` instead of discarding them, plus deciding whether a re-`PDUConnect` re-validates them against a possibly-different `hw_protocol_id`/channel. Recorded here per this file's backlog-durability convention rather than expanding ADR-129's scope.
- **P3** (conformance-audit fix A2-7, ADR-129's concurrency amendment, PR #139, `edge-case-hunter` review, test-coverage gap not a design residual): no regression test exercises the race Codex's review found and the PR #139 fix closes (a filter IOCTL landing in the gap `rpc_connect_com_logical_link` used to leave between its `pending_client_filters` snapshot and `finalize_connected_link`'s fold). Confirmed genuinely infeasible with this crate's current single-threaded mock harness, the same class as the other infeasibility notes in this file: the entire `shared_channels`-held span the race depended on (`connect_new_physical_channel` through `install_client_message_filters`, `rpc_link.rs`) has zero internal `.await` yield points — every native call inside it is synchronous Rust — so nothing suspends for a concurrent `ioctl_start_msg_filter`/`_stop_msg_filter`/`_clear_msg_filter` task to interleave into, even before this fix closed the gap. The fix's correctness was instead verified by an `edge-case-hunter` code-inspection pass tracing every `channel_id`/`pending_client_filters` mutation site to confirm all of them now serialize on `shared_channels` consistently. Separately noted by the same review, lower severity and out of scope for this fix: `ioctl_reset`'s `client_filters` snapshot (`rpc_misc.rs`) is still taken under `logical_links` alone, not `shared_channels` — unlike the three IOCTLs this fix touches — so a `PDU_IOCTL_RESET` landing between `ioctl_start_msg_filter`'s hardware install and its `client_filters` write won't see (and so won't stop) a filter installed in that same window. This is a "ran logically-before" linearization gap, not a client-visible success/state contradiction like the original finding, and `ioctl_reset` never touches `pending_client_filters` at all (confirmed), so it's unaffected by the concurrency guarantee this fix otherwise establishes. Not chased here; would need `ioctl_reset` to also acquire `shared_channels` before its snapshot if ever revisited.
- **P3** (conformance-audit fix A2-7, ADR-129, adversarial-review follow-up on PR #139 substituting for an unavailable Codex round, test-coverage gap not a design residual): the connect-time filter-install *rollback* path (`rpc_connect_com_logical_link`, `rpc_link.rs`: a native `PassThruStartMsgFilter` failure during connect must `api.disconnect(channel_id)` the just-created channel, leave `pending_client_filters` intact, and return `FCT_FAILED`) has no regression test — `j2534-0404-mock` has no error-injection hook for `PassThruStartMsgFilter` itself (only `__mock_set_stop_filter_error` exists, `j2534-0404-mock/src/lib.rs`), the same shape of gap already recorded elsewhere in this file for other native-call failure paths this crate's mock can't inject. Closing this needs a new `__mock_set_start_filter_error`-style hook, a mock-infrastructure change out of scope for the A2-7 PR. (This bullet used to also flag `ioctl_start_msg_filter`'s empty-`PDU_IO_FILTER_LIST` rejection and its ISO15765 `hw_protocol_id` rejection (ADR-038) as untested pre-connect or connected; both are now covered by `tests/grpc_mock/pdu_ioctl.rs`'s `start_msg_filter_rejects_an_empty_filter_list` and `start_msg_filter_rejects_pass_block_filters_on_an_iso15765_link` tests.)
- **P3** (conformance-audit fix A2-13, ADR-130, `edge-case-hunter` pre-merge review of PR #142, pre-existing bug of the same class found while checking for recurrences, explicitly out of scope): `rpc_get_com_param`'s Bytefield-fallback fix (ADR-130's Amendment 1, `comparam_support::is_bustype_bytes_param`) only covers `BUSTYPE_BYTES`-class params (currently just `CP_CanBaudrateRecord`). The identical getter defect -- falling through to `Unum32(0)` instead of the correct `Bytefield` oneof for a Bytefield-typed param with no populated default -- independently affects `PARAM_TESTER_PRESENT_MSG` (`CP_TesterPresentMessage`, id `0x8001`) on the `SAE_J2610_on_SAE_J2610_SCI` resource (`comparam_defaults.rs::sae_j2610_on_sae_j2610_sci`, live/reachable via resource IDs `0x021E`-`0x0221` and protocol names `"sae_j2610_on_sae_j2610_sci"`/`"sci_mode"`, confirmed against the resource table and `map_protocol_name`): `is_sci_param` allows `GetComParam`/`SetComParam` of this param for SCI, but the preset never seeds a `bytes` default for it, unlike its sibling `PARAM_CHANGE_SPEED_MSG` in the same function, which does get an explicit empty default. Repro confirmed by `edge-case-hunter` (a scratch `CreateComLogicalLink(0x021E)` + `GetComParam(0x8001)` returned `Unum32(0)`, not `Bytefield([])`; no test or code change left behind). Not fixed in the A2-13/ADR-130 PR: this resource/param combination is unrelated to that finding's scope (a different bus family, a different ComParam, discovered only as a by-product of checking whether the same bug class recurs elsewhere). Closing it would need either seeding an explicit empty `bytes` default for `PARAM_TESTER_PRESENT_MSG` in `sae_j2610_on_sae_j2610_sci` (mirroring `PARAM_CHANGE_SPEED_MSG`'s own handling there), or widening `is_bustype_bytes_param`'s registry concept to a general "known Bytefield param with no populated default" check independent of `BUSTYPE_BYTES` class membership.
- **P3** (ADR-134 Correction, Codex review round 2 on the same PR, test-coverage gap not a design residual): `rpc_link.rs::tests::probe_sae_j1850_flavor_skips_the_active_probe_when_module_marked_not_avail` (pinning the Correction's new gate inside `probe_sae_j1850_flavor`) cannot fully prove "zero native `PassThruConnect`/`PassThruWriteMsgs` calls occurred" -- only that the function returns `Fallback(J1850VPW)`, which a genuinely-run-but-inconclusive probe against the mock's default silent bus would also return. Distinguishing "gate short-circuited" from "probe ran for real and was inconclusive" needs a `MockBackdoor`-style raw FFI dlopen of the mock library from within this unit-test module (`j2534-0404-mock`'s plain Rust counters read the `rlib`'s own separate copy of the process-global mock state, not the dynamically-loaded `cdylib` instance the service actually calls into -- see `tests/grpc_mock/harness.rs::MockBackdoor`'s doc comment). The fix itself was verified correct by direct code inspection (a plain early return, structurally identical to the pre-existing "could not open device" branch the same function already has) and the full existing suite passing unchanged. Closing this would need either porting `MockBackdoor`'s dlopen approach into `src/`'s unit tests, or moving this specific case to the `tests/grpc_mock/j1850_autodetect.rs` integration suite -- which itself would first need a new error-injection hook to trigger a genuine `NotAvail` transition through the public RPC surface, since no such hook exists today.
- **P3** (conformance-audit fixes A2-17/A2-18, ADR-133, design-advisor review + `edge-case-hunter` pre-merge review, pre-existing three-way contradiction found while classifying `PARAM_TESTER_PRESENT_IMMED`, explicitly deferred — dependency note for whoever picks this up): `PARAM_TESTER_PRESENT_IMMED` (`CP_TesterPresentImmed`, id `0x80B1`, `service_params.rs`) is a dead param with three mutually contradicting sources: `comparam-protocol-support.md:333` documents it `S` (supported) for CAN/KWP/J1850/SCI; `comparam_defaults.rs` seeds it into ~10 protocol presets' Working sets; but it is absent from every one of `is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/`is_j1850vpw_param`/`is_sci_param` (`comparam_support.rs`), so `GetComParam`/`SetComParam` for it is rejected `PDU_ERR_COMPARAM_NOT_SUPPORTED` on every real (non-unknown) protocol today, and — confirmed by `edge-case-hunter` — nothing in this crate ever reads the seeded value at all (no runtime consumer besides the const declaration and, now, ADR-133's `TESTER_PRESENT_CLASS_PARAMS` classification list). Today's behavior is safe-conservative (a clean rejection, not silent misbehavior), so not fixed here — A2-17/A2-18's own scope is `GetComParam`'s class-reporting/typed-empty-fallback correctness, not this allowlist gap, and design-advisor confirmed the gap is orthogonal to (and does not block) that fix. Two exits, both leaving `TESTER_PRESENT_CLASS_PARAMS` internally consistent: (1) **wire it up** — add it to the relevant per-protocol allowlist(s) per `comparam-protocol-support.md`'s `S` row, and make the tester-present-arming path at `CoptStartcomm` actually consult it (the in-workspace ISO 22900-2:2009(E) text, lines 2141/2229, mandates unconditional immediate send with no such configurability — verify against the 2022 edition, which this repo's `docs/j2534-0404-architecture.md` targets, before assuming this param is spec-legitimate rather than a vendor artifact); or (2) **remove it** — drop the ~10 preset seeds, the `comparam-protocol-support.md:333` row, the `service_params.rs` const, and its `TESTER_PRESENT_CLASS_PARAMS` entry together. Whoever picks this up should treat the class-list membership as following whichever exit is chosen, not as a separate decision.
- **P3** (conformance-audit fix A2-18, ADR-133, design-advisor review, pre-existing dead-seed question found while designing the `CP_ExtendedTiming` empty-fallback shape, explicitly deferred): the 4 KWP-family presets in `comparam_defaults.rs` (`iso_14230_3_on_iso_14230_2`, `iso_15031_5_on_iso_14230_4`, `sae_j2190_on_iso_14230_2`, `iso_obd_on_k_line`) each seed `PARAM_EXTENDED_TIMING` (`CP_ExtendedTiming`) with `access_timing_zero()`'s one-all-zero-entry shape, but `is_kwp_param` (`comparam_support.rs`) does not include `PARAM_EXTENDED_TIMING` in its allowlist — only `is_can_param` does. So these 4 seeds are unreachable via `GetComParam`/`SetComParam` for any of these presets' connected CLLs (`check_param_allowed` rejects `CP_ExtendedTiming` with `PDU_ERR_COMPARAM_NOT_SUPPORTED` before the seeded value could ever be read or written). Not investigated or changed here — this fix's scope was the `GetComParam` fallback shape for protocols where the param genuinely is allowed but unseeded (confirmed reachable only on the CAN family, which is what the new `get_com_param_unseeded_structfield_reports_correct_empty_shape` regression test exercises); whether the KWP allowlist should include `CP_ExtendedTiming` (making these 4 seeds live) or the seeds are simply dead code that should be removed needs its own investigation, not a ride-along fix.
- **P3** (conformance-audit fixes A2-17/A2-18, ADR-133, Codex review round 3 on PR #147, declined-finding follow-up, pre-existing, explicitly deferred): `CoptUpdateparam` (`events.rs`'s `apply_bustype_lock` consumer, ~line 7874-7899) has no `bustype_params_differ`-style gate of its own — it never checked it before this PR and still doesn't. A client that reads an unseeded `BUSTYPE_UNUM32` param's default via `GetComParam` (reports `0`) and writes it straight back via `SetComParam` (a normal round-trip) leaves an explicit `Working` entry with no `Active` counterpart; if that client then issues `CoptUpdateparam` while not locked by another CLL, `apply_bustype_lock`'s unfiltered `hw_set` (the `!locked_by_other` branch, `comparam_support.rs`) pushes that explicit `0` to hardware via a real `SET_CONFIG` call. Root cause is `GetComParam` reporting `0` rather than "absent" for an unseeded BUSTYPE Unum32 param (a residual of the A2-17/A2-18 typed-reporting model, not something PR #147's `temp_param_update` differ fix touches — `CoptUpdateparam` is a different COP type with no differ gate at all). Not fixed here: closing it would need `apply_bustype_lock`/`CoptUpdateparam` to gain its own "does this push actually change anything effective" check (mirroring `effective_unum32`/`effective_bytes`), a design question of its own scope (would it also need to distinguish "the client explicitly wants hardware set to 0" from "the client blindly echoed a default" -- those are not the same intent, unlike the `temp_param_update` gate's simpler reject-or-allow decision).
- **P3** (conformance-audit fix A2-14 follow-up; narrower residual after Codex review, PR #79 — the general coverage gap this bullet originally described is now closed): `hard_channel_error_populates_error_event_data_on_a_later_unrelated_rpc` (`tests/grpc_mock/modules.rs`, using the `__mock_set_read_msgs_error` backdoor) now proves a hard channel error's tracked `last_error` surfaces in a later, unrelated RPC's `ErrorDetail.error_event_data`, closing the "no test anywhere asserts `ErrorDetail.error_event_data`" gap this bullet used to flag. What remains untested is narrower: no test specifically forces a concurrent `last_error` update to land inside the exact intervening-`.await`/lock-reacquisition window the original A2-14 bug (and its later `ioctl_start_msg_filter`/`rpc_connect_com_logical_link`/`ensure_uudt_companion_channel` `sc.dead`-rejection siblings, each also noted elsewhere in this file as shipping without their own regression test) all shared — i.e. proving a *fresh-read* fix actually beats a *stale-snapshot* bug, not just that the field gets populated at all. Would need new harness plumbing to inject a `last_error` update timed to land between a rejection path's lock release and its later reuse point.
- **P3** (accepted residual, narrowed by the A2-14 fix to `rpc_link.rs`'s `sc.dead` rejections, not eliminated): `events::handle_channel_hard_error` still holds `shared_channels` through its per-CLL loop and only acquires `module_state` after releasing it, so a connect racing the same writer can observe `sc.dead == true` but still report a `last_error` from just before this specific hard error's own `module_state` update -- diagnostic-only staleness (the dead-channel `NotAvail` rejection decision itself is always timely and correct). Already-narrow, likely-untestable window; not chased further.
- **P3** (conformance-audit fix A3-4, found while auditing `PDU_ERR_INVALID_HANDLE`'s outer gRPC code, out of scope): `require_module_handle` (`service.rs`) rejects an out-of-range `module_handle` via `state_guard_status(Code::InvalidArgument, ..., PduError::PduErrInvalidHandle, None)` — a *third* outer code for this one `PduError`, alongside the native-call path's `Code::NotFound` (`map_native_error_as`, fixed by A3-4) and the emulated unrecognized-handle path's `Code::NotFound` (`unknown_handle_status`). Arguably deliberate (an out-of-range `module_handle` is closer to a caller input-validation error than "this reference used to be valid but isn't anymore"), but A3-4's own scope was the native-vs-emulated pair the conformance audit specifically named (`error.rs:146` vs `:160`); this third call site was not part of that finding and is left unchanged here. Needs its own investigation before deciding whether to fold it into the `NotFound` convention or document the `InvalidArgument` choice as intentional.
- **P3** (ADR-147 correction round, design-advisor finding, pre-existing, out of scope): `bind_registrant`'s step-2 pending-RC check (`RcHandlingConfig::detect_pending_rc`) can BIND a tester-present reply that also independently matches `bind_frame`'s later step-3 tester-present-discard signature — per ADR-100's own "step 2 outranks step 3" precedence, a `request_sid: None` (or coincidentally-matching) tester-present exchange whose reply happens to carry a pending-RC-shaped NRC (0x78/0x21/0x23) at the configured `rc_byte_offset` gets claimed by an unrelated registrant's pending-RC wait instead of ever reaching the tester-present check, extending that unrelated COP's wait time. This predates the `CP_SuspendQueueOnError` (ADR-147) work entirely — it is a pending-RC/tester-present precedence gap, not a queue-suspension bug — and was not fixed as part of correcting ADR-147's three queue-classification bugs. Needs its own investigation (e.g. whether step 3 should be allowed to intercept a would-be pending-RC claim when the frame also matches a live discard window, or whether this is an accepted consequence of ADR-100's step ordering) before any fix is attempted.
- **P3** (ADR-147 deadlock-fix follow-up, design-advisor + edge-case-hunter, optional, not built here): a deterministic regression test for the SAE J1850 autodetect-retroactive-lock-suspension residual (documented above) is constructible if the mock harness gains probe-outcome injection for `autodetect_sae_j1850_flavor` — a not-yet-connected J1850 CLL acquiring `LOCK_PHYSICAL_TX_QUEUE` before its flavor resolves, then resolving to share a resource with another CLL that has a live transmitting COP, retroactively suspending that COP's CLL under the lock. Would give the wake-widening/middle-pop fix (and ADR-123's own "Finding G" retroactive-recompute machinery generally) its first direct test coverage, rather than the structural-argument-only justification currently in place. Not attempted in this PR — offered as a follow-up for whoever next touches the SAE J1850 autodetect path or the mock harness's injection capabilities.
- **P3** (ADR-140, design-advisor review on PR #3's second Codex round, non-blocking sweep suggested but not performed): after fixing `ioctl_set_event_queue_properties`'s connect-race gap (holding `logical_links` from its `pdu_connect_begun()` gate check through to the queue mutation it guards, restoring ADR-126's original check-and-write atomicity), design-advisor confirmed this is the only site of this *exact* mechanism (a `logical_links`-derived gate defeated by releasing the lock before acting on it) but flagged a broader, unaudited class worth a follow-up sweep: any site in this crate that reads a `logical_links`-derived predicate (`link.connected`, `comm_started`, `pdu_connect_begun()`, or similar), releases `logical_links`, and then *mutates persistent service state* (not merely calls hardware — hardware-call sites are best-effort by design and already covered by the disconnect-race ADRs) conditioned on that earlier read. Not performed here — design-advisor's own framing was "report sites, don't fix," i.e. a reconnaissance pass, not a scoped fix; needs a dedicated `code-scout`/`edge-case-hunter` sweep to enumerate candidate sites before any of them can be judged worth closing.
- **P3** (ADR-146, design-advisor flag during the round-7 `store_combined_timing_change` fix, PR #17, explicitly out of scope for that fix): `handle_start_comm`'s post-5-baud-init `CP_Baudrate` write-back (`events.rs`, the "Guard B" recheck right after `api.get_config_u32(ctx.channel_id, j2534_0404::DATA_RATE)` succeeds) writes the negotiated baud rate into both `link.working.unum32` and `link.active.unum32` unconditionally, gated only by the same `channel_id`/`connect_generation` recheck ADR-086 established elsewhere — with no per-key protection against a concurrent `SetComParam(CP_Baudrate)` landing on `link.working` during the `get_config_u32` await window. This is the identical lost-update shape ADR-146's round-7 fix just closed for the KWP Access Timing mechanism (`store_combined_timing_change`'s new `WorkingTimingSnapshot` first-wins guard): a concurrent client write to `link.working` for the SAME key, made while the hardware read/push runs without holding `logical_links`, can be silently clobbered moments later when this write-back re-acquires the lock and stores unconditionally. Flagged by design-advisor during that fix's audit as a likely future Codex finding on this OTHER mechanism if left unaddressed. Not fixed in PR #17, since it is out of that PR's scope (`CP_ModifyTiming`/Access Timing specifically, not the 5-baud-init path) — but the identical fold-time-snapshot-and-compare mechanism (`WorkingTimingSnapshot`) that PR built could be applied here too: snapshot `link.working.unum32.get(&DATA_RATE)` before the `get_config_u32` await, and only overwrite Working at store time if it is still unchanged.
- **P3** (ADR-086 round-11 amendment, accepted residual, preserved from a completed backlog entry removed during the 2026-08-18 backlog cleanup; Codex review finding, PR #79): `rpc_link.rs`'s `promote_unique_resp_id_table` (called from `handle_update_param` on an ISO15765 link with a changed UniqueRespIdTable) has its own internal window during its `FLOW_CONTROL_FILTER` teardown/install `.await`s -- a reconnect completing mid-call can re-land the same Active-table write onto the new session, since the helper itself has no `connect_generation` check (the caller's post-promotion recheck only guards the tester-present re-arm block and terminal status that follow, not the write already made during the call). Judged not worth the plumbing when this was found: the new session's own connect already installs its own filters independently, so the stale write is superseded, not silently load-bearing. Revisit if this proves reachable in a way the superseding connect doesn't cover.
- **P3** (ADR-148 fifth Amendment, `edge-case-hunter` pre-merge review of the Codex round-6 `Absorbed`/`PendingRc` fix, test-coverage gap not a design residual): the conditional-`pending_rc`-reset fix and its cap-triggered-`Matched`-still-clears-it composition are both pinned by unit-level tests (`observe_and_consume_pending_rc_outcome_tests`/`bind_frame_tests` in `events.rs`) exercising `observe_and_consume_pending_rc_outcome`/`bind_registrant` directly, but no `tests/grpc_mock/` integration test drives the full async `wait_for_expected_response_inner` poll loop through a concat-plus-pending-RC scenario end-to-end (confirmed: `tests/grpc_mock/concat.rs` never references RC handling and `tests/grpc_mock/rc_handling.rs` never references concat — zero file overlap). The unit-level coverage traces the exact mechanism this fix touches and is sufficient to catch a regression in the reset logic itself, but does not independently confirm the "preserved RC surfaces as `PendingRc` on the very next poll pass, which is immediate" claim at the real async/poll-task level (only unit-tested via a second manual `observe_and_consume_pending_rc_outcome` call simulating that next pass). Not added here — would need a new integration test injecting both an absorbable continuation frame and an NRC frame for the same registrant in one `PassThruReadMsgs`-equivalent mock batch, then asserting the eventual `ResultData` delivery timing reflects the RC's P2*/RC21-23 extension rather than timing out on the shorter `CP_P2Max` window.
- **P3** (ADR-148 sixth Amendment, `edge-case-hunter` pre-merge review of the Codex round-7 delivery-folding fix, pre-existing, not concat-specific, not fixed here): the round-7 fix closed a "resolve `rx_buf` under `logical_links`, release, deliver later" await-gap race for the concat delivery path specifically, but the same shape still exists in two OTHER sites in `events.rs`: `poll_rx_inner`'s per-frame delivery fan-out (which snapshots `entry.rx_buf` ahead of a delivery loop containing further `.await` points) and the fast-init synthetic-frame delivery (which re-resolves under a generation check immediately before delivering, but still has a residual window between that check and the delivery itself). Both predate this PR (ADR-100/ADR-115 era) and were confirmed narrower in consequence than the pre-Amendment-6 concat bug: `poll_rx_inner`'s own `merge_registrant_writeback` discards its entire per-pass delta, including `matches_got`, whenever the live `connect_generation` no longer matches the pass's own snapshot, so a race there cannot falsely advance a live COP's completion count — only a stray frame can land in a since-invalidated `rx_buf`. Not fixed here — out of scope for the `CP_EnableConcatenation` fixes, which only needed to close the concat-specific instance. Needs its own investigation/sweep: whether `poll_rx_inner`'s fan-out and the fast-init synthetic-frame site should adopt the same "resolve and deliver under one lock acquisition" shape ADR-148's sixth Amendment established for the concat path, or whether their narrower blast radius (no `matches_got` corruption, only a possible stray delivery) makes that not worth the added `logical_links` hold time for those call sites.
- **P2** (ADR-191, deferred narrowing of the former "RxStatus bits 16-18 forwarding" item): SAE J2534-2 clause 9's `SW_CAN_HS_RX`/`SW_CAN_NS_RX` (RxStatus bits 17/18, SW-CAN speed-transition confirmations) remain withheld entirely (ADR-172, unaffected by ADR-191) and unforwarded into `RxFlag`. ADR-191 judged the equivalence to ISO 22900-2's `SPD_CHG_EVENT` (`RxFlag` byte 1 bit 2, Annex D.2.2 Table D.5) genuine but unconfirmed beyond wording parallelism — a future implementer should re-check both specs' surrounding prose (clause 9.3.2.3's own transition-sequence text; ISO's own note that the message data of such a frame may carry the monitored Change Speed message) before committing to it. Even once confirmed, this needs new delivery machinery, not a bit assignment: these frames are withheld before any content path runs, so forwarding needs a flag-only-frame delivery shape with no CAN ID, no `UniqueRespIdTable`/COP match, and a recipient-set decision (a speed transition is physical-bus-wide; an automatic transition has no single commanding CLL) — closer to ADR-146's `ECU_TIMING_CHANGE` synthesis than to `rx_flag_bytes`'s existing `RxFlagExtras` parameter shape. `SW_CAN_HS`/`SW_CAN_NS`'s own success/failure remains separately observable via the triggering `IoCtl` RPC's own return status; only the passive, transition-*confirmation* RX signal is missing. `SW_CAN_HV_RX` (bit 16), FT-CAN's `LINK_FAULT` (bit 17), TP2.0's `CONNECTION_ESTABLISHED`/`_LOST` (bits 16/17), and SAE J1939's `ADDRESS_CLAIMED`/`_LOST` (bits 16/17) are all resolved (forwarded, or explicitly decided against) by ADR-191 and no longer tracked here.
- **P3** (ADR-165, Phase 12, Consequences section, accepted residual, not a defect): SAE J2534-2 clause 14's repeat-message slot budget (`MAX_REPEAT_SLOTS_PER_CHANNEL` on the mock, ≥10 per the spec's own minimum) is tracked per physical channel, not per CLL — `require_owned_repeat_message` scopes `MsgId` *ownership* per-CLL, but two sibling CLLs sharing one physical channel (`SharedChannel::ref_count > 1`) still draw from the same underlying device-side slot pool, so `PDU_ERR_EXCEEDED_LIMIT` on `START` can surface to whichever client's call happens to cross the shared limit first, regardless of how many slots that particular CLL itself has started. Accepted per ADR-165's Consequences section; revisit only if this proves surprising in practice (no fix planned — the device itself has no notion of CLL identity to partition the budget by).
- **P2** (found during ADR-201 verification -- clause 6 Pin Selection general secondary-pin validation -- orchestrator cross-check of `resources.rs`'s SCI pin tables against SAE J2534-1's own Figure 3 pin-usage table, not fixed here): `PINS_SCI_A_ENGINE`, `PINS_SCI_B_ENGINE`, and `PINS_SCI_B_TRANS` (`resources.rs`, lines ~176-179) appear to have their Tx/Rx pin roles reversed relative to Figure 3. Figure 3 lists pin 6 as SCI A Engine Rx and pin 7 as (among other roles) SCI A Engine Tx, but `PINS_SCI_A_ENGINE` is `&[(6, PIN_TX), (7, PIN_RX)]`; Figure 3 lists pin 12 as SCI B Engine Rx and pin 7 as (among other roles) SCI B Engine Tx, but `PINS_SCI_B_ENGINE` is `&[(12, PIN_TX), (7, PIN_RX)]`; Figure 3 lists pin 9 as SCI B Trans Rx and pin 15 as SCI B Trans Tx, but `PINS_SCI_B_TRANS` is `&[(9, PIN_TX), (15, PIN_RX)]`. SCI A Trans is deliberately excluded from this claim: Figure 3's own raw text for that row is internally ambiguous (pins 7 and 14 both appear to list a Tx role, possibly a PDF-extraction/merged-cell artifact rather than a genuine spec conflict), so `PINS_SCI_A_TRANS`'s `&[(14, PIN_TX), (7, PIN_RX)]` is not independently confirmed against the spec table either way. This only concerns default hardware pin *role* labeling, not pin count/completeness -- ADR-201's `secondary_pin_requirement` predicate cares only about how many pins are supplied, not which specific pin plays which role, so it is unaffected by this finding. Not fixed here -- risky to correct speculatively given the spec table's own internal ambiguity for one of the four SCI configurations; needs verification against real hardware, or a cleaner copy of Figure 3, before any pin-table edit. Done when: the Tx/Rx roles of all four SCI pin tables in `resources.rs` are confirmed or corrected against that evidence, with the source cited. Blocked on: a real SCI-capable J2534 device, or a cleaner copy of SAE J2534-1 Figure 3 than the converted text.
- **P3** (ADR-200 Phase 3, accepted residual, vendor-hardware-only, not testable in this repo):
  what CAN ID a reassembled multi-frame SAE J1939 RX response (a real ECU's BAM/RTS-CTS
  multi-packet response, clause 16.4.5) reports on a RawMode CLL is unspecified by that clause
  and is passed through untouched by this service, which performs no J1939 transport-layer
  reassembly of its own. `j2534-0404-mock` implements no J1939 multi-frame reassembly logic at
  all, so there is no reassembly-specific CAN-ID convention this repo can pin beyond what
  `tests/grpc_mock/j1939_raw_mode.rs`'s own RX test already fixes (a single injected frame's
  CAN-ID bytes pass through the destination-address-drop transform unchanged, byte for byte). A
  real vendor DLL's reassembled-response CAN ID is genuinely unverifiable without real J1939
  hardware. See ADR-200's Consequences section for the full analysis.
  Done when: the observed CAN ID is recorded in ADR-200's Consequences and the RawMode RX path
  matches it. Blocked on: real J1939 hardware and a vendor DLL to observe which CAN ID a
  reassembled multi-frame response reports on a RawMode CLL.
- **P3** (ADR-185, found while scoping Stage 1/Stage 2's per-phase parameter mapping, not part of
  either stage's own call-site list): each Stage-1-wired family's `DEVICE_INFO_*_SIMULTANEOUS`
  companion bit (SWCAN/FT-CAN/UART Echo Byte/Honda DIAG-H/J1708/Analog Inputs/GM UART, ADR-189/Phase
  8) is never consulted -- both stages only check the flat `_SUPPORTED` flag. No assigned phase;
  would need its own `DiscoveryCheck`/call-site mapping if picked up.
- **P3** (ADR-185, found while scoping Stage 1/Stage 2's per-phase parameter mapping, not part of
  either stage's own call-site list): SAE J1708's clause-6 `DEVICE_INFO_J1708_PS_J1962`/`_J1939`/
  `_J1708` connector-validity queries have no Discovery-cache consumer. No assigned phase.
- **P3** (ADR-189/Phase 8, deliberate scope exclusion this phase, not investigated further): SAE
  J2534-2 clause 11 GM UART's own per-pin validity query, `DEVICE_INFO_GM_UART_PS_J1962`, has no
  Discovery-cache consumer -- the same unwired-per-pin-query shape SAE J1708's own
  `DEVICE_INFO_J1708_PS_J1962`/`_J1939`/`_J1708` entry above already tracks, distinct query
  parameter, same residual. No assigned phase.
- **P3** (ADR-189/Phase 8, deliberate scope exclusion this phase, not investigated further):
  `comparam_support.rs`'s `is_param_allowed` gets no `GM_UART_PS`-specific allowlist branch --
  unlike every other standalone protocol (UART Echo Byte/Honda DIAG-H/J1708/J1939/TP2.0, each of
  which has a dedicated closed or near-universal allowlist), a GM UART link falls through to the
  generic "Unknown protocol — allow" tail, so every ComParam (not just `DATA_RATE`) stays
  `SetComParam`/`GetComParam`-reachable on it. Clause 11 defines no ComParam concept at all
  (unlike every sibling protocol's own Win32 API section), so whether a dedicated, narrower
  allowlist is even warranted is itself an open question ADR-189 left unresolved -- not fixed here.
  No assigned phase; would need a `design-advisor` consult (or a spec-clarity finding) to decide
  whether a closed allowlist is warranted at all before implementation starts.
  Done when: the decision is recorded in an ADR, and `comparam_support.rs` has the allowlist if
  one is warranted. Blocked on: the maintainer's decision on whether GM UART needs a closed ComParam
  allowlist in `comparam_support.rs` (ADR-189 left it unresolved).
- **P3** (found by `edge-case-hunter`'s review of the round-1 `BECOME_MASTER` `spawn_blocking` fix,
  PR #98, test-coverage gap, not fixed): no test exercises the actual concurrency property
  `ioctl_become_master`'s `spawn_blocking` dispatch provides -- that a concurrent RPC needing
  `shared_channels` or `self.api` on an unrelated channel/CLL no longer stalls for the duration of
  a `BECOME_MASTER` call. The existing `gm_uart.rs` tests only assert success/failure/rejection
  outcomes against the mock, which all return instantly, so none of them can distinguish "runs
  concurrently" from "blocks everything else." `j2534-0404-mock` has no delay-injection hook for
  `IOCTL_BECOME_MASTER` (unlike some other IOCTLs' own `__mock_set_*_error`-style backdoors) to make
  such a test cheap; would need one plus a `tokio::join!`-driven test asserting a second, unrelated
  RPC completes before the delayed `BECOME_MASTER` call does. Not attempted here.
- **P3** (ADR-185 Decision 2, correctness-bug fix round: `ioctl_set_prog_voltage`'s pin-9 Discovery
  gate narrowed to fire only for the `SHORT_TO_GROUND` sentinel, not every `prog_voltage_mv` value on
  pin 9): the other two pin-9 sub-cases -- a real millivolt programming-voltage value, and the
  `VOLTAGE_OFF` sentinel -- get no Discovery precheck at all. `DEVICE_INFO_PGM_VOLTAGE_J1962` (the
  separate clause-15 programming-voltage-on-pin-9 capability, distinct from `SHORT_TO_GND_J1962`) is
  never wired to any call site; `VOLTAGE_OFF` has no capability-flag concept to check against at all.
  Deliberate, per Decision 2's "pin-9 short-to-ground case only" scoping -- the native call remains
  the sole authority for both sub-cases. No assigned phase; would need `PGM_VOLTAGE_J1962` added to
  Decision 3's table (its own `DeviceFlag`/per-pin row) if picked up.
- **P3** (ADR-185 Stage 2 mock-fidelity fix, `read_j1962_pin_voltage_pin_4_is_rejected_by_the_
  discovery_fail_fast_path` test): the other two Stage-2 IOCTL-shape call sites --
  `ioctl_get_device_config`/`ioctl_set_device_config`'s `DeviceCapacity` check
  (`DEVICE_INFO_MAX_NON_VOLATILE_STORAGE`) and `ioctl_set_prog_voltage`'s narrowed short-to-ground
  check (`DEVICE_INFO_SHORT_TO_GND_J1962`) -- have no end-to-end integration test exercising their
  Discovery-driven rejection path the way the new J1962 Pin Voltage Read test does; both mechanisms
  are covered by Stage-1-era white-box `discovery.rs` unit tests
  (`enforce_discovery_capability_rejects_swcan_when_device_reports_unsupported`-style coverage of the
  underlying `DeviceFlag`/`DeviceCapacity` machinery, plus their own IOCTL's argument-shape tests) but
  not by a mock-backdoor-driven RPC-level test forcing the specific Discovery answer these two call
  sites consult. Adding one would need a new mock backdoor class (an override for a `GET_DEVICE_INFO`
  answer, not this mock's existing native-call-error-injection backdoors like
  `__mock_set_prog_voltage_error`) -- judged disproportionate to add here; revisit if this class of
  backdoor is needed for another Discovery-cache call site too.
- **P3** (ADR-185 Stage 2, diagnostics/perf nit, not a correctness issue): `ioctl_start_repeat_message`'s
  `ProtocolCapacity` Discovery precheck queries `GET_PROTOCOL_INFO` for `PROTOCOL_INFO_MAX_REPEAT_MESSAGING`
  on every call, but Discovery-query failures are not cached. The mock's `IOCTL_GET_PROTOCOL_INFO`
  allowlist does not include `PROTOCOL_J1939_PS`, so it rejects with `ERR_INVALID_PROTOCOL_ID` -- meaning
  every successful J1939 `START_REPEAT_MESSAGE` call pays a redundant failing native round-trip and logs a
  `warn!` on an otherwise fully normal path (a real device that doesn't enumerate this protocol for
  `GET_PROTOCOL_INFO` would see the same). Not fixed here -- a `debug!`-level log and/or negative caching
  of Discovery-query failures would close it.
- **P3** (`edge-case-hunter` finding, PR #72 round-20 verification pass, pre-existing, not introduced by
  ADR-180 Decision 2's round-20 corrections -- now fixed and out of this backlog -- to the J1939
  claim-timeout/failed-cancel retention paths): `handle_channel_hard_error` never sweeps a hard-errored channel's
  `SharedChannel::j1939_claims`/`j1939_claimed_address`/`j1939_claim_cursor`/`j1939_negotiation_posture`
  state (no `cancel_j1939_claims_for_cll` call), unlike `DisconnectComLogicalLink`/
  `DestroyComLogicalLink`. This is a resource leak, not a correctness hazard: `rpc_connect_com_logical_
  link` only permits a fresh `ConnectComLogicalLink` once `link.connected == false`, and a hard error
  already spawns a brand-new physical channel/`channel_id` on any later reconnect, so no live poll task
  ever matches the old, now-dead `SharedChannel` entry again -- it (and any `j1939_claims` it still
  carries, including a round-20-retained failed-cancel entry) simply leaks until process restart rather
  than being double-claimed or otherwise causing observable protocol misbehavior. Not fixed here --
  disproportionate to this round's scope; revisit if this class of leak needs bounding (e.g. alongside
  a future `SharedChannel` GC pass).
- **P3** (design-advisor-approved fix follow-up, this PR, accepted residual, not a live
  correctness bug): `rpc_io_ctl_legacy`'s `CLEAR_MSG_FILTERS` arm (`rpc_misc.rs`) now closes the
  channel-`channel_id` TOCTOU race that used to be tracked here (the fix holds `shared_channels`
  continuously from `channel_id` resolution through each of the 4 legacy arms' own native calls,
  mirroring `ioctl_sw_can_mode`/`ioctl_get_ndis_adapter_info`'s established pattern), but its own
  filter-reinstall tail runs AFTER that fix's `drop(chans)`, and can itself race a post-clear
  disconnect+reconnect that recycles the same numeric `channel_id`: the reinstall could in
  principle fire onto a channel that is no longer the one this arm actually cleared. This is
  self-neutralizing for the ISO15765 branch (`edge-case-hunter`, this PR's own close-out pass,
  finding #3): `reinstall_iso15765_channel_filters_after_clear`'s own
  `l.channel_id == Some(channel_id)` filtering only reinstalls filters for CLLs actually observed
  on that (possibly-reused) `channel_id` at reinstall time. The non-ISO15765 pass-all branch is
  NOT equally well-supported -- it re-issues a pass-all `PassThruIoctl` filter directly against
  the (possibly-recycled) numeric `channel_id` with no equivalent CLL-identity re-check, so
  whether the native driver call fails safely against a reused handle or could silently apply the
  reinstalled filter to the wrong (new) channel is unverified, not merely low-risk. Narrower
  than, and distinct from, the TOCTOU this PR closes -- documented here as an accepted residual
  rather than fixed, with the pass-all branch's exposure left open rather than asserted safe.
- **P3** (Codex review finding, PR #102 round 7, accepted residual, broader than Ethernet_NDIS,
  not fixed here): `rows_conflict`'s conflict model only ever compares each row's own static
  `dlc_pins` default (except for the two Ethernet_NDIS-specific exceptions in
  `resources::peer_closed_set_extra_pins`, checked against Ethernet_NDIS's Option 2 alternate
  pins) -- it has no general concept of a protocol's own dynamically selectable alternate pins at
  all. UART Echo Byte and J1708 (`names::resolve_pin_selection`'s own arms for both) accept ANY
  caller-supplied pin with no closed-set validation whatsoever, so there is no bounded
  alternate-pin set to enumerate for them even in principle -- meaning this gap is not fully
  closable without a design decision on what "conflict" even means against a protocol whose valid
  pins aren't enumerable. This predates Ethernet_NDIS entirely (Honda DIAG-H/GM UART/FT-CAN's own
  closed alternate sets have existed since their respective phases) and applies to every pair of
  dynamic-pin protocols symmetrically, not only involving Ethernet_NDIS -- e.g. Honda DIAG-H's
  alternate pin 1 against GM UART's own alternate pin 1 is not covered by
  `peer_closed_set_extra_pins` either, since that function only checks a peer's extras against
  Ethernet_NDIS's own pins, not against every other dynamic-pin protocol's extras. A proper fix
  needs `rows_conflict` itself (not just Ethernet_NDIS's own special-cased helper) to understand
  each dynamic-pin protocol's alternate-selection space pairwise, and a decided answer for the
  unbounded UART Echo Byte/J1708 case -- a design question, not a mechanical extension of the
  pattern this PR already used twice. (A round-8 finding raised the mirrored FT-CAN-vs-Option-1
  case specifically; investigation (`edge-case-hunter`, PR #102 close-out) found it is not a live
  gap -- FT-CAN's own `dlc_pins` default to pins 1/9, identical to Ethernet_NDIS's Option 2
  alternate set, so the ordinary `dlc_pins`-overlap check in `rows_conflict` already reports this
  pair as conflicting; no FT-CAN entry was needed in `peer_closed_set_extra_pins`. See
  `resources.rs`'s `peer_closed_set_extra_pins` doc comment for the full investigation.)
  Done when: the rule is recorded in an ADR and `rows_conflict` applies it to UART Echo Byte and
  J1708, with tests. Blocked on: the maintainer's decision on what a pin conflict means for protocols
  whose alternate pins cannot be enumerated (UART Echo Byte, J1708).
- **P3** (found while fixing [ADR-202](../docs/adr/ADR-202-j1850-unique-id-comparam-reclassification.md)'s
  J1850 `PDU_PC_UNIQUE_ID` reclassification, deliberately deferred, not fixed there): ISO 22900-2:2022
  Table B.11 scopes `CP_MidRespId` exclusively to SAE J1708 (its own column mark, distinct from every
  other protocol column), but `unique_id_params` (`comparam_support.rs`) has no J1708 branch at all --
  J1708 (ADR-175, Phase 11) falls into the final "SCI and unknown protocols" `else` arm, so
  `CP_MidRespId` has no reachable `PDU_PC_UNIQUE_ID` list for its own legitimately-scoped protocol.
  Unlike J1850 (where ADR-202 completed an already-built read-side mechanism -- `tx_header.rs`'s
  `ecu_addr`/`response_header_bytes` already consumed `CP_EcuRespSourceAddress` from the table), J1708
  has no read-side consumer for MID values anywhere: `tx_header.rs` has zero MID references, and
  `is_j1708_param` (`comparam_support.rs`) is ADR-175's own deliberate closed allow-list of exactly
  `DATA_RATE`/`LOOPBACK`/`PARAM_MESSAGE_PRIORITY`, with no MID param reachable via plain `SetComParam`
  either. Adding a J1708 UNIQUE_ID branch now would create write-side acceptance with no read-side
  effect at all -- a misleading no-op capability, worse than the current honest rejection. Closing
  this needs a real J1708 MID-addressing feature: TX composition from `CP_MidReqId`, a
  `J1708_UNIQUE_ID_UNUM32 = &[PARAM_MID_RESP_ID]` list wired into `unique_id_params`, and an
  RX-disambiguation decision -- not just a classification-list edit. Not implemented here; out of
  scope for ADR-202, which fixes J1850 only.
- **P3** ([ADR-203](../docs/adr/ADR-203-kwp-j1850-source-address-rx-routing.md) consult, genuinely
  new finding, not something the entry ADR-203 closed already named): ISO 22900-2:2022
  §8.4.28.7.2/.7.3's unmatched-ECU-response model defines a `PDU_ID_UNDEF` catch-all
  `UniqueRespIdTable` entry (a client-configured, params-empty entry with `unique_resp_identifier =
  PDU_ID_UNDEF`) that should deliver a response matching no known ECU entry under that sentinel
  identifier instead of dropping it. `build_cll_rx_entries`'s filter (`events_rx_routing.rs`) strips
  ANY params-empty entry out of `unique_resp_ids` entirely for EVERY protocol, including CAN -- there
  is no reachable `PDU_ID_UNDEF` catch-all delivery path anywhere in this codebase today. This
  predates and is orthogonal to ADR-203's own fix (which only wires up the KWP/J1850
  `CP_EcuRespSourceAddress` matching tier itself); it is a separate, protocol-wide gap, not
  introduced or closed by that ADR. Not investigated or scoped here.
- **P3** (`edge-case-hunter` finding against ADR-206's diff, verified by repro, pre-existing for GM UART
  since ADR-189/Phase 8 -- not introduced or fixed by ADR-206, which only inherits it by mirroring GM
  UART's own arm shape): a directly-named `_CHx` id (`PROTOCOL_GM_UART_CH1..128` or
  `PROTOCOL_J1939_CH1..128`) combined with non-empty, non-default `resource_data.dlc_pin_data` no longer
  reaches that protocol's own `resolve_pin_selection` arm at all (the arm's outer gate is an exact `_PS`
  match, so a raw `_CHx` id falls through to the function's generic clause-6 fallback tail instead) and
  gets the misleading "this protocol has no SAE J2534-2 clause 6 Pin Selection (_PS) variant in this
  service's supported scope" rejection (`names.rs`, the `resources::ps_protocol_id(hw_protocol_id)`
  fallback, since neither GM UART nor J1939 is one of `ps_protocol_id`'s seven `_PS`-covered families)
  instead of `resolve_channel_selection`'s own accurate "clause 6/7 mutually exclusive" message -- the
  request is still correctly rejected with `InvalidArgument` either way, so this is message-accuracy
  only, not a functional defect. Root cause: `resolve_pin_selection`'s `Err` propagates via `?` before
  `resolve_channel_selection` is ever reached for this input shape, the same "wrong stated reason" bug
  class PR #72 round 13 originally fixed for J1939's compound-name route (its own `requested_index.
  is_some()` bypass, mirrored by ADR-206, correctly handles the compound-name route -- only the raw
  numeric-id-plus-pins route is affected). Not fixed here -- would need either composing the
  `ps_protocol_id`-fallback check through `resources::is_chx_protocol_id` first (so a `_CHx` id detours
  to `resolve_channel_selection`'s message instead of the generic one), or moving the exact `_CHx`
  detection earlier, for BOTH GM UART's and J1939's arms at once rather than a one-family patch.
- **P3** (Codex review, ADR-206's PR #117, investigated directly against the ADR-196/198/200 text before
  recording): `CllCreateFlagRawMode` is not extended to any `_CHx` link, J1939's own new one included --
  `rpc_link.rs`'s RawMode protocol allowlist checks `hw_protocol_id` against each family's bare
  `_PS`/base id exactly (e.g. `hw_protocol_id == PROTOCOL_J1939_PS`), so a directly-connected
  `PROTOCOL_J1939_CH1..128` link (now legal to connect at all, per ADR-206) cannot also request RawMode --
  it fails with `PDU_ERR_ID_NOT_SUPPORTED`. None of ADR-196/ADR-198/ADR-200 discusses `_CHx`/Additional
  Channels interaction with RawMode at all -- the exact-id-only allowlist shape is not a considered,
  documented exclusion of `_CHx` specifically, just an artifact of how each entry happens to be written --
  so this same gap already exists, identically, for every one of the 8 protocol families `_CHx` already
  supported before ADR-206 (CAN, ISO15765, ISO9141, ISO14230, J1850VPW, J1850PWM, SCI, GM UART): RawMode
  has never been exercised against ANY `_CHx` link, for any family, and no test in this codebase does so
  either. Not fixed here -- deciding whether RawMode's existing TX/RX shim machinery actually behaves
  correctly end-to-end for a `_CHx` link once allowed (for every family, or only some) is its own
  cross-cutting design question spanning all 10 families as of ADR-207 (UART Echo Byte's own
  `PROTOCOL_UART_ECHO_BYTE_PS` allowlist entry has the identical exact-id-only shape, so it carries the
  same gap, unrecorded as a separate entry per ADR-207's own Consequences), out of scope for a
  single-family ADR to settle unilaterally. Not investigated or scoped here beyond confirming the gap
  and its true (repo-wide) extent.
  Done when: the decision is recorded in an ADR and the RawMode allowlist follows it for every
  family, with tests. Blocked on: the maintainer's decision on whether RawMode is valid on `_CHx` links,
  for all protocol families or only some (ADR-196, ADR-198 and ADR-200 do not address it).
- **P3** (Codex review, ADR-206's PR #117, verified pre-existing since Phase 5/ADR-179): the mock's
  `IOCTL_GET_DEVICE_INFO` has no arm for `DEVICE_INFO_J1939_SUPPORTED` at all -- it falls through to
  `Supported = 0` by construction, unlike GM UART's own dedicated arm or the flat `chx_capacity`-packed
  group the other six CAN-family `_SUPPORTED` params share -- so a discovery-first client can never learn
  J1939 is available, `_PS` or (as of ADR-206) `_CHx` alike. `discovery.rs`'s own
  `device_info_reports_not_supported_for_a_j2534_2_only_capability` test pins this as deliberate,
  pre-existing behavior, and a second `discovery.rs` test confirms real J1939 connects never actually
  consult `enforce_discovery_capability`/`DEVICE_INFO_J1939_SUPPORTED` at connect time at all -- which is
  why J1939 already connects successfully today despite Discovery reporting it unsupported. Not introduced
  or worsened by ADR-206 (identical gap existed for the `_PS` id alone before it). Not fixed here -- wiring
  `DEVICE_INFO_J1939_SUPPORTED`/`_SIMULTANEOUS` into the mock and `j2534-0404-service`'s own Discovery
  cache is a real, separate Phase-5-level gap that would also mean revisiting the two pinning tests above,
  out of scope for a single-family `_CHx`-extension ADR.
- **P3** (ADR-204, found during `cll_tag` removal): `tests/grpc_mock/harness.rs`'s `create_cll`/
  `create_cll_raw_checksum_mode`/`create_j1939_cll` and their own callers (`tests/grpc_mock/j1939.rs`
  and others) still take and thread through a `cll_tag: u64` parameter that does nothing now that
  `CreateComLogicalLinkRequest.cll_tag` no longer exists -- it was already unused before ADR-204
  (never read by this crate's own `rpc_create_com_logical_link`) and is now unused all the way up
  the test-helper call chain too (~150 call sites). Harmless (no lint fires, since the leaf
  parameters are already `_`-prefixed) but is dead test-harness surface; a future cleanup pass can
  delete the parameter and update every call site mechanically.
- **P3** (PR #118, `examples/grpc_j1939.rs`'s `CoptSendrecv` IS-MULTIPLE rewrite): the
  server-side `events.rs` IS-MULTIPLE (`num_receive_cycles: -2`) behavior this example relies on
  is already pinned by `cop_ctrl_cycles.rs`'s `sendrecv_is_multiple_collects_all_matches_within_the_window`
  -- that is NOT the gap. What has no `tests/grpc_mock` coverage is the EXAMPLE's own new
  client-side logic layered on top: stopping the wait on the first PGN-matched frame rather than
  the primitive's own terminal status, and the explicit `CancelComPrimitive` + drain this triggers
  when the wait ends without one. Examples aren't covered by that test suite at all, and this gap
  was deliberately left out of scope for PR #118 (an examples-only change); adding equivalent
  `grpc_mock`-level coverage of the stop-on-match/cancel/drain pattern (as opposed to the
  already-pinned server-side IS-MULTIPLE mechanics) is a separate, standalone task.
- **P3** (ADR-216 Decision item 10 amendment, Codex review, PR #130, Finding 2): evaluate
  extending the change-only-forward pattern `comparam_support::strip_unchanged_analog_channel_wide_keys`
  establishes -- strip a `BUSTYPE_UNUM32` key from a CLL's own hardware push whenever that CLL's
  own staged effective value equals its own current effective Active value, so only a genuine
  change is ever forwarded -- to the rest of `BUSTYPE_UNUM32` generally, not just
  `CP_AnalogActiveChannels`/`CP_AnalogAveragingMethod`. This PR's fix is scoped to those two
  specifically (the concrete cross-sibling stale-value clobber Codex found), not a general
  `BUSTYPE_UNUM32` sweep; every other BUSTYPE-class member (`CP_Baudrate`, `CP_BitSamplePoint`,
  `CP_ListenOnly`, etc.) can in principle suffer the identical clobber whenever two CLLs share a
  channel and one changes a value live after another has already staged its own now-stale copy.
  Widening the fix would be an ADR-110 amendment (that ADR governs `BUSTYPE_UNUM32`'s own
  classification and forwarding rules) -- not done here since it needs its own evaluation of
  whether change-only forwarding is correct for every member, not just these two.

## `vci-service-interface`

### Prioritized Backlog

- **P1**: Add explicit evolution notes for reserved/deprecated fields.
- **P2**: Add a minimal client usage sample aligned with latest schema.

## Cross-crate items

### `iso22900-service`: SubscribeEvent shutdown (`crates/iso22900-service/docs/subscribe-event-shutdown.md`)

- **P2**: Add an integration test that asserts the client sees a `cancelled` status on `SubscribeEvent` when shutdown is triggered with an active stream.
- **P2**: Add an integration test that checks for callback leaks after repeated subscribe/terminate cycles.
- **P2**: Document the expected status codes (`cancelled` vs `aborted`) in `docs/rpc-api-guide.md`.
- **P3**: If the note's assumptions stop holding, reintroduce explicit unregister calls in `terminate_subscription*` and re-test the race behavior.
- **P3**: Add metrics/logging for subscription termination count and finalizer unregister outcomes.

### `iso22900-service`: keyed instance stop (`crates/iso22900-service/docs/keyed-stop-design.md`)

Only if the archived keyed-stop design becomes runtime behavior again:

- **P3**: Reintroduce dedicated lifecycle modules from `src/jsonrpc.rs`, with integration coverage of keyed stop through the stdio server path.
- **P3**: Socket activation (systemd): keyed descriptor passing.
- **P3**: Log key-to-PID mappings.
- **P3**: Restrict stop-signal senders by OS user/group on Unix.
- **P3**: Windows: named event inheritance to isolate child instances.

### Windows tests on the gnullvm ABI (ADR-227)

- **P2**: Run the Windows CI tests for `x86_64-pc-windows-gnullvm` instead of the runner's default MSVC host target, so they exercise the ABI the Windows workers ship with (ADR-227). Needs llvm-mingw on the Windows runner and a check that the `grpc_mock` harness finds the mock libraries under `target/<triple>/`.

### `j2534-0404-service`: conformance audit Part B (`crates/j2534-0404-service/docs/iso22900-2-conformance-audit.md`)

- **P2**: Triage the 25 ADR-documented deviations in Part B (mostly expected to be accepted as-is; see the Part B triage column).

### `sim-vci` and `j2534-0404-mock`: two J2534 cdylib mocks

- **P3**: `sim-vci` and `j2534-0404-mock` are both J2534 cdylib mocks; decide whether `sim-vci` builds on the full mock. Obstacles found in an analysis on 2026-10-06: (1) `unsigned long` width: the mock exports the `j2534-0404-sys` binding types, always 32-bit (`docs/worker-crates.md`, "`unsigned long` width"), while `sim-vci` uses the native `c_ulong` on 64-bit Linux (`crates/sim-vci/src/lib.rs`, `PassThruUlong`) because it is the counterpart for `NGR_J2534_LONG_SIZE=8`; building on the mock would lose that unless the mock (about 11,000 lines) is made width-generic. (2) CI cost: `sim-vci` is cross-checked for the six worker targets and release-built on main (`.github/workflows/ci.yml`), the mock only for host tests; merging cross-builds the whole mock and raises Actions minutes. (3) Two response models: the mock answers internally (loopback echo, fixed per-protocol replies, `__mock_*` injection), `sim-vci` hands ISO-TP requests to `sim-ecu`; a merge needs a rule for which one answers a channel, and existing mock tests depend on today's behaviour. (4) Test control and global state: the mock has about 75 `__mock_*` back-door exports and a per-test `__mock_reset` (`crates/j2534-0404-mock/docs/testing-guide.md`); `sim-vci` configures one process-wide ECU from `NGR_SIM_ECU_CONFIG`. (5) Dependency direction: the mock is a dev-dependency of `j2534-0404-service`, so pulling `sim-ecu` into it makes the worker test double depend on the vehicle simulator. Likely outcome (inferred, not decided): keep both, and share only small pieces such as the calling-convention export macro (the mock's `exported_fn!`, `sim-vci`'s unused `passthru_abi!`) and constants via `j2534-defs`. Done when: the decision (merge, share parts, or keep separate) is recorded in `crates/sim-vci/docs/simulated-vci.md` and `docs/worker-crates.md`, and any agreed sharing is implemented. Blocked on: the maintainer's decision on merging, sharing parts, or keeping the two separate.

### `serial_test` 3.x -> 4.x (RUSTSEC-2026-0205)

- **P2**: `cargo audit` flags `scc` 2.4.0 (unsound `Array::insert`), pulled in by the `serial_test` dev-dependency of `iso22900-service` and `j2534-0404-service`. The fix needs `serial_test` 4.x: bump it, check that `#[serial]` is unchanged, and confirm the advisory is gone. Not a runtime dependency of any shipped binary.

