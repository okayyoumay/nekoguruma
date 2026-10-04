//! SAE J2534-2 clause 25 Discovery Mechanism support (ADR-153, Phase 1 of
//! `docs/j2534-2-support-plan.md`). Internal-only for this phase: no
//! `PDU_IOCTL_*`/RPC surface exposes this data to clients yet (ADR-153
//! Decision 3). ADR-156 Decision 4/Phase 2b gave the *shared cache-hit/
//! native-call machinery* its first real caller
//! ([`J2534Service::check_chx_capacity`], the SAE J2534-2 clause 7
//! Additional Channels capacity precheck) -- but `check_chx_capacity` is
//! always called while its own caller's `device_guard` is already held, so
//! it cannot call [`J2534Service::discovery_device_info`] itself (that
//! would re-lock the same non-reentrant `self.device_id` mutex the caller
//! already holds and deadlock); it instead calls the lower-level
//! `discovery_device_info_with_open_device` directly, against a `DeviceId`
//! it already has. [`J2534Service::discovery_device_info`] (which opens its
//! own device) and [`J2534Service::discovery_protocol_info`] therefore still
//! have no *production* caller of their own — a future phase whose
//! connect-time enforcement point does NOT already hold an open-device
//! guard is expected to call one of them directly at its own enforcement
//! point.
//!
//! ADR-185 Stage 1 gave [`J2534Service::discovery_device_info`] (via
//! [`J2534Service::enforce_discovery_capability`]) its first production
//! callers: `resources::connect_discovery_check`'s `DeviceFlag`
//! connect-path rows (SWCAN, FT-CAN, UART Echo Byte, Honda DIAG-H, J1708,
//! Analog Inputs, TP2.0, GM UART, and -- ADR-194/Phase 16 --
//! Ethernet_NDIS), wired from `rpc_link.rs`'s brand-new-physical-channel
//! connect path alongside `check_chx_capacity`. The Discovery cache key is
//! `(module_handle, parameter, input_value)` -- widened from the former
//! `(module_handle, parameter)` in the same change, so a per-pin
//! `DEVICE_INFO_*` parameter (clause 15/25.3.2.2's
//! `DEVICE_INFO_SHORT_TO_GND_J1962`/`DEVICE_INFO_PGM_VOLTAGE_J1962` -- both
//! `DeviceFlag`-shaped) can be queried through production code without
//! colliding with a different pin's cached answer; every caller added
//! before this widening passes `input_value: 0`, preserving its prior
//! behavior exactly. `DiscoveryCheck::DeviceCapacity` has no `input_value`
//! of its own yet (`enforce_discovery_capability` hardcodes `0` for it) --
//! the widening closes the landmine for `DeviceFlag` callers, the only
//! shape either documented per-pin parameter actually needs; a future
//! per-pin *capacity* check would need its own field added first.
//!
//! ADR-185 Stage 2 wires the four remaining IOCTL-shape call sites
//! (`ioctl_read_j1962_pin_voltage`, `ioctl_get_device_config`/
//! `ioctl_set_device_config`, `ioctl_set_prog_voltage`'s pin-9
//! Short-to-Ground case, `ioctl_start_repeat_message`), all in
//! `rpc_misc.rs`. All four hold a `device_guard` at their own top and use
//! `DeviceAccess::AlreadyOpen` -- they add new `DiscoveryCheck` construction
//! call sites but no new resolution path beyond the one this stage adds for
//! `ProtocolCapacity` (below).
//!
//! `ioctl_start_repeat_message` originally acquired `device_guard` lazily
//! (a peek-and-drop) and resolved its Discovery precheck via
//! `DeviceAccess::OpenIfNeeded`, deferred until after `shared_channels` was
//! already locked (this function's pre-existing "Bug 1 fix" holds
//! `shared_channels` across the native call). That ordering inverted the
//! codebase-wide `device_id`-outermost lock order
//! (`service.rs`'s `require_connected_device_for` doc comment, ADR-107
//! addendum) and risked an AB-BA deadlock against `ConnectComLogicalLink`'s
//! own `device_id`-outer/`shared_channels`-inner order (design-advisor
//! review, same-PR fix). The corrected version acquires and holds
//! `device_guard` BEFORE `shared_channels`, matching the connect path's
//! order, and resolves its Discovery precheck via `DeviceAccess::AlreadyOpen`
//! instead -- giving [`J2534Service::discovery_protocol_info_with_open_device`]
//! (via [`J2534Service::enforce_discovery_capability`]'s `ProtocolCapacity`
//! arm) its first production caller. `DeviceAccess::OpenIfNeeded` (and, by
//! extension, [`J2534Service::discovery_protocol_info`]) is left fully
//! implemented and unit-tested but has no production caller again -- see
//! each item's own doc comment.

use std::ffi::CStr;

use j2534_0404::{DiscoveryParam, DiscoveryResult};
use tracing::warn;

use super::*;

/// SAE J2534-2 clause 5's feature-enablement convention: only a `pname`
/// whose prefix is this literal string opts a module into J2534-2 behavior.
/// Any other value (including `None`) means J2534-1-only behavior must be
/// assumed, so the Discovery IOCTLs are never issued for it (ADR-153).
/// Narrower than `docs/j2534-2-support-plan.md`'s still-open §4.1 secondary
/// question (how a non-prefixed, non-`NULL` `pname` should otherwise be
/// treated) -- this only gates whether Discovery is queried.
const J2534_2_PNAME_PREFIX: &[u8] = b"J2534-2:";

pub(super) fn is_j2534_2_opted_in(pname: Option<&CStr>) -> bool {
    pname.is_some_and(|p| p.to_bytes().starts_with(J2534_2_PNAME_PREFIX))
}

/// ADR-185 Stage 1: what a [`J2534Service::enforce_discovery_capability`]
/// call site needs verified against the cached Discovery answer, mirroring
/// `check_chx_capacity`'s own numeric-unpacking precedent (ADR-156 Decision
/// 4/Phase 2b) but generalized to any `GET_DEVICE_INFO`/`GET_PROTOCOL_INFO`
/// parameter, not just the `_CHx` capacity one.
pub(super) enum DiscoveryCheck {
    /// `GET_DEVICE_INFO` must report `supported == true`. `input_value` is
    /// `0` for every flag-shaped connect-path row this Stage wires up; a
    /// per-pin parameter (clause 15/25.3.2.2) would pass a pin-selector
    /// bitmap instead -- not exercised by any Stage 1 caller.
    DeviceFlag { parameter: u32, input_value: u32 },
    /// `GET_DEVICE_INFO` must report `supported == true` AND
    /// `extract(result.value) >= needed` -- a numeric capacity check, not a
    /// boolean one (mirrors `check_chx_capacity`'s own `(value >> 16) &
    /// 0xFF` unpacking shape). Wired up by ADR-185 Stage 2:
    /// `ioctl_get_device_config`/`ioctl_set_device_config`'s
    /// `DEVICE_INFO_MAX_NON_VOLATILE_STORAGE` precheck (`extract` is the
    /// identity function -- Table 111's plain unsigned-long count, not a
    /// packed value).
    DeviceCapacity {
        parameter: u32,
        extract: fn(u32) -> u32,
        needed: u32,
    },
    /// `GET_PROTOCOL_INFO` must report the equivalent for a protocol-scoped
    /// numeric limit (`result.value >= needed` directly -- unlike
    /// `DeviceCapacity`, no `extract` field, since
    /// `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` (Table 114, clause 25.3.2.3) is
    /// a plain unsigned-long count, not a packed value). Wired up by ADR-185
    /// Stage 2: `ioctl_start_repeat_message`'s live-slot-count precheck.
    ProtocolCapacity {
        protocol_id: u32,
        parameter: u32,
        needed: u32,
    },
}

/// ADR-185 Stage 1: how [`J2534Service::enforce_discovery_capability`]
/// should resolve the Discovery answer -- encodes the non-reentrant
/// `self.device_id` deadlock hazard this module's doc comment already
/// documents (`check_chx_capacity` vs. `discovery_device_info`) in the type
/// system rather than relying on caller discipline alone.
pub(super) enum DeviceAccess {
    /// The caller already holds a `device_guard` for this `DeviceId` --
    /// funnels through `discovery_device_info_with_open_device`/
    /// `discovery_protocol_info_with_open_device`/`check_chx_capacity`'s own
    /// discipline. Calling `ensure_open_device_for` again from within the
    /// same task would deadlock (tokio's `Mutex` is not reentrant).
    AlreadyOpen(DeviceId),
    /// The caller holds no device guard -- funnels through
    /// `discovery_device_info`/`discovery_protocol_info`, which open the
    /// device themselves. No production caller yet: `ioctl_start_repeat_message`
    /// (`rpc_misc.rs`) briefly used this variant, but its lock-order fix
    /// (device_guard now acquired up front, before `shared_channels` --
    /// see this module's own doc comment) switched it to `AlreadyOpen`
    /// instead. Left in place (and unit-tested via `enforce_discovery_capability`)
    /// for a future enforcement point that does not already hold a device
    /// guard.
    #[allow(dead_code)]
    OpenIfNeeded,
}

impl J2534Service {
    /// Returns the cached `GET_DEVICE_INFO` answer for `parameter` under
    /// `module_handle`'s device (ADR-153), querying the native device on a
    /// cache miss (or stale-epoch entry) and caching the answer. Returns
    /// `Ok(None)` without opening a device or issuing any native call when
    /// the module's `pname` is not opted into J2534-2 (clause 5) -- a device
    /// never told to expose J2534-2 behavior is not queried.
    ///
    /// `module_handle` must already be a validated 1-based position into
    /// `self.modules` (same contract as `ensure_open_device_inner`'s
    /// `requested` parameter -- callers run `require_module_handle` first).
    /// Lazily opens the device via `ensure_open_device_for` if not already
    /// open, mirroring every other module-scoped IOCTL helper in this crate.
    ///
    /// No production caller yet -- see this module's doc comment for why
    /// `check_chx_capacity` (ADR-156 Decision 4/Phase 2b) can't be the one:
    /// it's always called while its own caller's `device_guard` is already
    /// held, so it uses `discovery_device_info_with_open_device` directly
    /// instead of this function.
    #[allow(dead_code)]
    pub(super) async fn discovery_device_info(
        &self,
        module_handle: u32,
        parameter: u32,
        input_value: u32,
    ) -> Result<Option<DiscoveryResult>, Status> {
        if !is_j2534_2_opted_in(self.modules[(module_handle - 1) as usize].pname.as_deref()) {
            return Ok(None);
        }
        if let Some(result) = self
            .discovery_device_info_cache_hit(module_handle, parameter, input_value)
            .await
        {
            return Ok(Some(result));
        }
        // ADR-156 Decision 4/Phase 2b correction: opening the device (and
        // holding the guard across the native call below) is factored into
        // this dedicated helper -- `Self::check_chx_capacity` cannot reuse
        // it, since it is always called from `rpc_connect_com_logical_link`
        // while THAT function's own `device_guard` (from an earlier
        // `ensure_open_device` call) is still held; `ensure_open_device_for`
        // re-locking the same `self.device_id` mutex from within the same
        // task would deadlock (tokio's `Mutex` is not reentrant). See
        // `discovery_device_info_with_open_device`, which both this function
        // and `check_chx_capacity` funnel through once a `DeviceId` is in
        // hand, whichever way it was obtained.
        let (_device_guard, device_id) = self.ensure_open_device_for(module_handle).await?;
        // Legal to read `module_state` here: this wrapper only ever holds
        // `device_id` (via `_device_guard` above), never `shared_channels`
        // -- unlike `discovery_device_info_with_open_device`'s
        // `DeviceAccess::AlreadyOpen` callers, which is why that function
        // takes `last_error` in from its caller instead of reading it
        // itself (ADR-185 Stage 2 lock-order fix).
        let last_error = Some(self.module_state.lock().await.last_error.clone());
        self.discovery_device_info_with_open_device(
            module_handle,
            device_id,
            parameter,
            input_value,
            last_error,
        )
        .await
    }

    /// The cache-hit half of [`Self::discovery_device_info`], factored out
    /// so [`Self::check_chx_capacity`] can also consult the cache without
    /// needing a `DeviceId` (and therefore without any device-opening lock)
    /// on the common cache-hit path.
    async fn discovery_device_info_cache_hit(
        &self,
        module_handle: u32,
        parameter: u32,
        input_value: u32,
    ) -> Option<DiscoveryResult> {
        // `module_handle` is part of the key, not just the epoch: without
        // it, a query for module A could serve module B's cached answer
        // whenever B happens to be the one currently open under the same
        // epoch, entirely bypassing `ensure_open_device_for`'s resource-busy
        // check below (which only runs on a miss) -- Codex review on PR #25.
        //
        // `input_value` widens the key alongside `parameter` (ADR-185 Stage
        // 1): closes the landmine this module's doc comment used to flag --
        // every `DEVICE_INFO_*` parameter queried before clause 15/25.3.2.2's
        // per-pin `DEVICE_INFO_SHORT_TO_GND_J1962`/`DEVICE_INFO_PGM_VOLTAGE_J1962`
        // was a flat, no-input capability flag, but those two take a
        // caller-supplied pin-selector bitmap as genuine input; without it in
        // the key, a query for one pin could be served another pin's cached
        // answer.
        let key = (module_handle, parameter, input_value);
        // The epoch is read AFTER the map lock is acquired, not before: a
        // read taken before `.lock().await` could be made stale by a
        // concurrent ModuleDisconnect completing while this task was
        // suspended waiting for the lock, letting an old-epoch entry
        // compare equal to a captured value that itself predates the
        // disconnect (Codex review on PR #25). No `.await` separates the
        // lock acquisition from this read, so nothing can invalidate the
        // entry in between.
        let cache = self.discovery_device_info.lock().await;
        let epoch = self.device_epoch.load(Ordering::SeqCst);
        cache
            .get(&key)
            .filter(|(cached_epoch, _)| *cached_epoch == epoch)
            .map(|(_, result)| *result)
    }

    /// Cache-miss half of [`Self::discovery_device_info`]: issues the native
    /// `GET_DEVICE_INFO` call against an already-open `device_id` and caches
    /// the answer. Does NOT itself open the device or check the J2534-2
    /// opt-in gate -- callers that don't already have both handled (i.e.
    /// everyone except [`Self::discovery_device_info`] and
    /// [`Self::check_chx_capacity`], both of which check opt-in via their
    /// own `is_j2534_2_opted_in` gate before reaching here) must not call
    /// this directly.
    ///
    /// `last_error` is a caller-supplied snapshot, not re-read from
    /// `self.module_state` in here (ADR-185 Stage 2 lock-order fix,
    /// `design-advisor` review): a caller reaching this function via
    /// `DeviceAccess::AlreadyOpen` may already hold `self.shared_channels`
    /// (`ioctl_start_repeat_message`'s Bug-1 region, `rpc_connect_com_logical_link`'s
    /// check-create-insert region), and `self.module_state` must never be
    /// locked while `shared_channels` is held -- the documented
    /// `device_id -> module_state -> shared_channels` order (ADR-107
    /// addendum/ADR-134; see `rpc_link.rs`'s own comment/precedent for this
    /// rule). Locking `module_state` internally here, as this function used
    /// to, would invert that order for exactly those callers. This also
    /// removes a `self.api` `MutexGuard`-held-during-`module_state`-lock
    /// overlap that existed too: the previous `match
    /// self.api.lock().await....` scrutinee's temporary guard lived to the
    /// end of the match arm, spanning the (now-removed) internal
    /// `module_state` lock on the error arm.
    async fn discovery_device_info_with_open_device(
        &self,
        module_handle: u32,
        device_id: DeviceId,
        parameter: u32,
        input_value: u32,
        last_error: Option<TrackedError>,
    ) -> Result<Option<DiscoveryResult>, Status> {
        // A cache-hit may have appeared between the caller's own check and
        // this call (e.g. `check_chx_capacity`'s cache-hit fast path already
        // returned before ever reaching here, but a concurrent lookup could
        // have populated the cache in between for `discovery_device_info`'s
        // own two-step path) -- re-checking here is cheap and avoids an
        // unnecessary native call in that narrow window.
        if let Some(result) = self
            .discovery_device_info_cache_hit(module_handle, parameter, input_value)
            .await
        {
            return Ok(Some(result));
        }
        let key = (module_handle, parameter, input_value);
        let query = [DiscoveryParam {
            parameter,
            value: input_value,
        }];
        let result = match self.api.lock().await.get_device_info(device_id, &query) {
            Ok(results) => results
                .into_iter()
                .next()
                .expect("get_device_info returns exactly one result per input parameter"),
            Err(err) => {
                return Err(map_native_error_for_link(
                    "PassThruIoctl GET_DEVICE_INFO",
                    &err,
                    last_error,
                ));
            }
        };
        // Re-read the epoch now, not the value captured before the device
        // was opened: opening the device may have just performed its first
        // `PassThruOpen`, which bumps `device_epoch` as a side effect
        // (`service.rs`'s `ensure_open_device_inner`) -- caching under the
        // pre-open epoch would self-invalidate this entry on the very next
        // lookup. Mirrors `probe_can_channel_mode`/`autodetect_sae_j1850_flavor`'s
        // re-read-right-before-write-back pattern (edge-case-hunter finding).
        let epoch = self.device_epoch.load(Ordering::SeqCst);
        self.discovery_device_info
            .lock()
            .await
            .insert(key, (epoch, result));
        Ok(Some(result))
    }

    /// The cache-hit half of [`Self::discovery_protocol_info`], factored out
    /// so [`Self::discovery_protocol_info_with_open_device`] can also
    /// consult the cache without needing a `DeviceId` on the common
    /// cache-hit path -- mirrors `discovery_device_info_cache_hit`'s own
    /// precedent exactly.
    async fn discovery_protocol_info_cache_hit(
        &self,
        module_handle: u32,
        protocol_id: u32,
        parameter: u32,
    ) -> Option<DiscoveryResult> {
        // See `discovery_device_info_cache_hit`'s matching comments:
        // `module_handle` must be part of the key (Codex review on PR #25),
        // and the epoch is read after the map lock is acquired, not before,
        // closing the same TOCTOU window.
        let key = (module_handle, protocol_id, parameter);
        let cache = self.discovery_protocol_info.lock().await;
        let epoch = self.device_epoch.load(Ordering::SeqCst);
        cache
            .get(&key)
            .filter(|(cached_epoch, _)| *cached_epoch == epoch)
            .map(|(_, result)| *result)
    }

    /// Same as [`Self::discovery_device_info`], but for `GET_PROTOCOL_INFO`,
    /// keyed by `(module_handle, protocol_id, parameter)` (ADR-153).
    ///
    /// No production caller yet -- see this module's doc comment:
    /// `enforce_discovery_capability`'s `ProtocolCapacity` arm's
    /// `DeviceAccess::OpenIfNeeded` case is the only route to this function,
    /// and `ioctl_start_repeat_message` (its would-be first production
    /// caller) was switched to `DeviceAccess::AlreadyOpen` /
    /// [`Self::discovery_protocol_info_with_open_device`] instead, by the
    /// same lock-order fix.
    #[allow(dead_code)]
    pub(super) async fn discovery_protocol_info(
        &self,
        module_handle: u32,
        protocol_id: u32,
        parameter: u32,
    ) -> Result<Option<DiscoveryResult>, Status> {
        if !is_j2534_2_opted_in(self.modules[(module_handle - 1) as usize].pname.as_deref()) {
            return Ok(None);
        }
        if let Some(result) = self
            .discovery_protocol_info_cache_hit(module_handle, protocol_id, parameter)
            .await
        {
            return Ok(Some(result));
        }
        let (_device_guard, device_id) = self.ensure_open_device_for(module_handle).await?;
        // See `discovery_device_info`'s matching comment: legal to read
        // `module_state` here since this wrapper only ever holds `device_id`,
        // never `shared_channels`.
        let last_error = Some(self.module_state.lock().await.last_error.clone());
        self.discovery_protocol_info_with_open_device(
            module_handle,
            device_id,
            protocol_id,
            parameter,
            last_error,
        )
        .await
    }

    /// Cache-miss half of [`Self::discovery_protocol_info`]: issues the
    /// native `GET_PROTOCOL_INFO` call against an already-open `device_id`
    /// and caches the answer. Does NOT itself open the device or check the
    /// J2534-2 opt-in gate -- mirrors
    /// `discovery_device_info_with_open_device`'s own contract exactly;
    /// callers that don't already have both handled must not call this
    /// directly. ADR-185 Stage 2's lock-order fix
    /// (`ioctl_start_repeat_message`, `rpc_misc.rs`, via
    /// `enforce_discovery_capability`'s `ProtocolCapacity` +
    /// `DeviceAccess::AlreadyOpen` arm) is its first production caller.
    ///
    /// `last_error` is caller-supplied, not re-read from `self.module_state`
    /// in here -- same rationale as
    /// `discovery_device_info_with_open_device`'s matching doc comment: a
    /// `DeviceAccess::AlreadyOpen` caller may already hold
    /// `self.shared_channels` (`ioctl_start_repeat_message`'s Bug-1 region),
    /// and `self.module_state` must never be locked while `shared_channels`
    /// is held (ADR-107 addendum/ADR-134). This also removes the same
    /// `self.api` `MutexGuard`-held-during-`module_state`-lock overlap that
    /// existed here too.
    async fn discovery_protocol_info_with_open_device(
        &self,
        module_handle: u32,
        device_id: DeviceId,
        protocol_id: u32,
        parameter: u32,
        last_error: Option<TrackedError>,
    ) -> Result<Option<DiscoveryResult>, Status> {
        // See `discovery_device_info_with_open_device`'s matching comment: a
        // cache-hit may have appeared between the caller's own check and
        // this call -- re-checking here is cheap and avoids an unnecessary
        // native call in that narrow window.
        if let Some(result) = self
            .discovery_protocol_info_cache_hit(module_handle, protocol_id, parameter)
            .await
        {
            return Ok(Some(result));
        }
        let key = (module_handle, protocol_id, parameter);
        let query = [DiscoveryParam {
            parameter,
            value: 0,
        }];
        let result = match self
            .api
            .lock()
            .await
            .get_protocol_info(device_id, protocol_id, &query)
        {
            Ok(results) => results
                .into_iter()
                .next()
                .expect("get_protocol_info returns exactly one result per input parameter"),
            Err(err) => {
                return Err(map_native_error_for_link(
                    "PassThruIoctl GET_PROTOCOL_INFO",
                    &err,
                    last_error,
                ));
            }
        };
        // See `discovery_device_info`'s matching comment: re-read the epoch
        // now, since `ensure_open_device_for` may have just bumped it.
        let epoch = self.device_epoch.load(Ordering::SeqCst);
        self.discovery_protocol_info
            .lock()
            .await
            .insert(key, (epoch, result));
        Ok(Some(result))
    }

    /// SAE J2534-2 clause 7 (Additional Channels) capacity precheck (ADR-156
    /// Decision 4/Phase 2b design review correction): before opening a
    /// brand-new `_CHx` physical channel, consults the cached
    /// `DEVICE_INFO_<PROTOCOL>_SUPPORTED` Discovery answer for
    /// `j2534_proto_id`'s family (`resources::chx_device_info_supported_parameter`).
    /// Its packed value's `QQ` byte (bits 16-23, Codex review PR #29 --
    /// NOT bits 8-15, which is `RR`, the unrelated `_PS` channel count) is
    /// the count of available `_CHx` indices for that family, contiguous
    /// from 1 --
    /// `channel_index > QQ` is rejected synchronously with a clean
    /// `invalid_argument`. A no-op (returns `Ok(())`) when
    /// `j2534_proto_id` has no in-scope `DEVICE_INFO_*_SUPPORTED`
    /// parameter, when nothing is cached and the module isn't J2534-2-opted-in
    /// (nothing to query), or when the parameter isn't `Supported` -- in
    /// every such case, the native `PassThruConnect` error remains the
    /// fallback (ADR-153 Decision 1's "Discovery refines, native error is
    /// the fallback").
    ///
    /// `j2534_proto_id` is the caller's RAW, POST-SUBSTITUTION hardware
    /// protocol id (Codex review correction, PR #124, ADR-211) -- NOT
    /// `base_protocol_id(...)`'s fully-normalized output. Passing a
    /// normalized base here would be wrong for the same reason
    /// `resources::connect_discovery_check`'s own doc comment already gives:
    /// `chx_device_info_supported_parameter` now performs its own internal
    /// FT-aware normalization, distinguishing a CAN-collapse family like
    /// Fault-Tolerant CAN from plain CAN before falling back to the generic
    /// per-family lookup.
    ///
    /// `device_id` is the caller's ALREADY-OPEN device handle
    /// (`rpc_connect_com_logical_link`'s own `device_guard`/`device_id`,
    /// from an earlier `ensure_open_device` call still held across this
    /// entire call) -- this function must NOT call
    /// `Self::discovery_device_info` (which internally calls
    /// `ensure_open_device_for`, re-locking the same `self.device_id` mutex
    /// the caller's still-held guard already holds): tokio's `Mutex` is not
    /// reentrant, so that would deadlock the calling task against itself
    /// (found via a hanging integration test, not a panic -- exactly the
    /// class of bug a non-reentrant async mutex produces). Consults the
    /// cache directly, and on a miss, issues the native call against the
    /// already-open `device_id` via
    /// `discovery_device_info_with_open_device` instead.
    ///
    /// `last_error` is caller-supplied, forwarded straight through to
    /// `discovery_device_info_with_open_device` on a cache miss -- same
    /// lock-order rationale as that function's own doc comment: this
    /// function is called from `rpc_connect_com_logical_link`'s
    /// check-create-insert region, which may already hold
    /// `self.shared_channels` (ADR-185's second lock-order fix).
    pub(super) async fn check_chx_capacity(
        &self,
        module_handle: u32,
        device_id: DeviceId,
        j2534_proto_id: u32,
        channel_index: u32,
        last_error: Option<TrackedError>,
    ) -> Result<(), Status> {
        let Some(parameter) = resources::chx_device_info_supported_parameter(j2534_proto_id) else {
            return Ok(());
        };
        if !is_j2534_2_opted_in(self.modules[(module_handle - 1) as usize].pname.as_deref()) {
            return Ok(());
        }
        let cached = self
            .discovery_device_info_cache_hit(module_handle, parameter, 0)
            .await;
        let result = match cached {
            Some(result) => result,
            None => {
                let Some(result) = self
                    .discovery_device_info_with_open_device(
                        module_handle,
                        device_id,
                        parameter,
                        0,
                        last_error,
                    )
                    .await?
                else {
                    return Ok(());
                };
                result
            }
        };
        if !result.supported {
            return Ok(());
        }
        // Packed 0xPPQQRRSS (ADR-156 Corrections, Codex review PR #29): QQ
        // occupies bits 16-23, not 8-15 (that's RR, the _PS channel count) --
        // is the available _CHx count.
        let available = (result.value >> 16) & 0xFF;
        if channel_index > available {
            return Err(Status::invalid_argument(format!(
                "resource_data names a SAE J2534-2 clause 7 Additional Channels (_CHx) index \
                 {channel_index}, which exceeds the {available} such channels this device's \
                 cached DEVICE_INFO_<PROTOCOL>_SUPPORTED Discovery answer reports available for \
                 this protocol family (valid indices 1..={available})",
            )));
        }
        Ok(())
    }

    /// ADR-185 Stage 1: shared Discovery-cache capability enforcement
    /// primitive, generalizing `check_chx_capacity`'s cache-hit/native-call
    /// machinery to any [`DiscoveryCheck`]. Fail-fast fires only on a
    /// DEFINITIVE negative -- `supported == false`, or (`DeviceCapacity`) a
    /// capacity that resolves but falls short of `needed`. Not opted into
    /// J2534-2, no cached/queryable answer (`Ok(None)`), and any
    /// Discovery-query error itself, all fall through as a no-op: this
    /// primitive must never turn an otherwise-successful native call into a
    /// failure because an unrelated Discovery query hiccuped -- "Discovery
    /// refines, native error is the fallback" (ADR-153 Decision 1), carried
    /// forward by ADR-185 Decision 1.
    ///
    /// The J2534-2 opt-in gate (clause 5) is checked HERE, up front, same as
    /// `check_chx_capacity`'s own gate: `discovery_device_info_with_open_device`/
    /// `discovery_protocol_info_with_open_device` (the `AlreadyOpen`
    /// resolution path) do not check it themselves -- their own doc comments
    /// require every caller reaching them to have already handled the gate,
    /// exactly as `check_chx_capacity` already does.
    ///
    /// `device` selects how the Discovery answer is resolved -- see
    /// [`DeviceAccess`]'s own doc comment for the non-reentrant
    /// `self.device_id` deadlock this distinction exists to prevent.
    ///
    /// A rejection reuses `reject_as` (the same `PduError` the native call
    /// would eventually have produced for the same unsupported capability,
    /// ADR-185 Decision 5) via `state_guard_status`, with `last_error` as
    /// read by the caller from the same lock acquisition its own guard
    /// decision was made from (same contract as `state_guard_status` itself
    /// and `map_native_error_for_link`).
    ///
    /// `DiscoveryCheck::ProtocolCapacity` resolves for real under either
    /// `DeviceAccess` variant: `AlreadyOpen` via
    /// [`Self::discovery_protocol_info_with_open_device`] (ADR-185 Stage 2's
    /// `ioctl_start_repeat_message` call site, after its lock-order fix --
    /// see this module's own doc comment), `OpenIfNeeded` via
    /// [`Self::discovery_protocol_info`] (no production caller currently,
    /// but fully implemented and unit-tested).
    pub(super) async fn enforce_discovery_capability(
        &self,
        module_handle: u32,
        device: DeviceAccess,
        check: DiscoveryCheck,
        operation: &str,
        reject_as: PduError,
        last_error: Option<TrackedError>,
    ) -> Result<(), Status> {
        if !is_j2534_2_opted_in(self.modules[(module_handle - 1) as usize].pname.as_deref()) {
            return Ok(());
        }
        match check {
            DiscoveryCheck::DeviceFlag {
                parameter,
                input_value,
            } => {
                let result = match self
                    .resolve_discovery_device_info(
                        module_handle,
                        device,
                        parameter,
                        input_value,
                        last_error.clone(),
                    )
                    .await
                {
                    Ok(Some(result)) => result,
                    Ok(None) => return Ok(()),
                    Err(status) => {
                        warn!(
                            module_handle,
                            parameter,
                            operation,
                            %status,
                            "ADR-185 Discovery-cache connect-time enforcement: the Discovery \
                             query itself failed -- falling through to the native call as the \
                             authority, per ADR-153 Decision 1"
                        );
                        return Ok(());
                    }
                };
                if !result.supported {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        format!(
                            "{operation} rejected by SAE J2534-2 Discovery-cache connect-time \
                             enforcement (ADR-185): the device's cached GET_DEVICE_INFO \
                             parameter {parameter} reports unsupported"
                        ),
                        reject_as,
                        last_error,
                    ));
                }
                Ok(())
            }
            DiscoveryCheck::DeviceCapacity {
                parameter,
                extract,
                needed,
            } => {
                let result = match self
                    .resolve_discovery_device_info(
                        module_handle,
                        device,
                        parameter,
                        0,
                        last_error.clone(),
                    )
                    .await
                {
                    Ok(Some(result)) => result,
                    Ok(None) => return Ok(()),
                    Err(status) => {
                        warn!(
                            module_handle,
                            parameter,
                            operation,
                            %status,
                            "ADR-185 Discovery-cache connect-time enforcement: the Discovery \
                             query itself failed -- falling through to the native call as the \
                             authority, per ADR-153 Decision 1"
                        );
                        return Ok(());
                    }
                };
                if !result.supported || extract(result.value) < needed {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        format!(
                            "{operation} rejected by SAE J2534-2 Discovery-cache connect-time \
                             enforcement (ADR-185): the device's cached GET_DEVICE_INFO \
                             parameter {parameter} reports a capacity below the {needed} this \
                             operation requires"
                        ),
                        reject_as,
                        last_error,
                    ));
                }
                Ok(())
            }
            DiscoveryCheck::ProtocolCapacity {
                protocol_id,
                parameter,
                needed,
            } => {
                let result = match device {
                    DeviceAccess::OpenIfNeeded => {
                        match self
                            .discovery_protocol_info(module_handle, protocol_id, parameter)
                            .await
                        {
                            Ok(Some(result)) => result,
                            Ok(None) => return Ok(()),
                            Err(status) => {
                                warn!(
                                    module_handle,
                                    protocol_id,
                                    parameter,
                                    operation,
                                    %status,
                                    "ADR-185 Discovery-cache connect-time enforcement: the \
                                     Discovery query itself failed -- falling through to the \
                                     native call as the authority, per ADR-153 Decision 1"
                                );
                                return Ok(());
                            }
                        }
                    }
                    // ADR-185 Stage 2 lock-order fix: `ioctl_start_repeat_message`
                    // (`rpc_misc.rs`) is this arm's first production caller
                    // -- see this module's own doc comment for why it
                    // switched from `OpenIfNeeded` to `AlreadyOpen`.
                    DeviceAccess::AlreadyOpen(device_id) => {
                        match self
                            .discovery_protocol_info_with_open_device(
                                module_handle,
                                device_id,
                                protocol_id,
                                parameter,
                                last_error.clone(),
                            )
                            .await
                        {
                            Ok(Some(result)) => result,
                            Ok(None) => return Ok(()),
                            Err(status) => {
                                warn!(
                                    module_handle,
                                    protocol_id,
                                    parameter,
                                    operation,
                                    %status,
                                    "ADR-185 Discovery-cache connect-time enforcement: the \
                                     Discovery query itself failed -- falling through to the \
                                     native call as the authority, per ADR-153 Decision 1"
                                );
                                return Ok(());
                            }
                        }
                    }
                };
                if !result.supported || result.value < needed {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        format!(
                            "{operation} rejected by SAE J2534-2 Discovery-cache connect-time \
                             enforcement (ADR-185): the device's cached GET_PROTOCOL_INFO \
                             parameter {parameter} for protocol {protocol_id} reports a capacity \
                             below the {needed} this operation requires"
                        ),
                        reject_as,
                        last_error,
                    ));
                }
                Ok(())
            }
        }
    }

    /// The `DeviceAccess`-dispatch half of
    /// [`Self::enforce_discovery_capability`]'s `DeviceFlag`/
    /// `DeviceCapacity` resolution, factored out since both arms need the
    /// identical `AlreadyOpen`/`OpenIfNeeded` dispatch.
    ///
    /// `last_error` is forwarded to the `AlreadyOpen` arm's
    /// `discovery_device_info_with_open_device` call, which needs a
    /// caller-supplied snapshot rather than reading `module_state` itself
    /// (ADR-185 Stage 2 lock-order fix -- see that function's own doc
    /// comment). The `OpenIfNeeded` arm's `discovery_device_info` reads its
    /// own snapshot internally instead, since it never holds
    /// `shared_channels`.
    async fn resolve_discovery_device_info(
        &self,
        module_handle: u32,
        device: DeviceAccess,
        parameter: u32,
        input_value: u32,
        last_error: Option<TrackedError>,
    ) -> Result<Option<DiscoveryResult>, Status> {
        match device {
            DeviceAccess::AlreadyOpen(device_id) => {
                self.discovery_device_info_with_open_device(
                    module_handle,
                    device_id,
                    parameter,
                    input_value,
                    last_error,
                )
                .await
            }
            DeviceAccess::OpenIfNeeded => {
                self.discovery_device_info(module_handle, parameter, input_value)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::Arc;

    use j2534_0404_sys::libloading::{Library, Symbol};
    use tokio::sync::Mutex;

    /// Reads the mock's count of `IOCTL_GET_DEVICE_INFO` calls made on the
    /// current thread, through a fresh `libloading::Library` handle rather
    /// than a safe `j2534_0404_mock::mock_get_*` helper: those helpers read
    /// the *caller's own statically-linked* copy of `MockState`, a different
    /// instance from the one the dynamically-loaded `.so` `service.api`
    /// actually mutates (`j2534-0404-mock/docs/testing-guide.md`'s "Loading
    /// the Mock in Tests" section). Loading the identical file path again
    /// resolves to the same shared object, so this observes what
    /// `service.api`'s calls actually did. Mirrors
    /// `tests/grpc_mock/harness.rs`'s `MockBackdoor::open_count`.
    ///
    /// Per-thread, not the process-wide `__mock_get_get_device_info_count`:
    /// every `#[tokio::test]` here runs on its own current-thread runtime,
    /// so its native calls happen on its own thread. A process-wide count
    /// also moved whenever any other test in this binary that reaches
    /// `GET_DEVICE_INFO` (through `enforce_discovery_capability`,
    /// `check_chx_capacity` or an RPC handler) ran in parallel, which broke
    /// the exact before/after deltas below under `cargo test`. Delta-measured
    /// all the same, since earlier tests may have run on a reused thread.
    fn get_device_info_call_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_get_device_info_count_on_current_thread\0")
                .expect("__mock_get_get_device_info_count_on_current_thread should be exported");
            f()
        }
    }

    use super::*;

    const TEST_HANDLE: u32 = 1;

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// no logical links registered and a single configured module whose
    /// `pname` is `pname`. Mirrors `rpc_module.rs::tests::
    /// minimal_service_with_modules`'s construction shape.
    async fn service_with_pname(pname: Option<&str>) -> J2534Service {
        service_with_modules(vec![pname]).await
    }

    /// Same as `service_with_pname`, but with one configured module per
    /// entry in `pnames` (1-based `module_handle`s, in order) -- for
    /// exercising cross-module cache isolation (Codex review on PR #25).
    async fn service_with_modules(pnames: Vec<Option<&str>>) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::SingleChannel,
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(
                pnames
                    .into_iter()
                    .enumerate()
                    .map(|(i, pname)| crate::config::ModuleEntry {
                        label: format!("module-{}", i + 1),
                        pname: pname.map(|p| CString::new(p).unwrap()),
                    })
                    .collect(),
            ),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::new())),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn opt_in_requires_exact_prefix() {
        assert!(is_j2534_2_opted_in(Some(
            &CString::new("J2534-2:mock").unwrap()
        )));
        assert!(is_j2534_2_opted_in(Some(
            &CString::new("J2534-2:").unwrap()
        )));
        assert!(!is_j2534_2_opted_in(None));
        assert!(!is_j2534_2_opted_in(Some(&CString::new("mock").unwrap())));
        assert!(!is_j2534_2_opted_in(Some(
            &CString::new("j2534-2:mock").unwrap()
        )));
    }

    #[tokio::test]
    async fn device_info_skips_the_native_call_when_not_opted_in() {
        let service = service_with_pname(None).await;
        let result = service
            .discovery_device_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::DEVICE_INFO_CAN_SUPPORTED,
                0,
            )
            .await
            .expect("gated lookup must not error");
        assert_eq!(result, None, "a non-opted-in module must never be queried");
        assert!(
            service.device_id.lock().await.is_none(),
            "the device must not even be opened for a gated lookup"
        );
    }

    #[tokio::test]
    async fn device_info_reports_supported_for_a_base_protocol() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = service
            .discovery_device_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::DEVICE_INFO_CAN_SUPPORTED,
                0,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(result.supported);
        // ADR-156 Decision 4/Phase 2b: the mock now packs its default SAE
        // J2534-2 clause 7 Additional Channels capacity (4) into the `QQ`
        // byte (bits 16-23, Codex review PR #29) rather than reporting a
        // flat `0x0000_0001` -- see `j2534-0404-mock`'s
        // `DEFAULT_CHX_CAPACITY`.
        assert_eq!(result.value, 0x0004_0001);
    }

    // Was keyed on `DEVICE_INFO_J1939_SUPPORTED` until the mechanical
    // extension of ADR-211's/ADR-212's established pattern
    // (`j2534-0404-mock`'s `IOCTL_GET_DEVICE_INFO` handler gaining a J1939
    // arm alongside `resources.rs::chx_device_info_supported_parameter`'s
    // own new guard arm): this mock DOES implement SAE J1939 (see
    // `tests/grpc_mock/j1939.rs`), so `DEVICE_INFO_J1939_SUPPORTED` is no
    // longer a genuinely-unimplemented capability -- re-keyed to
    // `DEVICE_INFO_TP2_0_SIMULTANEOUS`, one of the deliberately-unwired
    // `_SIMULTANEOUS` residual flags (ADR-188 §7's accepted residual; no
    // arm in the mock's handler answers it, so it still falls through to
    // the handler's own `_ => Supported = 0` default).
    #[tokio::test]
    async fn device_info_reports_not_supported_for_a_j2534_2_only_capability() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = service
            .discovery_device_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::DEVICE_INFO_TP2_0_SIMULTANEOUS,
                0,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(
            !result.supported,
            "mock leaves _SIMULTANEOUS residual flags unwired -- DEVICE_INFO_TP2_0_SIMULTANEOUS \
             has no arm in the mock's IOCTL_GET_DEVICE_INFO handler and must fall through to its \
             default Supported = 0"
        );
    }

    /// SAE J2534-2 clause 23 (Phase 13, Codex review PR #48 finding): unlike
    /// `DEVICE_INFO_TP2_0_SIMULTANEOUS` above (a capability this mock
    /// deliberately leaves unwired), the mock DOES implement
    /// `IOCTL_READ_J1962PIN_VOLTAGE`
    /// (`j2534-0404-mock`'s own `IOCTL_GET_DEVICE_INFO` dispatcher), so it
    /// must also advertise it as supported here -- an opted-in client
    /// following the clause 25 discovery-first workflow would otherwise
    /// wrongly conclude the already-implemented operation is unavailable.
    ///
    /// Queries pin 1 (bit 0), not `input_value: 0` -- since the mock-fidelity
    /// fix making this parameter's mock answer per-pin-aware (matching Table
    /// 111/clause 25.3.2.2's low-half bitmap convention, same as
    /// `SHORT_TO_GND_J1962`), `0` selects no pin at all and is correctly
    /// reported unsupported, same as pin 0 on the real
    /// `IOCTL_READ_J1962PIN_VOLTAGE` handler.
    #[tokio::test]
    async fn device_info_reports_supported_for_j1962_pin_voltage_read() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let supported = service
            .discovery_device_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED,
                1u32,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(supported.supported);

        let max = service
            .discovery_device_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::DEVICE_INFO_READ_J1962PIN_VOLTAGE_MAX,
                0,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(max.supported);
        assert_eq!(
            max.value, 24_000,
            "the mock's reported max voltage must match its own \
             IOCTL_READ_J1962PIN_VOLTAGE dispatcher (24 VDC minimum required range, clause 23)"
        );
    }

    /// Issues a raw `GET_DEVICE_INFO` query with a caller-controlled input
    /// `value`, bypassing `discovery_device_info`'s cache (which always
    /// sends `value: 0`, per its own `discovery_device_info_with_open_device`
    /// implementation) -- needed to exercise the per-pin
    /// `DEVICE_INFO_SHORT_TO_GND_J1962`/`DEVICE_INFO_PGM_VOLTAGE_J1962`
    /// queries (SAE J2534-2 clause 25.3.2.2), which read `Value` as an INPUT
    /// pin-selection bitmask rather than being a flat, no-input capability
    /// flag like every other `DEVICE_INFO_*` parameter this cache currently
    /// queries.
    async fn raw_device_info_query(
        service: &J2534Service,
        module_handle: u32,
        parameter: u32,
        value: u32,
    ) -> DiscoveryResult {
        let (_guard, device_id) = service
            .ensure_open_device_for(module_handle)
            .await
            .expect("device should open");
        let query = [DiscoveryParam { parameter, value }];
        service
            .api
            .lock()
            .await
            .get_device_info(device_id, &query)
            .expect("native GET_DEVICE_INFO call should succeed")
            .into_iter()
            .next()
            .expect("get_device_info returns exactly one result per input parameter")
    }

    /// SAE J2534-2 clause 15.3.2.1: pin 9 gains short-to-ground capability
    /// on the SAE J1962 connector, in addition to pin 15's pre-existing
    /// J2534-1-era support. Bit 0 = pin 1, so pin 9's bit is bit 8.
    #[tokio::test]
    async fn device_info_reports_supported_for_short_to_gnd_j1962_pin_9() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32 << 8;
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_SHORT_TO_GND_J1962,
            input_value,
        )
        .await;
        assert!(result.supported);
        assert_eq!(
            result.value, input_value,
            "clause 25.3.2.2's input pin-selection bitmap must remain un-altered"
        );
    }

    /// Same as `device_info_reports_supported_for_short_to_gnd_j1962_pin_9`,
    /// for pin 15 (bit 14) -- the pre-existing J2534-1-era pin.
    #[tokio::test]
    async fn device_info_reports_supported_for_short_to_gnd_j1962_pin_15() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32 << 14;
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_SHORT_TO_GND_J1962,
            input_value,
        )
        .await;
        assert!(result.supported);
        assert_eq!(result.value, input_value);
    }

    /// A pin other than 9 or 15 (pin 1, bit 0) must not be reported
    /// supported for short-to-ground on the SAE J1962 connector.
    #[tokio::test]
    async fn device_info_reports_not_supported_for_short_to_gnd_j1962_other_pin() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32;
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_SHORT_TO_GND_J1962,
            input_value,
        )
        .await;
        assert!(!result.supported);
        assert_eq!(result.value, input_value);
    }

    /// No bit set is not a valid single-pin selector and must not be
    /// reported supported.
    #[tokio::test]
    async fn device_info_reports_not_supported_for_short_to_gnd_j1962_zero_bits() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_SHORT_TO_GND_J1962,
            0,
        )
        .await;
        assert!(!result.supported);
        assert_eq!(result.value, 0);
    }

    /// Multiple bits set (pin 9 and pin 15 together) is not a valid
    /// single-pin selector, even though both individual pins are
    /// independently supported -- the query is answering "is THIS pin
    /// supported", not "is any bit in this bitmap a supported pin".
    #[tokio::test]
    async fn device_info_reports_not_supported_for_short_to_gnd_j1962_multiple_bits() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = (1u32 << 8) | (1u32 << 14);
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_SHORT_TO_GND_J1962,
            input_value,
        )
        .await;
        assert!(!result.supported);
        assert_eq!(result.value, input_value);
    }

    /// SAE J2534-2 clause 15.3.2.1: pin 9 also gains programming-voltage
    /// capability. Per clause 25.3.2.2, `PGM_VOLTAGE_J1962`'s pin bit lives
    /// in the HIGH nibble-pair (`0xHHHHLLLL`), so pin 9's bit is bit 24
    /// (`16 + 8`).
    #[tokio::test]
    async fn device_info_reports_supported_for_pgm_voltage_j1962_pin_9() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32 << (16 + 8);
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_PGM_VOLTAGE_J1962,
            input_value,
        )
        .await;
        assert!(result.supported);
        assert_eq!(result.value, input_value);
    }

    /// Same as `device_info_reports_supported_for_pgm_voltage_j1962_pin_9`,
    /// for pin 15 (bit 30, `16 + 14`) -- the pre-existing J2534-1-era pin.
    #[tokio::test]
    async fn device_info_reports_supported_for_pgm_voltage_j1962_pin_15() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32 << (16 + 14);
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_PGM_VOLTAGE_J1962,
            input_value,
        )
        .await;
        assert!(result.supported);
        assert_eq!(result.value, input_value);
    }

    /// A pin other than 9 or 15 (pin 1, bit 16) must not be reported
    /// supported for programming voltage on the SAE J1962 connector.
    #[tokio::test]
    async fn device_info_reports_not_supported_for_pgm_voltage_j1962_other_pin() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let input_value = 1u32 << 16;
        let result = raw_device_info_query(
            &service,
            TEST_HANDLE,
            j2534_0404_sys::bindings::DEVICE_INFO_PGM_VOLTAGE_J1962,
            input_value,
        )
        .await;
        assert!(!result.supported);
        assert_eq!(result.value, input_value);
    }

    #[tokio::test]
    async fn device_info_cache_hit_avoids_a_second_native_call() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let parameter = j2534_0404_sys::bindings::DEVICE_INFO_CAN_SUPPORTED;
        // Delta-measured against the mock's per-thread call counter (see
        // `get_device_info_call_count`). The counter is the only reliable proof that
        // a "hit" really skipped the native call -- closing the device (the
        // previous approach) doesn't work, since `ensure_open_device_for`
        // lazily reopens rather than failing on a closed/absent device
        // (edge-case-hunter finding: that version passed whether or not
        // caching worked at all).
        let before = get_device_info_call_count();
        let first = service
            .discovery_device_info(TEST_HANDLE, parameter, 0)
            .await
            .unwrap();
        let after_first = get_device_info_call_count();
        assert_eq!(
            after_first - before,
            1,
            "the first query must issue exactly one native call"
        );

        let second = service
            .discovery_device_info(TEST_HANDLE, parameter, 0)
            .await
            .unwrap();
        assert_eq!(
            get_device_info_call_count(),
            after_first,
            "a cache hit must not issue another native call"
        );
        assert_eq!(first, second);
    }

    /// Codex review on PR #25: an epoch-only cache key let a query for
    /// module 2 silently be served module 1's cached answer whenever the
    /// epoch happened to still match, entirely bypassing
    /// `ensure_open_device_for`'s resource-busy check (which only runs on a
    /// miss). With `module_handle` in the key, module 1's cached entry is
    /// simply invisible to a module-2 lookup, so the miss path runs and
    /// correctly rejects instead.
    #[tokio::test]
    async fn device_info_cache_does_not_leak_across_modules() {
        let service = service_with_modules(vec![Some("J2534-2:one"), Some("J2534-2:two")]).await;
        let parameter = j2534_0404_sys::bindings::DEVICE_INFO_CAN_SUPPORTED;

        let for_module_one = service
            .discovery_device_info(1, parameter, 0)
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(for_module_one.supported);

        let status = service
            .discovery_device_info(2, parameter, 0)
            .await
            .expect_err(
                "module 1 is the one actually open -- a query for module 2 must not be \
                 silently served module 1's cached answer",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);
    }

    #[tokio::test]
    async fn device_epoch_bump_invalidates_a_stale_cache_entry() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let parameter = j2534_0404_sys::bindings::DEVICE_INFO_CAN_SUPPORTED;
        let before = get_device_info_call_count();
        service
            .discovery_device_info(TEST_HANDLE, parameter, 0)
            .await
            .unwrap();
        let after_first = get_device_info_call_count();
        assert_eq!(after_first - before, 1);

        // Bump the epoch directly (simulating a ModuleDisconnect + reopen)
        // without touching the cache map itself -- proves the epoch tag,
        // not just an empty map, is what re-triggers the native call.
        service.device_epoch.fetch_add(1, Ordering::SeqCst);
        let cached_epoch = service
            .discovery_device_info
            .lock()
            .await
            .get(&(TEST_HANDLE, parameter, 0))
            .unwrap()
            .0;
        assert_ne!(
            cached_epoch,
            service.device_epoch.load(Ordering::SeqCst),
            "the stored entry must now be stale"
        );
        // A stale-epoch read still succeeds (re-queries transparently) and
        // issues exactly one more native call -- proving the re-query
        // actually happened, not just that the (identical, static-per-spec)
        // answer looked unchanged.
        let result = service
            .discovery_device_info(TEST_HANDLE, parameter, 0)
            .await
            .unwrap();
        assert_eq!(
            get_device_info_call_count() - after_first,
            1,
            "a stale-epoch entry must trigger exactly one fresh native call"
        );
        assert!(result.unwrap().supported);
    }

    // ── ADR-156 Decision 4/Phase 2b: `check_chx_capacity` (SAE J2534-2
    // clause 7 Additional Channels capacity precheck) tests ──────────────

    #[tokio::test]
    async fn chx_capacity_rejects_an_index_above_the_cached_count() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        // `check_chx_capacity` is always called while the caller's own
        // `device_guard` is still held (see its own doc comment) -- opening
        // the device here mirrors that contract for the test.
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        // The mock's default capacity is 4 (`DEFAULT_CHX_CAPACITY`); index 5
        // exceeds it. Caching happens as a side effect of the call itself.
        let status = service
            .check_chx_capacity(TEST_HANDLE, device_id, j2534_0404::CAN, 5, None)
            .await
            .expect_err("an index above the cached capacity must be rejected synchronously");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn chx_capacity_accepts_an_index_within_the_cached_count() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        service
            .check_chx_capacity(TEST_HANDLE, device_id, j2534_0404::CAN, 4, None)
            .await
            .expect("an index within the cached capacity must succeed");
    }

    #[tokio::test]
    async fn chx_capacity_is_a_noop_when_not_opted_in() {
        // A non-opted-in module returns before `device_id` is ever used, so
        // a dummy value (no device opened at all) is fine here -- proves
        // `check_chx_capacity` never even attempts to use it.
        let service = service_with_pname(None).await;
        service
            .check_chx_capacity(
                TEST_HANDLE,
                j2534_0404::DeviceId(0),
                j2534_0404::CAN,
                5,
                None,
            )
            .await
            .expect(
                "a non-opted-in module's precheck must fall through to the native connect \
                 error, not reject on its own",
            );
        assert!(
            service.device_id.lock().await.is_none(),
            "a non-opted-in module's precheck must not open a device"
        );
    }

    #[tokio::test]
    async fn chx_capacity_is_a_noop_for_an_out_of_scope_base_protocol() {
        // No `DEVICE_INFO_*_SUPPORTED` parameter exists for an out-of-scope
        // family (GM UART -- ADR-189 Decision 5's deliberately accepted
        // residual, still uncovered after the mechanical extension of
        // ADR-211's/ADR-212's established pattern to SAE J1939, UART Echo
        // Byte, Honda DIAG-H, SAE J1708, and TP2.0; re-keyed from
        // `PROTOCOL_J1939_PS` since that family is no longer out of scope)
        // -- `chx_device_info_supported_parameter` returns `None`, so this
        // must never even attempt a Discovery query (a dummy `device_id`
        // proves `check_chx_capacity` never uses it on this path either).
        let service = service_with_pname(Some("J2534-2:mock")).await;
        service
            .check_chx_capacity(
                TEST_HANDLE,
                j2534_0404::DeviceId(0),
                j2534_0404_sys::bindings::PROTOCOL_GM_UART_PS,
                5,
                None,
            )
            .await
            .expect("an out-of-scope base protocol must be a no-op, not an error");
        assert!(
            service.device_id.lock().await.is_none(),
            "an out-of-scope base protocol's precheck must not open a device"
        );
    }

    /// Regression test (Codex review correction, PR #124, ADR-211):
    /// `check_chx_capacity`'s third parameter is now keyed on the raw,
    /// pre-normalization `j2534_proto_id` -- passing a Fault-Tolerant CAN
    /// `_PS` id directly must reach the actual capacity-comparison logic
    /// (a Discovery query for `DEVICE_INFO_FT_CAN_SUPPORTED`, then an
    /// `invalid_argument` on an out-of-range index), not silently no-op via
    /// `chx_device_info_supported_parameter` matching nothing.
    ///
    /// The `InvalidArgument` outcome alone does not discriminate this fix
    /// from the pre-fix bug it corrects: the mock's `chx_capacity_override`
    /// is a single value shared by every packed-capacity family (ADR-156
    /// Decision 4's mock arm), so an index of 5 is rejected identically
    /// whether this call actually consulted `DEVICE_INFO_FT_CAN_SUPPORTED`
    /// (the fix) or wrongly fell through to `DEVICE_INFO_CAN_SUPPORTED`
    /// (the bug this fix corrects, `chx_device_info_supported_parameter`
    /// previously being keyed on the fully-normalized base) -- both report
    /// the same `DEFAULT_CHX_CAPACITY` (4) and reject index 5 either way
    /// (edge-case-hunter finding, verified by hand-reverting just the two
    /// FT guard arms in `resources::chx_device_info_supported_parameter`
    /// and confirming this assertion alone still passed). The cache-key
    /// assertion below is what actually discriminates: only the fix queries
    /// (and therefore caches) `DEVICE_INFO_FT_CAN_SUPPORTED` for this call;
    /// the bug would have cached `DEVICE_INFO_CAN_SUPPORTED` instead.
    #[tokio::test]
    async fn chx_capacity_keys_ft_can_by_the_raw_ft_id_not_the_collapsed_base() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        let status = service
            .check_chx_capacity(
                TEST_HANDLE,
                device_id,
                j2534_0404::PROTOCOL_FT_CAN_PS,
                5,
                None,
            )
            .await
            .expect_err(
                "a raw Fault-Tolerant CAN _PS id must reach the capacity-comparison logic \
                 (via DEVICE_INFO_FT_CAN_SUPPORTED), not silently no-op",
            );
        assert_eq!(status.code(), Code::InvalidArgument);
        let cache = service.discovery_device_info.lock().await;
        assert!(
            cache.contains_key(&(TEST_HANDLE, j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED, 0)),
            "the FT-specific DEVICE_INFO_FT_CAN_SUPPORTED parameter must have been queried \
             and cached -- this is the assertion that actually distinguishes the fix from the \
             bug it corrects"
        );
        assert!(
            !cache.contains_key(&(TEST_HANDLE, j2534_0404::DEVICE_INFO_CAN_SUPPORTED, 0)),
            "the generic DEVICE_INFO_CAN_SUPPORTED parameter must NOT have been queried for a \
             Fault-Tolerant CAN capacity check -- that would be the pre-fix bug"
        );
    }

    // ── ADR-185 Stage 1: `enforce_discovery_capability` (SAE J2534-2
    // Discovery-cache connect-time enforcement) tests -- SWCAN and Analog
    // Inputs as representative `DeviceFlag` connect-path rows, mirroring
    // `check_chx_capacity`'s own test shapes above. ────────────────────────

    #[tokio::test]
    async fn enforce_discovery_capability_is_a_noop_when_not_opted_in() {
        // A non-opted-in module returns before `device_id` is ever used, so
        // a dummy value (no device opened at all) is fine here -- proves
        // `enforce_discovery_capability` never even attempts to use it,
        // mirroring `chx_capacity_is_a_noop_when_not_opted_in` above.
        let service = service_with_pname(None).await;
        let check = resources::connect_discovery_check(j2534_0404::PROTOCOL_SW_CAN_PS)
            .expect("SWCAN must have a connect-time Discovery check mapping (ADR-185)");
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(j2534_0404::DeviceId(0)),
                check,
                "ConnectComLogicalLink",
                PduError::PduErrIdNotSupported,
                None,
            )
            .await
            .expect(
                "a non-opted-in module's enforcement must fall through to the native connect \
                 error, not reject on its own",
            );
        assert!(
            service.device_id.lock().await.is_none(),
            "a non-opted-in module's enforcement must not open a device"
        );
    }

    #[tokio::test]
    async fn enforce_discovery_capability_rejects_swcan_when_device_reports_unsupported() {
        // The mock implements SAE J2534-2 clause 9 Single Wire CAN (see
        // `tests/grpc_mock/sw_can.rs`), so `DEVICE_INFO_SW_CAN_SUPPORTED`
        // itself is advertised supported -- ADR-185 Stage 1 added that
        // advertisement precisely so this real connect-time enforcement
        // wouldn't reject SWCAN's own already-working connects. To exercise
        // the rejection path without inventing a new mock backdoor, this
        // test builds the `DiscoveryCheck` directly (not via
        // `resources::connect_discovery_check`, which only ever maps to a
        // parameter the mock now reports supported) against
        // `DEVICE_INFO_TP2_0_SIMULTANEOUS` -- one of the deliberately-
        // unwired `_SIMULTANEOUS` residual flags (ADR-188 §7's accepted
        // residual) this mock genuinely still reports unsupported by
        // default (re-keyed from `DEVICE_INFO_J1939_SUPPORTED`, which is no
        // longer unsupported after the mechanical extension of ADR-211's/
        // ADR-212's established pattern gave this mock's
        // `IOCTL_GET_DEVICE_INFO` handler a J1939 arm -- mirrors
        // `device_info_reports_not_supported_for_a_j2534_2_only_capability`
        // above), proving `enforce_discovery_capability`'s own rejection
        // logic fires on a definitive negative regardless of which
        // `DeviceFlag` parameter it's checking.
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        let check = DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404_sys::bindings::DEVICE_INFO_TP2_0_SIMULTANEOUS,
            input_value: 0,
        };
        let status = service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(device_id),
                check,
                "ConnectComLogicalLink",
                PduError::PduErrIdNotSupported,
                None,
            )
            .await
            .expect_err(
                "the mock reports DEVICE_INFO_TP2_0_SIMULTANEOUS unsupported by default -- \
                 enforce_discovery_capability must reject on a definitive negative",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrIdNotSupported as i32);
    }

    #[tokio::test]
    async fn enforce_discovery_capability_is_a_noop_when_not_opted_in_for_analog_in() {
        let service = service_with_pname(None).await;
        let check = resources::connect_discovery_check(j2534_0404::PROTOCOL_ANALOG_IN_1)
            .expect("Analog Inputs must have a connect-time Discovery check mapping (ADR-185)");
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(j2534_0404::DeviceId(0)),
                check,
                "ConnectComLogicalLink",
                PduError::PduErrIdNotSupported,
                None,
            )
            .await
            .expect(
                "a non-opted-in module's enforcement must fall through to the native connect \
                 error, not reject on its own",
            );
        assert!(
            service.device_id.lock().await.is_none(),
            "a non-opted-in module's enforcement must not open a device"
        );
    }

    #[tokio::test]
    async fn enforce_discovery_capability_accepts_analog_in_when_device_reports_supported() {
        // This mock implements the 32 native `PROTOCOL_ANALOG_IN_x` ids, so
        // `DEVICE_INFO_ANALOG_IN_SUPPORTED` is reported supported by
        // default (`j2534-0404-mock`'s own `IOCTL_GET_DEVICE_INFO` handler)
        // -- no mock backdoor override needed to exercise the acceptance.
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        let check = resources::connect_discovery_check(j2534_0404::PROTOCOL_ANALOG_IN_1)
            .expect("Analog Inputs must have a connect-time Discovery check mapping (ADR-185)");
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(device_id),
                check,
                "ConnectComLogicalLink",
                PduError::PduErrIdNotSupported,
                None,
            )
            .await
            .expect(
                "the mock reports DEVICE_INFO_ANALOG_IN_SUPPORTED supported by default -- \
                 Analog Inputs's connect-time Discovery check must accept",
            );
    }

    #[tokio::test]
    async fn protocol_info_reports_a_known_limit_for_a_base_protocol() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = service
            .discovery_protocol_info(
                TEST_HANDLE,
                j2534_0404::CAN,
                j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_RX_BUFFER_SIZE,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(result.supported);
        assert_eq!(result.value, 4128);
    }

    /// SAE J2534-2 clause 25.3.2.3 Repeat Messaging discovery: must match
    /// this mock's own real `IOCTL_START_REPEAT_MESSAGE` enforcement
    /// (`MAX_REPEAT_SLOTS_PER_CHANNEL` in `j2534-0404-mock/src/lib.rs`,
    /// currently `10`).
    #[tokio::test]
    async fn protocol_info_reports_max_repeat_messaging() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = service
            .discovery_protocol_info(
                TEST_HANDLE,
                j2534_0404::CAN,
                j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(result.supported);
        assert_eq!(result.value, 10);
    }

    /// Same as `protocol_info_reports_max_repeat_messaging`, for
    /// `PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH` -- ADR-186 replaced the
    /// mock's flat structural-buffer answer (`j2534-0404`'s
    /// `MAX_MESSAGE_DATA`) with each protocol's own periodic-message cap,
    /// mirroring `ioctl_start_repeat_message`'s own enforcement; raw CAN
    /// gets the generic 12-byte cap.
    #[tokio::test]
    async fn protocol_info_reports_max_repeat_messaging_length() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let result = service
            .discovery_protocol_info(
                TEST_HANDLE,
                j2534_0404::CAN,
                j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH,
            )
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(result.supported);
        assert_eq!(result.value, 12);
    }

    #[tokio::test]
    async fn protocol_info_maps_an_unknown_protocol_id_to_invalid_protocol_id() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let status = service
            .discovery_protocol_info(
                TEST_HANDLE,
                j2534_0404_sys::bindings::PROTOCOL_J1939_PS,
                j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_RX_BUFFER_SIZE,
            )
            .await
            .expect_err("an unimplemented J2534-2 protocol id must be rejected");
        assert_eq!(status.code(), Code::Internal);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrIdNotSupported as i32);
    }

    /// Same as `device_info_cache_does_not_leak_across_modules`, for
    /// `discovery_protocol_info` (Codex review on PR #25 explicitly flagged
    /// both methods as sharing this bug).
    #[tokio::test]
    async fn protocol_info_cache_does_not_leak_across_modules() {
        let service = service_with_modules(vec![Some("J2534-2:one"), Some("J2534-2:two")]).await;
        let parameter = j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_RX_BUFFER_SIZE;

        let for_module_one = service
            .discovery_protocol_info(1, j2534_0404::CAN, parameter)
            .await
            .expect("query should succeed")
            .expect("opted-in module must be queried");
        assert!(for_module_one.supported);

        let status = service
            .discovery_protocol_info(2, j2534_0404::CAN, parameter)
            .await
            .expect_err(
                "module 1 is the one actually open -- a query for module 2 must not be \
                 silently served module 1's cached answer",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrResourceBusy as i32);
    }

    // ── ADR-185 Stage 2: `enforce_discovery_capability`'s `ProtocolCapacity`
    // arm (SAE J2534-2 clause 14/25.3.2.3 Repeat Messaging), mirroring the
    // Stage 1 `DeviceFlag`/`DeviceCapacity` test shapes above. ────────────

    #[tokio::test]
    async fn enforce_discovery_capability_is_a_noop_for_protocol_capacity_when_not_opted_in() {
        // Mirrors `enforce_discovery_capability_is_a_noop_when_not_opted_in`
        // above, for the `ProtocolCapacity`/`OpenIfNeeded` combination --
        // `ioctl_start_repeat_message` (ADR-185 Stage 2) uses `AlreadyOpen`
        // in production (see this module's own doc comment for why), not
        // `OpenIfNeeded`; this test exercises the not-opted-in short-circuit,
        // which is identical under either `DeviceAccess` variant, so
        // `OpenIfNeeded` still applies here without loss of coverage.
        let service = service_with_pname(None).await;
        let check = DiscoveryCheck::ProtocolCapacity {
            protocol_id: j2534_0404::CAN,
            parameter: j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            needed: 1,
        };
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::OpenIfNeeded,
                check,
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                None,
            )
            .await
            .expect(
                "a non-opted-in module's enforcement must fall through to the native IOCTL \
                 error, not reject on its own",
            );
        assert!(
            service.device_id.lock().await.is_none(),
            "a non-opted-in module's enforcement must not open a device"
        );
    }

    #[tokio::test]
    async fn enforce_discovery_capability_rejects_protocol_capacity_shortfall() {
        // The mock's `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` is 10
        // (`protocol_info_reports_max_repeat_messaging` above) -- `needed:
        // 11` must be rejected.
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let check = DiscoveryCheck::ProtocolCapacity {
            protocol_id: j2534_0404::CAN,
            parameter: j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            needed: 11,
        };
        let status = service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::OpenIfNeeded,
                check,
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                None,
            )
            .await
            .expect_err(
                "the mock's cached PROTOCOL_INFO_MAX_REPEAT_MESSAGING (10) falls short of the \
                 11 slots this check requires -- enforce_discovery_capability must reject",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrResourceError as i32);
    }

    #[tokio::test]
    async fn enforce_discovery_capability_accepts_protocol_capacity_when_sufficient() {
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let check = DiscoveryCheck::ProtocolCapacity {
            protocol_id: j2534_0404::CAN,
            parameter: j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            needed: 10,
        };
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::OpenIfNeeded,
                check,
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                None,
            )
            .await
            .expect(
                "the mock's cached PROTOCOL_INFO_MAX_REPEAT_MESSAGING (10) meets the 10 slots \
                 this check requires -- enforce_discovery_capability must accept",
            );
    }

    #[tokio::test]
    async fn enforce_discovery_capability_rejects_protocol_capacity_shortfall_already_open() {
        // ADR-185 Stage 2 lock-order fix (design-advisor review, same-PR
        // correction): `ioctl_start_repeat_message` (`rpc_misc.rs`) now
        // resolves `ProtocolCapacity` via `DeviceAccess::AlreadyOpen`
        // instead of `OpenIfNeeded` (holding `device_guard` up front,
        // before `shared_channels`, closes an AB-BA deadlock risk against
        // `ConnectComLogicalLink`'s own lock order -- see this module's own
        // doc comment). `discovery_protocol_info_with_open_device` resolves
        // this combination for real now, mirroring
        // `enforce_discovery_capability_rejects_swcan_when_device_reports_unsupported`'s
        // `AlreadyOpen` shape above. The mock's cached
        // `PROTOCOL_INFO_MAX_REPEAT_MESSAGING` is 10
        // (`protocol_info_reports_max_repeat_messaging` above) -- `needed:
        // 11` must be rejected.
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        let check = DiscoveryCheck::ProtocolCapacity {
            protocol_id: j2534_0404::CAN,
            parameter: j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            needed: 11,
        };
        let status = service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(device_id),
                check,
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                None,
            )
            .await
            .expect_err(
                "the mock's cached PROTOCOL_INFO_MAX_REPEAT_MESSAGING (10) falls short of the \
                 11 slots this check requires -- enforce_discovery_capability must reject",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrResourceError as i32);
    }

    #[tokio::test]
    async fn enforce_discovery_capability_accepts_protocol_capacity_when_sufficient_already_open() {
        // See `enforce_discovery_capability_rejects_protocol_capacity_shortfall_already_open`'s
        // comment: `AlreadyOpen` is now `ProtocolCapacity`'s real,
        // production-used resolution path.
        let service = service_with_pname(Some("J2534-2:mock")).await;
        let (_guard, device_id) = service.ensure_open_device_for(TEST_HANDLE).await.unwrap();
        let check = DiscoveryCheck::ProtocolCapacity {
            protocol_id: j2534_0404::CAN,
            parameter: j2534_0404_sys::bindings::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
            needed: 10,
        };
        service
            .enforce_discovery_capability(
                TEST_HANDLE,
                DeviceAccess::AlreadyOpen(device_id),
                check,
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                None,
            )
            .await
            .expect(
                "the mock's cached PROTOCOL_INFO_MAX_REPEAT_MESSAGING (10) meets the 10 slots \
                 this check requires -- enforce_discovery_capability must accept",
            );
    }
}
