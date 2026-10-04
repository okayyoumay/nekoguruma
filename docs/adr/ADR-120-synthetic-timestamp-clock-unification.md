# ADR-120: `j2534-0404-service` Synthetic Timestamps Move to a Single Boot-Relative Microsecond Clock

**Date:** 2026-07-23
**Status:** Accepted (amended by this file's own Amendment below)
**Affects:** `j2534-0404-service` events, rpc_primitive, rpc_module, rpc_misc, service; `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`

## Context

Conformance-audit finding A2-26 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`)
found that `j2534-0404-service` had three independent, host-wall-clock-derived
timestamp sources, all disagreeing with the spec and with each other:

- `events::event_timestamp_ms()` — synthetic status/error event timestamps.
- `rpc_primitive::current_timestamp_millis()` — a second, independently
  written copy of the same logic.
- `rpc_module::rpc_get_timestamp` (the `GetTimestamp` RPC) — a third,
  independently written copy.

All three called `SystemTime::now().duration_since(UNIX_EPOCH)` and truncated
to `u32` **milliseconds**. Separately, RX-frame timestamps
(`events.rs`, `let timestamp = msg.timestamp();`) carry the native J2534
`PASSTHRU_MSG.Timestamp` value through unmodified — a real device-clock value
in **microseconds**, with a vendor-defined, non-Unix epoch (SAE J2534-1
§8.2 defines the unit but not the origin). Both kinds of timestamp are
delivered to clients through the same proto fields (`EventItem.timestamp`,
`StatusResponse.timestamp`, `ErrorEventData.timestamp`, `TimestampResponse.timestamp`),
so a client could receive Unix-epoch milliseconds and vendor-epoch
microseconds interchangeably depending on which code path produced the
value — the "mixed clock sources" defect the audit called out.

The repository's own docs disagreed about which of these was even intended:
`docs/rpc-api-guide.md:54` documented `GetTimestamp` as "the VCI hardware
clock (microseconds)"; `docs/j2534-0404-architecture.md:534` documented it as
"current Unix time in milliseconds."

ISO 22900-2:2009(E) is unambiguous on what the wire contract must be:

- **§9.1.6.1** (Timestamp requirements — General information): every
  timestamp is expressed in microseconds and carried in a 32-bit value. The
  time base restarts from zero both when `PDU_IOCTL_RESET` runs and after
  boot-up. Logical links, events and errors belonging to one device all
  take their timestamps from a single common time base. The D-PDU API has
  no means of detecting a timestamp overflow, so handling a wrap is left to
  the application.
- **§9.4.6.4** (`PDUGetStatus`): `pTimestamp` is a microsecond value.
- **§9.4.31.1** (`PDUGetTimestamp`): the module hardware clock that this
  call reads also feeds the timestamps that `PDUGetStatus` returns, with
  the same unit and resolution — i.e. `GetTimestamp` and `GetStatus` are
  spec-required to be the *same* clock, not independently synthesized.

So the fix is not "s/ms/us/": the spec mandates one shared microsecond time
base per device, rooted at boot/reset, with overflow handling left to the
client by design (a 32-bit microsecond counter wraps in ~71.6 minutes; the
spec explicitly accepts this rather than treating it as a defect).

`j2534-0404-service` is a J2534-v04.04-to-D-PDU adapter. J2534 v04.04
exposes no module-clock IOCTL (confirmed by inspecting the full
`PDU_IOCTL_*`/legacy-raw command set in `j2534-0404-sys/src/bindings/j2534_v0404.h`) —
there is no native value this service could read to serve as "the module's
hardware clock" for `GetStatus`/`GetTimestamp`/error-event timestamps. Those
three have always had to be synthesized. RX-frame timestamps, by contrast,
*are* a genuine hardware value (SAE J2534-1 §8.2's per-protocol start/end-of-bit
semantics) and the adapter has no calibration primitive to rebase them onto
any other clock without destroying that fidelity.

By contrast, `iso22900-service` (the native D-PDU API service) is
spec-correct today by construction: `rpc_get_timestamp` passes the vendor
DLL's `PDUGetTimestamp` result straight through, and RX-frame timestamps
pass the vendor DLL's frame timestamp straight through — both genuinely
share the vendor's own hardware clock, satisfying §9.4.31.1 directly. This
ADR does not change `iso22900-service`.

## Decision

1. **Introduce one shared synthetic clock**, `events::module_timestamp_us() -> u32`:
   microseconds elapsed since this service process started, backed by a
   monotonic `std::time::Instant`, not `SystemTime`. Monotonic time is used
   because the spec's time base is a hardware counter that must never step
   backward (as a wall clock can, on an NTP correction); it also removes the
   fallible `duration_since(UNIX_EPOCH)` error path that all three prior
   implementations carried for no real benefit (the error case cannot
   occur with `Instant`). The stored `Instant` is also rebased to
   (approximately) zero on `PDU_IOCTL_RESET` via a second function,
   `events::reset_module_clock()` -- see the Amendment below; this was a
   real gap in the original decision, not a deliberate omission.
2. **Replace all three independent implementations** with calls to this one
   function:
   - `events::event_timestamp_ms()` is removed; its call sites
     (`events.rs:250,277,298,381,411,504,6065`) call `module_timestamp_us()`.
   - `rpc_primitive::current_timestamp_millis()` is removed; its call site
     (`rpc_primitive.rs:1748`) calls `events::module_timestamp_us()`.
   - `rpc_module::rpc_get_timestamp` (`rpc_module.rs:239-257`) returns
     `events::module_timestamp_us()` directly, dropping its own
     `SystemTime` logic.
3. **RX-frame timestamps are left untouched** — still the raw native
   `PASSTHRU_MSG.Timestamp` value, unmodified, in `events.rs`
   (`let timestamp = msg.timestamp();`). This is a genuine hardware value on
   a vendor-defined time base; there is no calibration data available to
   rebase it onto `module_timestamp_us()`'s epoch, and doing so anyway would
   silently mutate real device data and discard its accuracy.
4. **No cross-service alignment with `iso22900-service`.** Both services now
   expose the same *semantics* (an opaque, boot/reset-relative 32-bit
   microsecond counter, spec-conformant unit and reset behavior) but not the
   same *values* — `iso22900-service`'s epoch is whatever the vendor DLL's
   hardware clock uses; `j2534-0404-service`'s epoch is this process's
   start time. There is no shared reference clock available to unify them
   further under J2534 v04.04.
5. **Fix both contradicting docs** (`docs/rpc-api-guide.md:54`,
   `docs/j2534-0404-architecture.md:534`) to describe the actual, now-unified
   behavior, and record A2-26 as fixed in
   `j2534-0404-service/docs/iso22900-2-conformance-audit.md`.

### Alternatives considered

- **Unix-epoch microseconds, truncated to `u32`.** Satisfies the unit but
  not §9.1.6.1's boot/reset-relative epoch, and wraps at the same ~71.6-minute
  interval as the chosen design with a meaningless arbitrary phase (Unix
  epoch mod 2^32 µs) instead of a clean zero-at-start origin. Rejected: no
  advantage over the chosen design, and it still doesn't match the spec's
  own definition of the time base.
- **Keep milliseconds, fix only the docs.** Directly contradicts
  §9.1.6.1/§9.4.6.4/§9.4.31.1. Would leave the audit finding open.
- **Rebase RX-frame timestamps onto `module_timestamp_us()`'s epoch** (e.g.
  by estimating a per-connection offset between the native counter and the
  host clock). Rejected: J2534 v04.04 provides no calibration primitive for
  this; any offset estimate is unbounded in error due to vendor-DLL
  buffering, and the SAE J2534-1 start/end-of-bit timing meaning of the
  native field would be silently degraded to fit a synthetic scale.
- **Zero the synthetic clock at `ModuleConnect` instead of process start.**
  Would make timestamps jump backward on a same-process reconnect
  (disconnect, then connect again) for no spec benefit — §9.1.6.1 only
  requires a reset at boot-up or `PDU_IOCTL_RESET`, and this adapter has
  neither of those as a distinct, addressable event from process lifetime.
- **A per-source local fix (three independent unit conversions, no shared
  function).** Rejected as structurally reintroducing exactly the defect
  A2-26 found: three independently maintained clock implementations that
  can silently re-diverge. §9.1.6.1 requires one shared time base per
  device; the code should have exactly one implementation of it.

## Consequences

- **Breaking wire change.** Any client currently interpreting these fields
  as Unix-epoch milliseconds will read wrong values after this change. This
  is accepted: the prior `u32`-truncated-Unix-ms encoding already wrapped
  every ~49 days and was not spec-conformant, so no client could have relied
  on it as a stable wall-clock value in the first place.
- **Two time bases remain, by design, and are not comparable.** RX-frame
  timestamps (native device µs, vendor-defined epoch) and synthetic
  timestamps (`module_timestamp_us()`, process-start epoch) can both appear
  in the same CLL event queue. §9.1.6.1's "same time base" requirement is
  unsatisfiable for `j2534-0404-service` specifically, because J2534 v04.04
  gives the adapter no module-clock read to calibrate against. Clients must
  not compare a synthetic timestamp against a native RX-frame timestamp for
  ordering or elapsed-time purposes — only within-source comparisons are
  meaningful.
- **~71.6-minute wraparound**, consistent with §9.1.6.1's explicit
  statement that overflow handling is the application's job — this is
  spec-mandated behavior, not a regression from the prior ~49-day ms wrap.
- **Synthetic timestamps reset to zero on every service restart** and have
  no relationship to wall-clock time; this matches §9.1.6.1's boot-relative
  definition.
- **`iso22900-service` and `j2534-0404-service` share timestamp semantics
  but never share timestamp values** — each has its own, unrelated 32-bit
  microsecond epoch. A client bridging both services must not attempt to
  correlate their timestamps.
- **2009→2022 revision risk**: this finding and ADR cite ISO 22900-2:2009(E)
  §9.1.6 only (no 2022 copy is available in this workspace per CLAUDE.md's
  spec-references note). The UNUM32-microsecond wire shape is defined at the
  ABI level and is very unlikely to have changed, but the boot/reset-relative
  epoch requirement should be re-verified against the 2022 text if it ever
  becomes available.

## Amendment — 2026-07-23: `PDU_IOCTL_RESET` Also Rebases the Synthetic Clock (Codex review, PR #129)

**Context.** §9.1.6.1 requires the time base to restart from zero
both when `PDU_IOCTL_RESET` runs **and** after boot-up -- two distinct reset
triggers. The Decision as originally implemented only covered the
boot-up half: `module_timestamp_us()` captured its `Instant` once, in a
`OnceLock<Instant>`, permanently, at first call. `rpc_misc.rs::ioctl_reset`
(the `PDU_IOCTL_RESET` handler) reset all other module/link soft state but
had no way to rebase this immutable, process-global clock, so
`GetTimestamp`/`GetStatus`/synthetic event timestamps kept counting from
the original process-start epoch across a client's `PDU_IOCTL_RESET` call,
contradicting §9.1.6.1 and this ADR's own Decision point 1 as written. A
Codex review on PR #129 caught this gap.

**Decision.** `events.rs` replaces the bare `OnceLock<Instant>` with
`static CLOCK_START: OnceLock<RwLock<Instant>>`. `module_timestamp_us()`
reads the current `Instant` through a brief `read()` guard (cheap on the
read side, since it is called on every synthetic timestamp/event, across
many concurrent async tasks). A new function, `events::reset_module_clock()`,
takes a brief `write()` guard and overwrites the stored `Instant` with a
fresh `Instant::now()`. Neither path ever holds the lock across an
`.await`, so there is no deadlock exposure. `rpc_misc.rs::ioctl_reset` calls
`events::reset_module_clock()` early in its body, before any of its
documented `logical_links`/`api`/`primitives` locking (ADR-107 addendum's
lock-ordering discipline) -- the clock reset is a self-contained global with
no lock-ordering relationship to those, so it is placed where it cannot be
reasoned about as ordered relative to them at all, rather than being fit
into that ordering.

**Consequences.** Unchanged from the original Decision except: synthetic
timestamps now also reset to (approximately) zero on `PDU_IOCTL_RESET`, not
only on process restart. `docs/j2534-0404-architecture.md` and
`docs/rpc-api-guide.md` are both updated in this same amendment to describe
the reset explicitly, instead of only "process-start-relative."

**Accepted residual (`edge-case-hunter` finding on this amendment):** only
`ModuleConnect`/`ModuleDisconnect` (`rpc_module.rs`) and `ioctl_reset` itself
acquire `lock_device_for`/`device_id`; every other event-emitting call site
(background poll-task dispatch, error events, etc.) calls
`module_timestamp_us()` with no coordination against a concurrent
`reset_module_clock()` call. An event that logically pertains to activity
just before a client's `PDU_IOCTL_RESET` can therefore race the reset and
receive a post-reset (small) timestamp inconsistent with that CLL's own
prior event timestamps. Not fixed here: serializing all event emission
behind the device lock to close this window would be a much larger change
than this mechanical fix, for a narrow, `PDU_IOCTL_RESET`-triggered race
whose worst case is a locally out-of-order-looking timestamp on one
event -- not a wrong status, lost event, or misattributed COP/CLL. If this
is ever judged worth closing, route it to `design-advisor` with
`events.rs`'s event-emission call sites and `rpc_module.rs`'s
`lock_device_for` usage as the evidence set.

## Amendment 2 — 2026-07-23: Eager Clock Init, and Why the Clock Stays Process-Global (Codex review, PR #129, round 2)

A second Codex review round on Amendment 1's diff raised two findings.

**Finding A (fixed): the clock was first-use-relative, not
service-start-relative.** `module_timestamp_us()`'s `CLOCK_START.get_or_init`
captured its epoch lazily, on whatever call reached it first -- which could
be long after the service actually started if, for example, a client's
first `GetTimestamp` call arrives well after startup with no prior
synthetic event to have triggered the lazy init sooner. This contradicted
this ADR's own "microseconds elapsed since this service process started"
framing. **Fixed** by adding `events::init_module_clock()` (calls
`CLOCK_START.get_or_init` eagerly, idempotently) and invoking it as the
first statement of `J2534Service::new`, ahead of every other, potentially
fallible, constructor step.

**Finding B (not fixed, accepted as consistent with ADR-107): the clock is
one process-global value, not partitioned per configured `modules` entry.**
When `config.apis.j2534-0404.libs."<lib>".modules` declares more than one
module (ADR-107), `PDU_IOCTL_RESET` for module A's handle also rebases what
a later `GetTimestamp`/`GetStatus` call for module B's handle observes,
even though B was never reset -- module B's own time base, in isolation,
does not stay untouched by an operation addressed at module A.

This is a real, spec-literal read of §9.1.6.1's "one device, one time base"
language. It is **not fixed**, because ADR-107 Decision (b) already made
this exact tradeoff for every other piece of module-scoped state this
clock sits alongside, and extending only the clock to be per-module would
create a new inconsistency rather than close one:

- ADR-107 Decision (b) states plainly: "`J2534Service`'s state model --
  `device_id`, `ModuleState`, `shared_channels`, event subscriptions... is
  single-module by design... Extending every one of those to a per-module
  partition... is explicitly out of scope."
- `module_state: Arc<Mutex<ModuleState>>` (`service.rs`) is the *exact*
  same shape of global, non-partitioned state the clock now uses:
  `GetStatus(ModuleHandle)` returns `self.module_state.lock().await.status`
  for ANY validly-ranged handle, regardless of which configured module it
  names, and `ioctl_reset` resets that same single `module_state` for any
  handle it's allowed to act on -- identical to the clock's behavior this
  finding flags.
- ADR-107's own Accepted Residual #1: "`module_status` reports
  `PDU_MODST_READY` for every configured row, including ones not currently
  open... there is no J2534 enumeration API to probe real per-device...
  status with." Residual #4: "Module-level event/subscription attribution
  still hardcodes `DEFAULT_MODULE_HANDLE`... A module-status event
  published while module 2 is open is still filed under handle 1's
  subscription key." Both are the identical "not partitioned per
  configured module" limitation this finding describes, already accepted
  for `GetStatus`, `GetEventItem(module)`, and `SubscribeEvent`.
- ADR-107's "Design Alternatives Not Chosen" explicitly rejects true
  per-module state partitioning as its own, much larger, separately-scoped
  change: "would require reworking `shared_channels`, `logical_links`,
  subscription keys, and the poll-task model to be module-aware
  throughout, not just at the device-open boundary." A clock-only
  partition would leave `GetTimestamp(module_handle=2)` isolated while
  `GetStatus(module_handle=2)` still transparently reports whatever module
  is actually open (module 1's real status) -- a new, more confusing
  three-way split (real device state vs. isolated fake clock vs. the
  reject-on-switch model's own single-open-device reality) rather than a
  fix.

**Accepted residual:** `j2534-0404-service`'s module-scoped synthetic
state -- `module_state`, module/system event attribution, and now
`module_timestamp_us()` -- is uniformly single-open-device-scoped, not
per-configured-module-scoped, matching ADR-107 Decision (b) and its
Residuals #1/#4. A client that addresses `GetTimestamp`/`GetStatus`/
`PDU_IOCTL_RESET` at a configured-but-not-currently-open module handle
observes the currently-open module's state, not that handle's own
isolated state -- there is no isolated per-handle state anywhere in this
service to observe. True per-module partitioning (clock included) is out
of scope here for the same reason ADR-107 scoped it out: it would require
reworking `shared_channels`/`logical_links`/subscription keys/the
poll-task model to be module-aware throughout, not a mechanical,
clock-only change.
