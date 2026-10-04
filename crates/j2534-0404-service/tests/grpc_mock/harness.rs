//! Shared scaffolding for the `grpc_mock` integration test: a gRPC server
//! backed by a `J2534Service` that loads the compiled `j2534-0404-mock`
//! cdylib, a back door into the mock's process-global state, and common
//! request/wait/assertion helpers.
//!
//! Because the service loads the mock as a dynamically-loaded module (not as
//! an `rlib`), the mock's safe Rust accessors (`j2534_0404_mock::mock_get_*`)
//! operate on a *different* copy of the mock's process-global state than the
//! one the service is talking to. [`MockBackdoor`] dynamically loads the
//! identical library file path itself (which the dynamic linker resolves to
//! the very same loaded module as the service's copy) and calls the mock's
//! `__mock_*` FFI exports directly, so it observes the state the service
//! actually produced.
//!
//! Every `Symbol` below is loaded as `extern "system"`, not `extern "C"`:
//! `j2534-0404-mock`'s `exported_fn!` macro exports every function it wraps
//! -- including these `__mock_*` backdoors, not just the native J2534 entry
//! points -- as `extern "stdcall"` on the `i686-pc-windows-gnullvm` target
//! (32-bit Windows, matching the real J2534 API's calling convention there)
//! and `extern "C"` everywhere else. `extern "system"` resolves to the same
//! platform-correct choice on the caller side, so this stays correct on both
//! this crate's `x86_64-unknown-linux-gnu` test-execution target and the
//! `i686-pc-windows-gnullvm` cross-check target -- a hardcoded `extern "C"`
//! here would be an ABI mismatch that could corrupt the stack or fail symbol
//! resolution on 32-bit Windows (Codex review, PR #153, which fixed the
//! identical mismatch in `iso22900-service`'s own `MockBackdoor`; tracked as
//! a not-fixed backlog item for this file until now, see
//! the Prioritized Backlog).
//!
//! ## Writing new timing-sensitive tests (ADR-149)
//!
//! Five flaky-test incidents in this file's siblings (see
//! `docs/implementation-notes.md`'s "Known Flaky Tests" / "Resolved"
//! history) resolved to three write-time causes and one wrong fix
//! attempt. Follow these when a new test waits on a timer, an idle
//! interval, or a deadline, **or** asserts against shared/queued state
//! (an event queue, a per-entity counter) that a concurrent, unrelated
//! entity could also satisfy:
//!
//! - **Margin per round trip, not a flat total.** Budget
//!   [`GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`] **once per client-observable
//!   round trip inside the window being tested**, not once total — a test
//!   doing two round trips (e.g. disconnect then reconnect) needs roughly
//!   double the margin a single-round-trip sibling needs with the
//!   identical configured interval. Generous headroom (this repo's
//!   resolved fixes used ~3-7x the per-round-trip ceiling) beats a tight
//!   calculation.
//! - **Anchor deadlines; don't chain relative sleeps.** Use
//!   `tokio::time::sleep_until(deadline)` against a single reference
//!   instant for anything asserting timer/deadline behavior (idle timers,
//!   periodic dispatch, gap timeouts). Chained relative `sleep(duration)`
//!   calls let each call's own scheduling slop compound.
//! - **Scope assertions on shared/queued state to the entity under test.** A
//!   predicate like "is this COP finished" that doesn't filter by the
//!   specific handle/entity under test can be satisfied by an unrelated
//!   entity's activity (e.g. another COP's own terminal event sitting in
//!   the same queue). Follow the `is_finished_for_cop_a`-style naming
//!   convention already used in this file's siblings.
//! - **Never weaken an exact assertion to fix a timing flake.** If an
//!   `== 1`-style count (or any exact-match assertion) is racing round-trip
//!   latency, widen the configured interval/window so the exact assertion
//!   has real margin — do not relax it to `>= 1` or drop the count. A
//!   relaxed assertion can silently stop distinguishing the regression it
//!   exists to catch (a Codex review caught exactly this on this repo's
//!   PR history; see ADR-149). When widening, keep the elapsed/upper bound
//!   meaningfully below the interval's un-overridden default, so a
//!   regression where the override silently didn't take effect is still
//!   caught rather than passing inside a too-generous window.
//!
//! ## A sixth, distinct flaky-test cause: shared startup state, not timing
//!
//! Beyond the five timing-margin incidents above, one flaky-test root cause
//! in this file's history was a genuine concurrency race rather than an
//! insufficient wall-clock margin: concurrent `TestServer::start*` calls
//! racing on shared process-global state (a fixed temp config file path and
//! the `VCI_CONFIG_PATH` env var). See [`TestServer::try_start_with_extra_config`]'s
//! doc comment and `docs/implementation-notes.md`'s "Known Flaky Tests" /
//! "Resolved (2026-08-07)" entry for the full mechanism and the original fix
//! (`VCI_CONFIG_STARTUP_LOCK`). None of the margin/deadline/scoping
//! guidelines above apply to this class of bug -- widening a timing margin
//! does nothing for a data race on shared state.
//!
//! That 2026-08-07 fix was later found to be incomplete: `VCI_CONFIG_STARTUP_LOCK`
//! only serializes concurrent *threads* within a single test process -- it does
//! nothing against two separate `cargo test` processes racing on the same fixed
//! config file path. See `docs/implementation-notes.md`'s "Resolved (2026-08-27)"
//! entry and ADR-195 for the cross-process fix (a per-process-unique config
//! path), applied in [`TestServer::try_start_with_extra_config`] below.

use std::os::raw::{c_long, c_void};
use std::path::Path;

use j2534_0404_service::service::J2534Service;
use j2534_0404_sys::bindings::{
    IOCTL_QUERY_REPEAT_MESSAGE, IOCTL_STOP_REPEAT_MESSAGE, IOCTL_TEARDOWN_CONNECTION, SBYTE_ARRAY,
    STATUS_NOERROR,
};
use j2534_0404_sys::libloading::{Library, Symbol};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use vci_service_interface::{
    CllCreateFlag, CllCreateFlagBit, ComLogicalLinkHandle, ComPrimitiveCtrlData,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, EcuUniqueRespData,
    EventNotification, ExpectedResponseData, GetEventItemRequest, GetUniqueRespIdTableRequest,
    ModuleHandle, ParamItem, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    StartComPrimitiveRequest, TxFlag, TxFlagBit, UniqueRespIdTableItem, com_primitive_ctrl_data,
    create_com_logical_link_request, event_item, event_notification, get_event_item_request,
    param_item, resource_data, vci_service_client::VciServiceClient,
    vci_service_server::VciServiceServer,
};
use vci_service_launcher::vci_server::VciServer;

/// Measured ceiling for one client-observable gRPC/event round trip in this
/// harness under load (environment-measured, most recently ~85ms — see
/// ADR-149). New timing-window tests should budget this **once per
/// round trip inside the window being tested**, not as a flat total; see
/// the module doc above for the full guideline.
// Not yet referenced by any Rust code, only by doc comments across this
// file's siblings (ADR-149) -- allow(dead_code) so `-D warnings` clippy runs
// don't fail on a documented reference constant.
#[allow(dead_code)]
pub(crate) const GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS: u64 = 85;

pub(crate) const MOCK_MODULE_HANDLE: u32 = 1;

/// The physical J2534 channel id the mock assigns to the first
/// `PassThruConnect` after `__mock_reset()`. Each test creates exactly one
/// logical link (and therefore exactly one physical channel), so this is
/// deterministic.
pub(crate) const MOCK_CHANNEL_ID: u32 = 1;

pub(crate) const NC: c_long = STATUS_NOERROR as c_long;

/// The fixed `PASSTHRU_MSG.Timestamp` value `j2534-0404-mock` stamps on
/// every RX frame it produces (its own `MOCK_TIMESTAMP` constant), whether
/// injected via `inject_rx`/`inject_rx_with_status` or synthesized
/// internally (e.g. the fast-init response). Used by tests asserting
/// `ResultData.tx_msg_done_timestamp`/`start_msg_timestamp` (ADR-143).
pub(crate) const MOCK_TIMESTAMP: u32 = 4242;

/// Dynamically loads the mock library and calls its `__mock_*` back-door FFI
/// exports directly, bypassing the `j2534-0404-mock` rlib's own (separate)
/// copy of the process-global mock state. See the module doc comment.
pub(crate) struct MockBackdoor {
    lib: Library,
}

impl MockBackdoor {
    pub(crate) fn open(path: &Path) -> Self {
        let lib = unsafe { Library::new(path) }.expect("mock library should be loadable");
        Self { lib }
    }

    pub(crate) fn reset(&self) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> c_long> = self
                .lib
                .get(b"__mock_reset\0")
                .expect("__mock_reset should be exported");
            f();
        }
    }

    pub(crate) fn config_value(&self, channel_id: u32, param_id: u32) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_config_value\0")
                .expect("__mock_get_config_value should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, param_id, &mut value);
            assert_eq!(
                rc, NC,
                "__mock_get_config_value should succeed for a connected channel"
            );
            value
        }
    }

    /// The `Parameter` id of every `IOCTL_SET_CONFIG` entry successfully
    /// applied on `channel_id`, in call order (ADR-158/Phase 3a) -- lets a
    /// test assert cross-call ordering (e.g.
    /// `CONFIG_FD_CAN_DATA_PHASE_RATE` before `CONFIG_J1962_PINS`, clause
    /// 21.3.2.5.1) that `config_value` alone (final value only) cannot.
    pub(crate) fn set_config_param_log(&self, channel_id: u32) -> Vec<u32> {
        unsafe {
            let count_fn: Symbol<unsafe extern "system" fn(u32) -> usize> = self
                .lib
                .get(b"__mock_get_set_config_param_log_count\0")
                .expect("__mock_get_set_config_param_log_count should be exported");
            let entry_fn: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_set_config_param_log_entry\0")
                .expect("__mock_get_set_config_param_log_entry should be exported");
            let count = count_fn(channel_id);
            (0..count)
                .map(|index| {
                    let mut value = 0u32;
                    let rc = entry_fn(channel_id, index, &mut value);
                    assert_eq!(
                        rc, NC,
                        "__mock_get_set_config_param_log_entry should succeed within count"
                    );
                    value
                })
                .collect()
        }
    }

    pub(crate) fn baud_rate(&self, channel_id: u32) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_channel_baud_rate\0")
                .expect("__mock_get_channel_baud_rate should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, &mut value);
            assert_eq!(
                rc, NC,
                "__mock_get_channel_baud_rate should succeed for a connected channel"
            );
            value
        }
    }

    /// The J2534 `ProtocolID` `channel_id` was opened with via
    /// `PassThruConnect` (ADR-158/Phase 3a) -- e.g. `PROTOCOL_FD_CAN_PS` vs.
    /// plain `PROTOCOL_CAN`.
    pub(crate) fn channel_protocol_id(&self, channel_id: u32) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_channel_protocol_id\0")
                .expect("__mock_get_channel_protocol_id should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, &mut value);
            assert_eq!(
                rc, NC,
                "__mock_get_channel_protocol_id should succeed for a connected channel"
            );
            value
        }
    }

    pub(crate) fn written_count(&self, channel_id: u32) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> usize> = self
                .lib
                .get(b"__mock_get_written_msg_count\0")
                .expect("__mock_get_written_msg_count should be exported");
            f(channel_id)
        }
    }

    /// The real gap between written messages `from_index` and `to_index` on
    /// `channel_id`, as stamped by the mock when it stored each one. Measure
    /// transmit spacing with this rather than by timing `wait_for_written_count`
    /// calls: polling adds up to one poll interval plus the OS timer resolution
    /// (about 15.6 ms on Windows) to each end of the measurement.
    pub(crate) fn written_gap(
        &self,
        channel_id: u32,
        from_index: usize,
        to_index: usize,
    ) -> std::time::Duration {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, usize, *mut i64) -> c_long> = self
                .lib
                .get(b"__mock_get_written_msg_gap_us\0")
                .expect("__mock_get_written_msg_gap_us should be exported");
            let mut gap_us = 0i64;
            let rc = f(channel_id, from_index, to_index, &mut gap_us);
            assert_eq!(
                rc, NC,
                "written messages {from_index} and {to_index} should exist on channel {channel_id}"
            );
            assert!(
                gap_us >= 0,
                "written message {to_index} should have been written after {from_index}"
            );
            std::time::Duration::from_micros(gap_us as u64)
        }
    }

    pub(crate) fn written_data(&self, channel_id: u32, index: usize) -> Vec<u8> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u8, u32, *mut u32) -> c_long> =
                self.lib
                    .get(b"__mock_get_written_msg\0")
                    .expect("__mock_get_written_msg should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(
                channel_id,
                index,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            );
            assert_eq!(
                rc, NC,
                "written message {index} should exist on channel {channel_id}"
            );
            buf[..out_len as usize].to_vec()
        }
    }

    pub(crate) fn written_protocol_id(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_written_msg_protocol_id\0")
                .expect("__mock_get_written_msg_protocol_id should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(
                rc, NC,
                "written message {index} should exist on channel {channel_id}"
            );
            value
        }
    }

    pub(crate) fn written_tx_flags(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_written_msg_tx_flags\0")
                .expect("__mock_get_written_msg_tx_flags should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(
                rc, NC,
                "written message {index} should exist on channel {channel_id}"
            );
            value
        }
    }

    /// Read a still-live repeat slot's mask/pattern `PassThruMessage`s' raw,
    /// UNMASKED native `TxFlags` (`RepeatSlot::mask_pattern_tx_flags` in
    /// `j2534-0404-mock/src/lib.rs`) -- test-introspection only, distinct
    /// from the derived/masked value the mock itself uses for RX-frame
    /// comparison. Codex review, ADR-165 PR #42 round 15.
    pub(crate) fn repeat_slot_mask_pattern_tx_flags(&self, channel_id: u32, msg_id: u32) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_repeat_slot_mask_pattern_tx_flags\0")
                .expect("__mock_get_repeat_slot_mask_pattern_tx_flags should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, msg_id, &mut value);
            assert_eq!(
                rc, NC,
                "repeat slot {msg_id} should exist on channel {channel_id}"
            );
            value
        }
    }

    /// Input frame (`PASSTHRU_MSG.Data[..DataSize]`) of the most recent
    /// `IOCTL_FAST_INIT` call on `channel_id`, or `None` if no such call
    /// with a non-null input has been made (ADR-075).
    pub(crate) fn fast_init_input(&self, channel_id: u32) -> Option<Vec<u8>> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *mut u8, u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_fast_init_input\0")
                .expect("__mock_get_fast_init_input should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(channel_id, buf.as_mut_ptr(), buf.len() as u32, &mut out_len);
            if rc != NC {
                return None;
            }
            Some(buf[..out_len as usize].to_vec())
        }
    }

    /// Target address byte of the most recent `IOCTL_FIVE_BAUD_INIT` call on
    /// `channel_id`, or `None` if no such call with a non-null input has been
    /// made (ADR-076).
    pub(crate) fn five_baud_init_input(&self, channel_id: u32) -> Option<u8> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *mut u8) -> c_long> = self
                .lib
                .get(b"__mock_get_five_baud_init_input\0")
                .expect("__mock_get_five_baud_init_input should be exported");
            let mut address = 0u8;
            let rc = f(channel_id, &mut address);
            if rc != NC {
                return None;
            }
            Some(address)
        }
    }

    pub(crate) fn filter_count(&self, channel_id: u32) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> usize> = self
                .lib
                .get(b"__mock_get_filter_count\0")
                .expect("__mock_get_filter_count should be exported");
            f(channel_id)
        }
    }

    pub(crate) fn filter_type(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_filter_type\0")
                .expect("__mock_get_filter_type should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            value
        }
    }

    pub(crate) fn filter_mask(&self, channel_id: u32, index: usize) -> Vec<u8> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u8, u32, *mut u32) -> c_long> =
                self.lib
                    .get(b"__mock_get_filter_mask\0")
                    .expect("__mock_get_filter_mask should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(
                channel_id,
                index,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            );
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            buf[..out_len as usize].to_vec()
        }
    }

    pub(crate) fn filter_pattern(&self, channel_id: u32, index: usize) -> Vec<u8> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u8, u32, *mut u32) -> c_long> =
                self.lib
                    .get(b"__mock_get_filter_pattern\0")
                    .expect("__mock_get_filter_pattern should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(
                channel_id,
                index,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            );
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            buf[..out_len as usize].to_vec()
        }
    }

    /// `ProtocolID` of filter `index`'s `pPatternMsg` on `channel_id` (ADR-157
    /// Plane A, Fix 1, Codex review PR #28) -- `pMaskMsg`/`pFlowControlMsg`
    /// are built from the same `protocol_id` in the same call
    /// (`can_filter_message`), so this is representative of all three.
    pub(crate) fn filter_pattern_protocol_id(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_filter_pattern_protocol_id\0")
                .expect("__mock_get_filter_pattern_protocol_id should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            value
        }
    }

    pub(crate) fn filter_flow_control(&self, channel_id: u32, index: usize) -> Option<Vec<u8>> {
        unsafe {
            let has_fc: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_filter_has_flow_control\0")
                .expect("__mock_get_filter_has_flow_control should be exported");
            let mut has = 0u32;
            let rc = has_fc(channel_id, index, &mut has);
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            if has == 0 {
                return None;
            }
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u8, u32, *mut u32) -> c_long> =
                self.lib
                    .get(b"__mock_get_filter_flow_control\0")
                    .expect("__mock_get_filter_flow_control should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(
                channel_id,
                index,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            );
            assert_eq!(rc, NC, "filter {index} should have a flow-control message");
            Some(buf[..out_len as usize].to_vec())
        }
    }

    pub(crate) fn filter_pattern_tx_flags(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_filter_pattern_tx_flags\0")
                .expect("__mock_get_filter_pattern_tx_flags should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(
                rc, NC,
                "filter {index} should exist on channel {channel_id}"
            );
            value
        }
    }

    pub(crate) fn filter_flow_control_tx_flags(&self, channel_id: u32, index: usize) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_filter_flow_control_tx_flags\0")
                .expect("__mock_get_filter_flow_control_tx_flags should be exported");
            let mut value = 0u32;
            let rc = f(channel_id, index, &mut value);
            assert_eq!(rc, NC, "filter {index} should have a flow-control message");
            value
        }
    }

    /// Cumulative count of `PassThruOpen` calls since the last `reset()`.
    pub(crate) fn open_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_open_count\0")
                .expect("__mock_get_open_count should be exported");
            f()
        }
    }

    /// The `pName` argument of the most recent `PassThruOpen` call: `None` if
    /// it was NULL (or `PassThruOpen` has never been called since the last
    /// `reset()`), `Some(bytes)` otherwise (ADR-106).
    pub(crate) fn open_pname(&self) -> Option<Vec<u8>> {
        unsafe {
            let is_set_fn: Symbol<unsafe extern "system" fn() -> u32> = self
                .lib
                .get(b"__mock_open_pname_is_set\0")
                .expect("__mock_open_pname_is_set should be exported");
            if is_set_fn() == 0 {
                return None;
            }
            let f: Symbol<unsafe extern "system" fn(*mut u8, u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_open_pname\0")
                .expect("__mock_get_open_pname should be exported");
            let mut buf = [0u8; 256];
            let mut out_len = 0u32;
            let rc = f(buf.as_mut_ptr(), buf.len() as u32, &mut out_len);
            assert_eq!(rc, NC, "__mock_get_open_pname should succeed when set");
            Some(buf[..out_len as usize].to_vec())
        }
    }

    pub(crate) fn connect_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_connect_count\0")
                .expect("__mock_get_connect_count should be exported");
            f()
        }
    }

    /// Cumulative count of successful `PassThruDisconnect` calls across
    /// every channel since the last `reset()` -- lets a test confirm a
    /// failed connect-time `SET_CONFIG` (e.g. ADR-160's `CAN_MIXED_FORMAT`
    /// rollback) actually disconnected the just-opened channel rather than
    /// leaking it.
    pub(crate) fn disconnect_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_disconnect_count\0")
                .expect("__mock_get_disconnect_count should be exported");
            f()
        }
    }

    /// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase
    /// 2b): overrides the `_CHx` capacity the mock reports via
    /// `DEVICE_INFO_<PROTOCOL>_SUPPORTED` (and enforces at `PassThruConnect`)
    /// for every in-scope protocol family, replacing `DEFAULT_CHX_CAPACITY`
    /// -- lets a test drive `j2534-0404-service`'s `check_chx_capacity`
    /// precheck against a known, deliberately small count instead of relying
    /// on the mock's own default.
    pub(crate) fn set_chx_capacity(&self, count: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_chx_capacity\0")
                .expect("__mock_set_chx_capacity should be exported");
            let rc = f(count);
            assert_eq!(rc, NC, "__mock_set_chx_capacity should succeed");
        }
    }

    /// ADR-211 (Codex review round-2 correction, PR #124): overrides ONLY
    /// `DEVICE_INFO_FT_CAN_SUPPORTED`/`DEVICE_INFO_FT_ISO15765_SUPPORTED`'s
    /// own packed `_CHx` count, independent of [`Self::set_chx_capacity`]
    /// above -- lets a test diverge Fault-Tolerant CAN's own Discovery-
    /// reported capacity from the generic families', which is the only way
    /// to prove `check_chx_capacity` actually consults the FT-specific flag
    /// rather than `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED`
    /// through the real gRPC/mock stack.
    pub(crate) fn set_ft_can_chx_capacity(&self, count: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_ft_can_chx_capacity\0")
                .expect("__mock_set_ft_can_chx_capacity should be exported");
            let rc = f(count);
            assert_eq!(rc, NC, "__mock_set_ft_can_chx_capacity should succeed");
        }
    }

    /// ADR-212/Round 2: the Single Wire CAN analog of
    /// [`Self::set_ft_can_chx_capacity`] above -- overrides ONLY
    /// `DEVICE_INFO_SW_CAN_SUPPORTED`/`DEVICE_INFO_SW_ISO15765_SUPPORTED`'s
    /// own packed `_CHx` count, independent of [`Self::set_chx_capacity`] --
    /// lets a test diverge Single Wire CAN's own Discovery-reported capacity
    /// from the generic families', which is the only way to prove
    /// `check_chx_capacity` actually consults the SW-CAN-specific flag rather
    /// than `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED`
    /// through the real gRPC/mock stack.
    pub(crate) fn set_sw_can_chx_capacity(&self, count: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_sw_can_chx_capacity\0")
                .expect("__mock_set_sw_can_chx_capacity should be exported");
            let rc = f(count);
            assert_eq!(rc, NC, "__mock_set_sw_can_chx_capacity should succeed");
        }
    }

    /// ADR-213/Round 3: the CAN FD analog of [`Self::set_ft_can_chx_capacity`]/
    /// [`Self::set_sw_can_chx_capacity`] above -- overrides ONLY
    /// `DEVICE_INFO_FD_CAN_SUPPORTED`'s own packed `_CHx` count, independent
    /// of [`Self::set_chx_capacity`] -- lets a test diverge CAN FD's own
    /// Discovery-reported capacity from the generic families', which is the
    /// only way to prove `check_chx_capacity` actually consults the
    /// CAN-FD-specific flag rather than `DEVICE_INFO_CAN_SUPPORTED` through
    /// the real gRPC/mock stack.
    pub(crate) fn set_fd_can_chx_capacity(&self, count: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_fd_can_chx_capacity\0")
                .expect("__mock_set_fd_can_chx_capacity should be exported");
            let rc = f(count);
            assert_eq!(rc, NC, "__mock_set_fd_can_chx_capacity should succeed");
        }
    }

    /// ADR-213/Round 3: the ISO15765-on-CAN-FD analog of
    /// [`Self::set_fd_can_chx_capacity`] just above -- overrides ONLY
    /// `DEVICE_INFO_FD_ISO15765_SUPPORTED`'s own packed `_CHx` count.
    pub(crate) fn set_fd_iso15765_chx_capacity(&self, count: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_fd_iso15765_chx_capacity\0")
                .expect("__mock_set_fd_iso15765_chx_capacity should be exported");
            let rc = f(count);
            assert_eq!(rc, NC, "__mock_set_fd_iso15765_chx_capacity should succeed");
        }
    }

    /// The `Flags` argument of every successful `PassThruConnect`, in call order.
    pub(crate) fn connect_flags_log(&self) -> Vec<u32> {
        unsafe {
            let len_fn: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_connect_flags_log_len\0")
                .expect("__mock_get_connect_flags_log_len should be exported");
            let entry_fn: Symbol<unsafe extern "system" fn(usize, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_connect_flags_log_entry\0")
                .expect("__mock_get_connect_flags_log_entry should be exported");
            let mut log = Vec::with_capacity(len_fn());
            for index in 0..len_fn() {
                let mut value = 0u32;
                let rc = entry_fn(index, &mut value);
                assert_eq!(rc, NC, "connect_flags_log entry {index} should exist");
                log.push(value);
            }
            log
        }
    }

    /// Total number of `PassThruIoctl SET_CONFIG` calls across every channel
    /// since the last `reset()`.
    pub(crate) fn set_config_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_set_config_count\0")
                .expect("__mock_get_set_config_count should be exported");
            f()
        }
    }

    /// Total number of `PassThruStartPeriodicMsg` calls across every channel
    /// since the last `reset()`.
    pub(crate) fn start_periodic_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_start_periodic_count\0")
                .expect("__mock_get_start_periodic_count should be exported");
            f()
        }
    }

    /// Total number of `PassThruStopPeriodicMsg` calls across every channel
    /// since the last `reset()`.
    pub(crate) fn stop_periodic_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_stop_periodic_count\0")
                .expect("__mock_get_stop_periodic_count should be exported");
            f()
        }
    }

    /// Number of periodic messages currently live (started, not yet
    /// stopped/cleared) on `channel_id` -- ADR-192/Phase 7 Stage 7c: the
    /// mock's own device-side state, distinct from this service's internal
    /// `LogicalLinkState::tp20_broadcast_periodic` tracking.
    pub(crate) fn periodic_msg_count(&self, channel_id: u32) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> usize> = self
                .lib
                .get(b"__mock_get_periodic_msg_count\0")
                .expect("__mock_get_periodic_msg_count should be exported");
            f(channel_id)
        }
    }

    /// Cumulative count of `PassThruStartMsgFilter` calls across every
    /// channel since the last `reset()`. Used to distinguish "no filter I/O
    /// happened" from "an identical filter was stopped and reinstalled"
    /// (ADR-068's promotion diff gate).
    pub(crate) fn start_filter_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_start_filter_count\0")
                .expect("__mock_get_start_filter_count should be exported");
            f()
        }
    }

    /// Cumulative count of successful `IOCTL_FIVE_BAUD_INIT` calls across
    /// every channel since the last `reset()` (ADR-074: `CP_InitializationSettings`-
    /// driven init sequence selection).
    pub(crate) fn five_baud_init_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_five_baud_init_count\0")
                .expect("__mock_get_five_baud_init_count should be exported");
            f()
        }
    }

    /// Cumulative count of successful `IOCTL_FAST_INIT` calls across every
    /// channel since the last `reset()` (ADR-074).
    pub(crate) fn fast_init_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_fast_init_count\0")
                .expect("__mock_get_fast_init_count should be exported");
            f()
        }
    }

    /// Cumulative count of `PassThruStopMsgFilter` calls across every
    /// channel since the last `reset()`. See `start_filter_count`.
    pub(crate) fn stop_filter_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_stop_filter_count\0")
                .expect("__mock_get_stop_filter_count should be exported");
            f()
        }
    }

    /// Cumulative count of `PassThruIoctl(CLEAR_RX_BUFFER)` calls across
    /// every channel since the last `reset()`. Used to confirm
    /// `PDU_IOCTL_RESET`'s hardware teardown clears a shared physical
    /// channel's buffers exactly once per channel (ADR-161 Phase 1).
    pub(crate) fn clear_rx_buffer_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_clear_rx_buffer_count\0")
                .expect("__mock_get_clear_rx_buffer_count should be exported");
            f()
        }
    }

    /// Cumulative count of `PassThruIoctl(CLEAR_TX_BUFFER)` calls across
    /// every channel since the last `reset()`. See `clear_rx_buffer_count`.
    pub(crate) fn clear_tx_buffer_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_clear_tx_buffer_count\0")
                .expect("__mock_get_clear_tx_buffer_count should be exported");
            f()
        }
    }

    /// Cumulative count of successful (right-protocol) `PassThruIoctl(SW_CAN_HS)`
    /// calls across every channel since the last `reset()` (ADR-164/Phase
    /// 4). Used to confirm `PDU_IOCTL_SW_CAN_HS`'s `SharedChannel::ref_count
    /// == 1` gate actually skips the native call on a shared physical
    /// channel.
    pub(crate) fn sw_can_hs_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_sw_can_hs_count\0")
                .expect("__mock_get_sw_can_hs_count should be exported");
            f()
        }
    }

    /// Cumulative count of successful (right-protocol) `PassThruIoctl(SW_CAN_NS)`
    /// calls across every channel since the last `reset()` (ADR-164/Phase
    /// 4). See `sw_can_hs_count`.
    pub(crate) fn sw_can_ns_count(&self) -> usize {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> usize> = self
                .lib
                .get(b"__mock_get_sw_can_ns_count\0")
                .expect("__mock_get_sw_can_ns_count should be exported");
            f()
        }
    }

    /// Whether SAE J2534-2 clause 14's `msg_id`-identified repeat-message
    /// slot still EXISTS on `channel_id` (ADR-165/Phase 12) -- issued as a
    /// raw `PassThruIoctl(QUERY_REPEAT_MESSAGE)` call directly against the
    /// mock cdylib (`PassThruIoctl` is itself one of the native entry points
    /// this module's `Library` already resolves symbols from, so no
    /// dedicated `__mock_*` export is needed), through the dynamically-
    /// loaded copy of the mock's state this file's module doc comment
    /// describes, rather than an in-process call. Lets a test confirm slot
    /// teardown once the owning CLL -- and therefore any gRPC handle to
    /// query it through the service -- is already gone.
    ///
    /// **ADR-173 note:** "exists" is NOT the same as "live" -- a
    /// self-terminated-but-unstopped slot (QUERY status `0`) still exists
    /// here and this returns `true` for it, exactly as it does for a live
    /// (status `1`) slot; only an explicit `IOCTL_STOP_REPEAT_MESSAGE` (or
    /// the owning channel/CLL going away) makes this return `false`. Use
    /// [`Self::repeat_message_status`] when a test needs to distinguish
    /// live from terminated-but-unstopped.
    ///
    /// Returns `true` only on `STATUS_NOERROR` (the slot is still present).
    /// Deliberately does not special-case `ERR_INVALID_MSG_ID` (the slot was
    /// explicitly stopped, channel still open) vs. any other failure (e.g.
    /// `ERR_INVALID_CHANNEL_ID` once a sole-owner CLL's teardown has also
    /// torn down the physical channel itself, since `PDU_IOCTL_STOP_REPEAT_
    /// MESSAGE` best-effort-runs before `PassThruDisconnect`, not after) --
    /// every non-success code here means "this MsgId cannot be queried
    /// anymore", which is exactly what "gone" means for a caller of this
    /// method.
    pub(crate) fn repeat_message_exists(&self, channel_id: u32, msg_id: u32) -> bool {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> c_long> =
                self.lib
                    .get(b"PassThruIoctl\0")
                    .expect("PassThruIoctl should be exported");
            let mut query_msg_id = msg_id;
            let mut status: u32 = 0;
            let rc = f(
                channel_id,
                IOCTL_QUERY_REPEAT_MESSAGE,
                &mut query_msg_id as *mut u32 as *mut c_void,
                &mut status as *mut u32 as *mut c_void,
            );
            rc == NC
        }
    }

    /// The mock's raw `IOCTL_QUERY_REPEAT_MESSAGE` status for `msg_id` on
    /// `channel_id`, bypassing this service's own per-CLL `MsgId` ownership
    /// tracking exactly like [`Self::repeat_message_exists`] does. ADR-173
    /// Decision 3's Table 53 polarity: `Some(1)` for a still-live slot,
    /// `Some(0)` for a terminated-but-unstopped one (`MsgId` still valid),
    /// `None` on any failure (including `ERR_INVALID_MSG_ID` once genuinely
    /// removed by an explicit STOP).
    pub(crate) fn repeat_message_status(&self, channel_id: u32, msg_id: u32) -> Option<u32> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> c_long> =
                self.lib
                    .get(b"PassThruIoctl\0")
                    .expect("PassThruIoctl should be exported");
            let mut query_msg_id = msg_id;
            let mut status: u32 = 0;
            let rc = f(
                channel_id,
                IOCTL_QUERY_REPEAT_MESSAGE,
                &mut query_msg_id as *mut u32 as *mut c_void,
                &mut status as *mut u32 as *mut c_void,
            );
            if rc == NC { Some(status) } else { None }
        }
    }

    /// Issues a raw `PassThruIoctl(STOP_REPEAT_MESSAGE)` directly against the
    /// mock, bypassing this service's own per-CLL `MsgId` ownership/tracking
    /// entirely (unlike the gRPC `PDU_IOCTL_STOP_REPEAT_MESSAGE` surface,
    /// which validates the caller-supplied `MsgId` against
    /// `LogicalLinkState::repeat_message_ids` first). Simulates "the device
    /// genuinely forgot this `MsgId` for a reason this service was never
    /// told about" -- under ADR-173, the mock itself never spontaneously
    /// forgets a slot (self-termination is retained, not removed, until an
    /// explicit STOP), so this is the only way a test can manufacture the
    /// "this service's own tracking is now stale" scenario `prune_stale_
    /// repeat_message_ids`/the leaked-slot machinery exists to handle
    /// -- mirroring how a real, non-thin-forwarded vendor DLL might
    /// independently drop a slot. Returns `true` on `STATUS_NOERROR`.
    pub(crate) fn stop_repeat_message_directly(&self, channel_id: u32, msg_id: u32) -> bool {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> c_long> =
                self.lib
                    .get(b"PassThruIoctl\0")
                    .expect("PassThruIoctl should be exported");
            let mut stop_msg_id = msg_id;
            let rc = f(
                channel_id,
                IOCTL_STOP_REPEAT_MESSAGE,
                &mut stop_msg_id as *mut u32 as *mut c_void,
                std::ptr::null_mut(),
            );
            rc == NC
        }
    }

    /// Issues a raw `PassThruIoctl(TEARDOWN_CONNECTION)` directly against the
    /// mock, bypassing this service's own TP2.0 connection-lifecycle
    /// tracking entirely (mirrors `stop_repeat_message_directly`'s own
    /// "raw IOCTL, service never told" shape, for the TP2.0 quarantine/
    /// reconciliation mechanism instead of Repeat Messaging). Frees the
    /// mock's OWN internal `rx_id` slot and, unless
    /// `__mock_set_tp20_no_indication` is armed, queues the mock's own
    /// `CONNECTION_LOST` indication for it -- simulating a device
    /// SPONTANEOUSLY losing an established connection for a reason this
    /// service was never told about (no local `CoptStopcomm`/`Disconnect`/
    /// `Destroy` ever ran), the scenario `reconcile_established_tp20_loss`/
    /// `run_tp20_connection_request`'s own registration-time reconcile
    /// exist to handle (Codex review finding via `edge-case-hunter`,
    /// design-advisor consult, P1, PR #97, round 24). `rx_id` is packed as
    /// the native 4-byte big-endian `SBYTE_ARRAY` input clause 19.3.3.3's
    /// own teardown request uses (mirrors `j2534_0404::J2534Api0404::
    /// tp20_teardown_connection`'s identical wire shape). Returns `true` on
    /// `STATUS_NOERROR`.
    pub(crate) fn tp20_teardown_connection_directly(&self, channel_id: u32, rx_id: u32) -> bool {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> c_long> =
                self.lib
                    .get(b"PassThruIoctl\0")
                    .expect("PassThruIoctl should be exported");
            let mut bytes = rx_id.to_be_bytes().to_vec();
            let mut input = SBYTE_ARRAY {
                NumOfBytes: bytes.len() as u32,
                BytePtr: bytes.as_mut_ptr(),
            };
            let rc = f(
                channel_id,
                IOCTL_TEARDOWN_CONNECTION,
                std::ptr::addr_of_mut!(input).cast(),
                std::ptr::null_mut(),
            );
            rc == NC
        }
    }

    /// Injects a frame into the RX queue of `channel_id`, as if the vehicle
    /// had transmitted it.
    pub(crate) fn inject_rx(&self, channel_id: u32, data: &[u8], protocol_id: u32) {
        self.inject_rx_with_status(channel_id, data, protocol_id, 0);
    }

    /// Like `inject_rx`, but with an explicit `PASSTHRU_MSG.RxStatus` value
    /// (e.g. `0x00000002` for `START_OF_MESSAGE`, ADR-097, or
    /// `j2534_0404::CAN_29BIT_ID_STATUS`/`ISO15765_ADDR_TYPE_STATUS` to
    /// exercise a `Condition == 1` SAE J2534-2 clause 14 repeat slot's
    /// response-format check, ADR-165 PR #42 round 9, corrected in round 14
    /// to compare `RxStatus` rather than `TxFlags` -- see
    /// `RepeatSlot::response_format_rx_bits`'s doc comment in
    /// `j2534-0404-mock/src/lib.rs`) instead of the always-`0` default.
    /// Resolves the original five-argument `__mock_inject_rx_msg` symbol
    /// directly.
    pub(crate) fn inject_rx_with_status(
        &self,
        channel_id: u32,
        data: &[u8],
        protocol_id: u32,
        rx_status: u32,
    ) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *const u8, u32, u32, u32) -> c_long> =
                self.lib
                    .get(b"__mock_inject_rx_msg\0")
                    .expect("__mock_inject_rx_msg should be exported");
            let rc = f(
                channel_id,
                data.as_ptr(),
                data.len() as u32,
                protocol_id,
                rx_status,
            );
            assert_eq!(
                rc, NC,
                "__mock_inject_rx_msg should succeed for a connected channel"
            );
        }
    }

    /// Like `inject_rx_with_status`, but with an explicit native
    /// `ExtraDataIndex` (ADR-171) instead of the mock's default of
    /// `DataSize` (i.e. "no trailing IFR bytes"). Resolves the six-argument
    /// `__mock_inject_rx_msg_with_edi` symbol -- a separate export from
    /// `__mock_inject_rx_msg` rather than a widened signature, per that
    /// export's own doc comment.
    pub(crate) fn inject_rx_with_edi(
        &self,
        channel_id: u32,
        data: &[u8],
        protocol_id: u32,
        rx_status: u32,
        extra_data_index: u32,
    ) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *const u8, u32, u32, u32, u32) -> c_long> =
                self.lib
                    .get(b"__mock_inject_rx_msg_with_edi\0")
                    .expect("__mock_inject_rx_msg_with_edi should be exported");
            let rc = f(
                channel_id,
                data.as_ptr(),
                data.len() as u32,
                protocol_id,
                rx_status,
                extra_data_index,
            );
            assert_eq!(
                rc, NC,
                "__mock_inject_rx_msg_with_edi should succeed for a connected channel"
            );
        }
    }

    /// Arms a ONE-SHOT RX injection on `channel_id`: on the NEXT
    /// `PassThruWriteMsgs` call for this channel, `data` is pushed into the
    /// RX queue and the mock write call then blocks for `hold_ms`
    /// (simulating slow hardware write I/O) before returning -- letting
    /// tests land an RX frame (e.g. a FlowControl) deterministically DURING
    /// a specific write, rather than merely before or after it (ADR-095
    /// amendment).
    pub(crate) fn arm_write_rx_injection(
        &self,
        channel_id: u32,
        data: &[u8],
        protocol_id: u32,
        hold_ms: u32,
    ) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *const u8, u32, u32, u32) -> c_long> =
                self.lib
                    .get(b"__mock_arm_write_rx_injection\0")
                    .expect("__mock_arm_write_rx_injection should be exported");
            let rc = f(
                channel_id,
                data.as_ptr(),
                data.len() as u32,
                protocol_id,
                hold_ms,
            );
            assert_eq!(
                rc, NC,
                "__mock_arm_write_rx_injection should succeed for a connected channel"
            );
        }
    }

    /// Caps the number of simultaneously open channels; `PassThruConnect`
    /// returns `ERR_EXCEEDED_LIMIT` past the limit. `0` removes the cap.
    pub(crate) fn set_max_channels(&self, limit: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_max_channels\0")
                .expect("__mock_set_max_channels should be exported");
            assert_eq!(f(limit), NC, "__mock_set_max_channels should succeed");
        }
    }

    /// SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
    /// (ADR-160/Phase 3c): `true` forces every subsequent
    /// `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` call, on any channel, to fail
    /// `ERR_NOT_SUPPORTED` -- simulating a device that doesn't implement
    /// clause 8 at all. `false` clears the override (the default after
    /// `reset()`).
    pub(crate) fn set_can_mixed_format_unsupported(&self, unsupported: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_can_mixed_format_unsupported\0")
                .expect("__mock_set_can_mixed_format_unsupported should be exported");
            assert_eq!(
                f(unsupported as u32),
                NC,
                "__mock_set_can_mixed_format_unsupported should succeed"
            );
        }
    }

    /// Simulates the SAE J1850 bus flavor (ADR-070) for the VPW/PWM
    /// auto-detect probe: `Some(j2534_0404::J1850VPW)` / `Some(J1850PWM)`
    /// makes the bus "answer" only a connect opened with that exact protocol
    /// id (a canned response is queued on the channel immediately); `None`
    /// simulates a silent bus (the default after `reset()`).
    pub(crate) fn set_j1850_bus_flavor(&self, flavor: Option<u32>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_j1850_bus_flavor\0")
                .expect("__mock_set_j1850_bus_flavor should be exported");
            let rc = f(flavor.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_j1850_bus_flavor should succeed");
        }
    }

    /// Pin-gated variant of `set_j1850_bus_flavor` (ADR-156/157 Bug B
    /// regression coverage): the bus only "answers" once a channel opened
    /// with `flavor`'s `_PS` variant has `CONFIG_J1962_PINS` applied with
    /// exactly `pin_select` -- unlike `set_j1850_bus_flavor`, which answers
    /// unconditionally at connect time regardless of pin state. Lets tests
    /// prove a probe candidate actually binds the caller's selected pins
    /// before reading, not just that it connects.
    pub(crate) fn set_j1850_bus_flavor_requiring_pins(&self, flavor: u32, pin_select: u32) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32) -> c_long> = self
                .lib
                .get(b"__mock_set_j1850_bus_flavor_requiring_pins\0")
                .expect("__mock_set_j1850_bus_flavor_requiring_pins should be exported");
            let rc = f(flavor, pin_select);
            assert_eq!(
                rc, NC,
                "__mock_set_j1850_bus_flavor_requiring_pins should succeed"
            );
        }
    }

    /// Forces every subsequent `IOCTL_FAST_INIT` call, on every channel, to
    /// fail with `code` instead of its normal success behavior -- lets tests
    /// exercise `CoptStartcomm`'s init-failure path (ADR-077) without a real
    /// adapter. `Some(code)` installs the override; `None` clears it (the
    /// default after `reset()`), mirroring `set_j1850_bus_flavor`'s `Option`
    /// convention.
    pub(crate) fn set_fast_init_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_fast_init_error\0")
                .expect("__mock_set_fast_init_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_fast_init_error should succeed");
        }
    }

    /// Forces every subsequent `PassThruStopMsgFilter` call, on every
    /// channel, to fail with `code` instead of its normal success behavior --
    /// lets tests exercise `PDU_IOCTL_STOP_MSG_FILTER`/
    /// `PDU_IOCTL_CLEAR_MSG_FILTER`'s `PDU_ERR_FCT_FAILED` reporting path
    /// (ADR-114) without a real adapter. `Some(code)` installs the override;
    /// `None` clears it (the default after `reset()`), mirroring
    /// `set_fast_init_error`'s `Option` convention.
    pub(crate) fn set_stop_filter_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_stop_filter_error\0")
                .expect("__mock_set_stop_filter_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_stop_filter_error should succeed");
        }
    }

    /// Forces every subsequent `PassThruWriteMsgs` call, on every channel, to
    /// fail with `code` instead of its normal success behavior -- lets tests
    /// exercise the RC21/RC23 (NRC 0x21/0x23) auto-re-request path's own
    /// retransmit failure (`TxFailure::Event(PduErrEvtTxError)` ->
    /// `ReceivePhaseOutcome::ReRequestTxFailed`, ADR-087) without a real
    /// adapter. `Some(code)` installs the override; `None` clears it (the
    /// default after `reset()`), mirroring `set_stop_filter_error`'s `Option`
    /// convention.
    pub(crate) fn set_write_msgs_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_write_msgs_error\0")
                .expect("__mock_set_write_msgs_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_write_msgs_error should succeed");
        }
    }

    /// Forces every subsequent `PassThruIoctl(IOCTL_STOP_REPEAT_MESSAGE)`
    /// call, on every channel, to fail with `code` instead of its normal
    /// success/`ERR_INVALID_MSG_ID` behavior -- lets tests exercise
    /// `PDU_IOCTL_STOP_REPEAT_MESSAGE`'s (ADR-180 Decision 20) leaked-STOP
    /// retry mechanism for a `MsgId` that is genuinely still alive and
    /// retransmitting. `Some(code)` installs the override; `None` clears it
    /// (the default after `reset()`), mirroring `set_stop_filter_error`'s
    /// `Option` convention.
    pub(crate) fn set_stop_repeat_message_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_stop_repeat_message_error\0")
                .expect("__mock_set_stop_repeat_message_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(
                rc, NC,
                "__mock_set_stop_repeat_message_error should succeed"
            );
        }
    }

    /// ADR-192/Phase 7 Stage 7c, Codex review round 2 (P2) fix: forces
    /// every subsequent `PassThruStopPeriodicMsg` call, on every channel, to
    /// fail with `code` instead of its normal unconditional-success
    /// behavior -- lets tests exercise `rpc_cancel_com_primitive`'s
    /// restore-and-error path for a TP2.0 broadcast periodic COP's
    /// `CoptCancel` without a real adapter. `Some(code)` installs the
    /// override; `None` clears it (the default after `reset()`), mirroring
    /// `set_stop_repeat_message_error`'s `Option` convention.
    pub(crate) fn set_stop_periodic_message_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_stop_periodic_message_error\0")
                .expect("__mock_set_stop_periodic_message_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(
                rc, NC,
                "__mock_set_stop_periodic_message_error should succeed"
            );
        }
    }

    /// Forces every subsequent `PassThruSetProgrammingVoltage` call to fail
    /// with `code` instead of its normal success behavior -- lets tests
    /// exercise `PDU_IOCTL_SET_PROG_VOLTAGE`'s native-failure mapping
    /// (A2-21) without a real adapter. `Some(code)` installs the override;
    /// `None` clears it (the default after `reset()`), mirroring
    /// `set_stop_filter_error`'s `Option` convention.
    pub(crate) fn set_prog_voltage_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_prog_voltage_error\0")
                .expect("__mock_set_prog_voltage_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_prog_voltage_error should succeed");
        }
    }

    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): forces
    /// every subsequent `PassThruIoctl(IOCTL_BECOME_MASTER)` call to fail
    /// with `code` instead of its normal success behavior -- lets tests
    /// exercise `PDU_IOCTL_BECOME_MASTER`'s native-failure mapping (e.g. the
    /// documented `ERR_FAILED` "no poll message within 2s" outcome) without
    /// a real adapter or an actual 2-second wait. `Some(code)` installs the
    /// override; `None` clears it (the default after `reset()`), mirroring
    /// `set_prog_voltage_error`'s `Option` convention.
    pub(crate) fn set_become_master_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_become_master_error\0")
                .expect("__mock_set_become_master_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_become_master_error should succeed");
        }
    }

    /// PR #98 GM UART `BECOME_MASTER`-in-flight race regression test: arms
    /// the next `IOCTL_BECOME_MASTER` call to block until
    /// [`Self::release_become_master_hold`] is called. See
    /// `j2534-0404-mock`'s `BecomeMasterHold` doc comment for the underlying
    /// rendezvous mechanism.
    pub(crate) fn arm_become_master_hold(&self) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> c_long> = self
                .lib
                .get(b"__mock_arm_become_master_hold\0")
                .expect("__mock_arm_become_master_hold should be exported");
            let rc = f();
            assert_eq!(rc, NC, "__mock_arm_become_master_hold should succeed");
        }
    }

    /// `true` once the currently-armed [`Self::arm_become_master_hold`]'s
    /// `IOCTL_BECOME_MASTER` call has actually entered its wait -- `false`
    /// if no hold is armed, or the armed hold's call has not reached its
    /// wait yet. Poll this (ADR-149) rather than sleeping a fixed duration
    /// before asserting anything that depends on the call being genuinely
    /// in flight.
    pub(crate) fn become_master_hold_engaged(&self) -> bool {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> u32> = self
                .lib
                .get(b"__mock_become_master_hold_engaged\0")
                .expect("__mock_become_master_hold_engaged should be exported");
            f() != 0
        }
    }

    /// Releases the currently-armed [`Self::arm_become_master_hold`], letting
    /// its blocked `IOCTL_BECOME_MASTER` call return.
    pub(crate) fn release_become_master_hold(&self) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> c_long> = self
                .lib
                .get(b"__mock_release_become_master_hold\0")
                .expect("__mock_release_become_master_hold should be exported");
            let rc = f();
            assert_eq!(rc, NC, "__mock_release_become_master_hold should succeed");
        }
    }

    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): the bytes
    /// most recently staged via a successful `IOCTL_SET_POLL_RESPONSE` on
    /// `channel_id`; empty before any such call. Mirrors `written_data`'s own
    /// out-buffer shape.
    pub(crate) fn poll_response(&self, channel_id: u32) -> Vec<u8> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, *mut u8, u32, *mut u32) -> c_long> = self
                .lib
                .get(b"__mock_get_poll_response\0")
                .expect("__mock_get_poll_response should be exported");
            let mut buf = [0u8; j2534_0404::MAX_MESSAGE_DATA];
            let mut out_len = 0u32;
            let rc = f(channel_id, buf.as_mut_ptr(), buf.len() as u32, &mut out_len);
            assert_eq!(rc, NC, "__mock_get_poll_response should succeed");
            buf[..out_len as usize].to_vec()
        }
    }

    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8):
    /// `supported == false` forces `IOCTL_GET_DEVICE_INFO`'s
    /// `DEVICE_INFO_GM_UART_SUPPORTED` arm to report unsupported on every
    /// subsequent call -- lets tests exercise `enforce_discovery_capability`'s
    /// ADR-185 Stage 1 fail-fast connect-time rejection for this family.
    /// `true` (the default after `reset()`) restores normal
    /// advertised-supported behavior, mirroring `set_chx_capacity`'s
    /// convention.
    pub(crate) fn set_gm_uart_supported(&self, supported: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_gm_uart_supported\0")
                .expect("__mock_set_gm_uart_supported should be exported");
            let rc = f(supported as u32);
            assert_eq!(rc, NC, "__mock_set_gm_uart_supported should succeed");
        }
    }

    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    /// `supported == false` forces `IOCTL_GET_DEVICE_INFO`'s
    /// `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` arm to report unsupported on
    /// every subsequent call -- lets tests exercise
    /// `enforce_discovery_capability`'s ADR-185 Stage 1 fail-fast
    /// connect-time rejection for this family. `true` (the default after
    /// `reset()`) restores normal advertised-supported behavior, mirroring
    /// `set_gm_uart_supported`'s convention.
    pub(crate) fn set_ndis_supported(&self, supported: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_ndis_supported\0")
                .expect("__mock_set_ndis_supported should be exported");
            let rc = f(supported as u32);
            assert_eq!(rc, NC, "__mock_set_ndis_supported should succeed");
        }
    }

    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): forces every
    /// subsequent `PassThruConnect` whose `protocol_id ==
    /// PROTOCOL_ETHERNET_NDIS` to fail with `code` instead of its normal
    /// success behavior -- lets tests exercise clause 24's
    /// activation-line-not-achieved connect failure path
    /// (`ERR_NO_CONNECTION_ESTABLISHED` -> `PDU_ERR_NO_CABLE_DETECTED`)
    /// without a real adapter. Unaffected by any other protocol's connect.
    /// `Some(code)` installs the override; `None` clears it (the default
    /// after `reset()`), mirroring `set_fast_init_error`'s `Option`
    /// convention.
    pub(crate) fn set_ndis_connect_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_ndis_connect_error\0")
                .expect("__mock_set_ndis_connect_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_ndis_connect_error should succeed");
        }
    }

    /// Forces every subsequent `IOCTL_READ_J1962PIN_VOLTAGE` call to fail
    /// with `code`, before the mock's pin-based validation runs -- lets
    /// tests exercise `PDU_IOCTL_READ_J1962PIN_VOLTAGE`'s generic
    /// native-failure fallback (any code other than `ERR_PIN_INVALID`,
    /// which the pin-based checks alone can never produce) without a real
    /// adapter. `Some(code)` installs the override; `None` clears it (the
    /// default after `reset()`), mirroring `set_prog_voltage_error`'s
    /// `Option` convention.
    pub(crate) fn set_j1962_pin_voltage_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_j1962_pin_voltage_error\0")
                .expect("__mock_set_j1962_pin_voltage_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_j1962_pin_voltage_error should succeed");
        }
    }

    /// Forces every subsequent `PassThruReadMsgs` call, on every channel, to
    /// fail with `code` instead of its normal success/`ERR_BUFFER_EMPTY`
    /// behavior -- lets tests exercise `j2534-0404-service`'s background poll
    /// task's hard-channel-error path (`poll_rx_inner`/
    /// `handle_channel_hard_error`, ADR-105) without a real adapter.
    /// `Some(code)` installs the override; `None` clears it (the default
    /// after `reset()`), mirroring `set_prog_voltage_error`'s `Option`
    /// convention.
    pub(crate) fn set_read_msgs_error(&self, code: Option<c_long>) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = self
                .lib
                .get(b"__mock_set_read_msgs_error\0")
                .expect("__mock_set_read_msgs_error should be exported");
            let rc = f(code.unwrap_or(0));
            assert_eq!(rc, NC, "__mock_set_read_msgs_error should succeed");
        }
    }

    /// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): forces
    /// every subsequent `IOCTL_PROTECT_J1939_ADDR` claim attempt (non-cancel
    /// form) on every channel to resolve `J1939_ADDRESS_LOST` instead of
    /// `J1939_ADDRESS_CLAIMED` -- lets tests exercise
    /// `j2534-0404-service`'s retry-over-the-`CP_J1939PreferredAddress`-list
    /// claim state machine (ADR-179 Decision 3) without a real adapter.
    /// `true` installs the override; `false` clears it (the default after
    /// `reset()`), mirroring `set_can_mixed_format_unsupported`'s bool
    /// convention. This is a single GLOBAL toggle, not per-address/per-call:
    /// it affects every subsequent claim attempt on every channel until
    /// cleared, regardless of which candidate address is being attempted.
    pub(crate) fn set_j1939_claim_lost(&self, lost: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_j1939_claim_lost\0")
                .expect("__mock_set_j1939_claim_lost should be exported");
            let rc = f(lost as u32);
            assert_eq!(rc, NC, "__mock_set_j1939_claim_lost should succeed");
        }
    }

    /// ADR-180 Decision 21 regression coverage (design-advisor consult,
    /// Codex review PR #72): forces every subsequent `IOCTL_PROTECT_J1939_
    /// ADDR` claim attempt (non-cancel form) on every channel to return
    /// success synchronously without ever reporting `J1939_ADDRESS_CLAIMED`/
    /// `_LOST` -- unlike [`Self::set_j1939_claim_lost`]'s immediate `_LOST`
    /// push, this makes `j2534-0404-service`'s bounded claim wait actually
    /// wait, so a test can deterministically send `CancelComPrimitive` while
    /// it is in flight. `true` installs the override; `false` clears it
    /// (the default after `reset()`), same global-toggle convention as
    /// [`Self::set_j1939_claim_lost`].
    pub(crate) fn set_j1939_claim_no_indication(&self, enabled: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_j1939_claim_no_indication\0")
                .expect("__mock_set_j1939_claim_no_indication should be exported");
            let rc = f(enabled as u32);
            assert_eq!(
                rc, NC,
                "__mock_set_j1939_claim_no_indication should succeed"
            );
        }
    }

    /// ADR-180 Decision 22 regression coverage (design-advisor consult,
    /// Codex review PR #72): forces every subsequent `IOCTL_PROTECT_J1939_
    /// ADDR` CANCEL form (all-zero NAME) on every channel to return a
    /// failure status instead of its normal unconditional success, WITHOUT
    /// removing the address from the mock's own claimed-address bookkeeping
    /// -- test realism for a `cancel_j1939_claims_for_cll`/leaked-claim
    /// retry that fails natively. `true` installs the override; `false`
    /// clears it (the default after `reset()`), same global-toggle
    /// convention as [`Self::set_j1939_claim_lost`].
    /// ADR-188 regression coverage (Codex review, PR #97, 4th round): forces
    /// every subsequent `IOCTL_REQUEST_CONNECTION` (non-slots-full success
    /// path) on every channel to still allocate a real
    /// `tp20_connections` slot but return success synchronously without
    /// ever reporting `CONNECTION_ESTABLISHED` -- unlike the immediate
    /// `CONNECTION_ESTABLISHED`/`_LOST` pushes every other TP2.0 path
    /// uses, this makes `j2534-0404-service`'s bounded connection-request
    /// wait actually wait, so a test can deterministically disconnect the
    /// requesting CLL while it is in flight. `true` installs the override;
    /// `false` clears it (the default after `reset()`), same global-toggle
    /// convention as [`Self::set_j1939_claim_no_indication`].
    pub(crate) fn set_tp20_no_indication(&self, enabled: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_tp20_no_indication\0")
                .expect("__mock_set_tp20_no_indication should be exported");
            let rc = f(enabled as u32);
            assert_eq!(rc, NC, "__mock_set_tp20_no_indication should succeed");
        }
    }

    pub(crate) fn set_j1939_cancel_error(&self, enabled: bool) {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> c_long> = self
                .lib
                .get(b"__mock_set_j1939_cancel_error\0")
                .expect("__mock_set_j1939_cancel_error should be exported");
            let rc = f(enabled as u32);
            assert_eq!(rc, NC, "__mock_set_j1939_cancel_error should succeed");
        }
    }

    /// ADR-190/Phase 7 Stage 7b: simulates an unsolicited inbound TP2.0
    /// connection request arriving for `channel_id`'s currently-armed
    /// passive listener (`__mock_inject_tp20_passive_connection`'s own doc
    /// comment). Returns `true` if the injection actually landed (a
    /// connection was established), `false` if it silently no-op'd (the
    /// listener isn't armed, or the channel is at four-slot capacity).
    pub(crate) fn inject_tp20_passive_connection(&self, channel_id: u32, peer_tx_id: u32) -> bool {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32) -> u32> = self
                .lib
                .get(b"__mock_inject_tp20_passive_connection\0")
                .expect("__mock_inject_tp20_passive_connection should be exported");
            f(channel_id, peer_tx_id) != 0
        }
    }

    /// ADR-190/Phase 7 Stage 7b: the number of times
    /// [`Self::inject_tp20_passive_connection`] has silently no-op'd for
    /// `channel_id` since the mock was last reset.
    pub(crate) fn passive_connection_rejected_count(&self, channel_id: u32) -> u32 {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32) -> u32> = self
                .lib
                .get(b"__mock_get_passive_connection_rejected_count\0")
                .expect("__mock_get_passive_connection_rejected_count should be exported");
            f(channel_id)
        }
    }
}

/// ADR-219 (as amended): default `vendor_ioctls` config for the mock
/// library's two dedicated vendor-range `IoctlID`s
/// (`j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32`/`MOCK_VENDOR_IOCTL_WRAPPED_ECHO`),
/// baked into every `TestServer::start*` call's config file (not opt-in via
/// `extra_toml`) so every existing vendor-passthrough test keeps working
/// unchanged now that a non-NULL-buffer vendor IOCTL requires a configured
/// contract. `0x00010001` (`MOCK_VENDOR_IOCTL_RAW_U32`) reads/writes exactly
/// 4 bytes through a direct `u32 *` pair; `0x00010002`
/// (`MOCK_VENDOR_IOCTL_WRAPPED_ECHO`) is `SBYTE_ARRAY`-wrapped (no configured
/// size needed). `0x00010003` (`shape = "raw", input_bytes = 0, output_bytes
/// = 4`) is an unregistered-at-the-mock `cmd_id`, used only to exercise the
/// "`input_bytes = 0` configured but client sends non-empty input"
/// rejection, which happens at dispatch time before any native call is ever
/// made, so no mock-side native handler is needed for it. `0x00010000` (the
/// bare vendor-range floor, `shape = "raw"`, both byte counts left at `0`)
/// is the documented way to allowlist a genuinely bufferless vendor command
/// (ADR-219's seventh-round amendment reversed the earlier "an unconfigured
/// cmd_id needs no config for a NULL/NULL request" exemption -- every
/// vendor `cmd_id` must now appear in this table, bufferless ones
/// included). `0x00010005` (`shape = "sbyte_array", input_required = true,
/// output_required = true`) is likewise unregistered at the mock, used only
/// to exercise the wrapped-mode required-direction rejections, which also
/// happen at dispatch time before any native call.
///
/// Any OTHER `cmd_id` (e.g. `0x00010004`) stays deliberately unconfigured,
/// so tests exercising the unconfigured-`cmd_id` rejection path use a
/// `cmd_id` outside this table.
const VENDOR_IOCTLS_TOML: &str = concat!(
    "[config.apis.j2534-0404.libs.mock-lib.vendor_ioctls.\"0x00010000\"]\n",
    "shape = \"raw\"\n",
    "[config.apis.j2534-0404.libs.mock-lib.vendor_ioctls.\"0x00010001\"]\n",
    "shape = \"raw\"\n",
    "input_bytes = 4\n",
    "output_bytes = 4\n",
    "[config.apis.j2534-0404.libs.mock-lib.vendor_ioctls.\"0x00010002\"]\n",
    "shape = \"sbyte_array\"\n",
    "[config.apis.j2534-0404.libs.mock-lib.vendor_ioctls.\"0x00010003\"]\n",
    "shape = \"raw\"\n",
    "input_bytes = 0\n",
    "output_bytes = 4\n",
    "[config.apis.j2534-0404.libs.mock-lib.vendor_ioctls.\"0x00010005\"]\n",
    "shape = \"sbyte_array\"\n",
    "input_required = true\n",
    "output_required = true\n",
);

/// Serializes every `TestServer::start*` call's config-file-write +
/// `VCI_CONFIG_PATH` critical section against every other concurrent one in
/// this binary -- see [`TestServer::try_start_with_extra_config`]'s own doc
/// comment for the full race this closes.
static VCI_CONFIG_STARTUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct TestServer {
    port: u16,
    shutdown_tx: Option<oneshot::Sender<()>>,
    _service_shutdown_tx: tokio::sync::watch::Sender<bool>,
    server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    pub(crate) backdoor: MockBackdoor,
}

impl TestServer {
    /// Points `vci-service-launcher`'s library-path config at the compiled
    /// `j2534-0404-mock` cdylib (via `VCI_CONFIG_PATH`), resets mock state,
    /// and starts a `J2534Service` backed by it.
    pub(crate) async fn start() -> Self {
        Self::start_with_can_mode(None).await
    }

    /// Like [`TestServer::start`], but also writes a `can_channel_mode` entry
    /// into the test `config.toml` (ADR-046).
    pub(crate) async fn start_with_can_mode(can_channel_mode: Option<&str>) -> Self {
        let mode_line = can_channel_mode
            .map(|mode| format!("can_channel_mode = {mode:?}\n"))
            .unwrap_or_default();
        Self::try_start_with_extra_config(&mode_line)
            .await
            .expect("service should initialize with mock library")
    }

    /// Like [`TestServer::start`], but also writes a `modules` array-of-tables
    /// into the test `config.toml` (ADR-106), one `[[...modules]]` entry per
    /// `(label, pname)` pair. Panics if startup fails -- for the failure case
    /// (e.g. `modules = []`), use [`TestServer::try_start_with_extra_config`]
    /// directly.
    pub(crate) async fn start_with_modules(modules: &[(&str, &str)]) -> Self {
        Self::try_start_with_extra_config(&modules_toml(modules))
            .await
            .expect("service should initialize with a valid modules config")
    }

    /// Like [`TestServer::start_with_can_mode`]/[`TestServer::start_with_modules`],
    /// but returns the startup `Result` instead of panicking, and lets the
    /// caller supply arbitrary extra TOML lines appended inside
    /// `[config.apis.j2534-0404.libs.mock-lib]` -- for tests asserting a
    /// startup error (e.g. `modules = []`, ADR-106).
    ///
    /// **Serialized against every other concurrent `TestServer::start*` call
    /// in this binary via [`VCI_CONFIG_STARTUP_LOCK`]** (a sixth flaky-test
    /// root cause beyond the five ADR-149 already covers, see the module doc
    /// above and `docs/implementation-notes.md`'s "Known Flaky Tests"):
    /// `config_path`/`VCI_CONFIG_PATH` are both process-global mutable state
    /// -- a fixed temp-file path, written non-atomically via `std::fs::write`,
    /// and an environment variable (`unsafe` to set since Rust 1.82+ for
    /// exactly this reason) -- read synchronously and repeatedly (once per
    /// `vci_service_config` lookup) by `J2534Service::get_startup_config`/
    /// `::new` below, all before either ever awaits. Two tests running on
    /// genuinely parallel OS threads (the normal `cargo test` execution
    /// model; `#[serial]` only serializes against OTHER `#[serial]` tests,
    /// not the many non-`#[serial]` ones) can interleave their writes/reads
    /// of this shared file and env var with no synchronization at all --
    /// `vci-service-config::load_toml_config` silently treats a resulting
    /// torn/corrupted read as "nothing configured" (`TomlConfig::default()`,
    /// a warning-only `eprintln!`) rather than propagating the parse error,
    /// so the observable failure is `library_path` silently resolving to
    /// `None` and falling through to real Windows-registry auto-discovery,
    /// which immediately errors `RegistryUnsupported` on any other platform.
    /// The lock is held from just before the file write through the end of
    /// `J2534Service::new`'s `.await` (confirmed to read `VCI_CONFIG_PATH`
    /// only synchronously, before its first real await point, but the lock
    /// spans the whole `.await` regardless -- correct even if a future
    /// refactor of `new()` introduces a real yield point before it finishes
    /// consuming the startup config) -- nothing after that point touches
    /// `config_path`/`VCI_CONFIG_PATH`.
    ///
    /// `VCI_CONFIG_STARTUP_LOCK` only ever handled the intra-process case: it
    /// is a per-process Rust static, so it does nothing against two separate
    /// `cargo test` processes racing on the same fixed config file path. To
    /// close that gap, `config_path` is now also made per-process-unique (via
    /// `std::process::id()`), so two concurrent processes never write to the
    /// same file even without any cross-process synchronization (ADR-195).
    pub(crate) async fn try_start_with_extra_config(
        extra_toml: &str,
    ) -> Result<Self, vci_service_launcher::BoxError> {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");

        let backdoor = MockBackdoor::open(&lib_path);
        backdoor.reset();

        let _startup_config_guard = VCI_CONFIG_STARTUP_LOCK.lock().await;

        let config_path = std::env::temp_dir().join(format!(
            "j2534-0404-service-grpc-mock-test-config-{}.toml",
            std::process::id()
        ));
        // `extra_toml` MUST come before `VENDOR_IOCTLS_TOML`: some callers
        // (e.g. `start_with_can_mode`) pass a bare `key = value` line (no
        // section header of its own) that relies on still being inside the
        // just-opened `[config.apis.j2534-0404.libs.mock-lib]` table -- once
        // `VENDOR_IOCTLS_TOML`'s own `[...vendor_ioctls."0x...."]` headers
        // appear, a later bare key/value line would instead land inside the
        // last-opened sub-table, not back at the top-level instance table
        // (TOML has no way to "return" to an enclosing table after a nested
        // header). `VENDOR_IOCTLS_TOML`'s own headers are always fully
        // qualified, so its position relative to `modules_toml`'s
        // `[[...modules]]` array-of-tables entries (also fully qualified)
        // does not matter either way.
        let config_toml = format!(
            "[config.apis.j2534-0404.libs.mock-lib]\nlibrary_path = {:?}\n{extra_toml}{VENDOR_IOCTLS_TOML}",
            lib_path.display().to_string()
        );
        std::fs::write(&config_path, config_toml).expect("config file should be writable");
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", &config_path);
        }

        let (service_shutdown_tx, service_shutdown_rx) = tokio::sync::watch::channel(false);
        let service = J2534Service::new(
            J2534Service::get_startup_config(["j2534-0404-service", "j2534-0404:mock-lib"])
                .expect("startup config"),
            service_shutdown_rx,
        )
        .await?;

        drop(_startup_config_guard);

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener should have a port")
            .port();
        let incoming = TcpListenerStream::new(listener);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(VciServiceServer::new(service))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Ok(Self {
            port,
            shutdown_tx: Some(shutdown_tx),
            _service_shutdown_tx: service_shutdown_tx,
            server_handle,
            backdoor,
        })
    }

    pub(crate) async fn client(&self) -> VciServiceClient<Channel> {
        VciServiceClient::connect(format!("http://127.0.0.1:{}", self.port))
            .await
            .expect("client should connect")
    }

    pub(crate) async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        self.server_handle
            .await
            .expect("server task should complete")
            .expect("server should stop cleanly");
    }
}

/// Builds the `[[config.apis.j2534-0404.libs.mock-lib.modules]]` TOML
/// array-of-tables for [`TestServer::start_with_modules`] (ADR-106), one
/// entry per `(label, pname)` pair.
pub(crate) fn modules_toml(modules: &[(&str, &str)]) -> String {
    let mut toml = String::new();
    for (label, pname) in modules {
        toml.push_str(&format!(
            "\n[[config.apis.j2534-0404.libs.mock-lib.modules]]\nlabel = {label:?}\npname = {pname:?}\n"
        ));
    }
    toml
}

pub(crate) fn resource_with_protocol(protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// Like `resource_with_protocol`, but also names a D-PDU bustype so
/// `CreateComLogicalLink` pre-populates a nonzero default `DATA_RATE` in the
/// Working set (`comparam_defaults::bustype_default_params`) without ever
/// calling `SetComParam`.
pub(crate) fn resource_with_protocol_and_bustype(
    protocol_id: u32,
    bustype_name: &str,
) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeName(bustype_name.to_owned())),
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// Creates (but does not connect) a CLL for `protocol_id` and returns its
/// handle.
pub(crate) async fn create_cll(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(protocol_id),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Sets a single `PDU_PC_COM`-class Unum32 ComParam on `cll_handle`.
pub(crate) async fn set_com_param_unum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
    value: u32,
) {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(value)),
            }),
        })
        .await
        .unwrap_or_else(|err| {
            panic!("set_com_param({com_param_id:#x}={value}) should succeed: {err}")
        });
}

/// Sets a single `PDU_PC_COM`-class Bytefield ComParam on `cll_handle` (e.g.
/// `CP_TesterPresentMessage`).
pub(crate) async fn set_com_param_bytes(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
    value: Vec<u8>,
) {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Bytefield(value)),
            }),
        })
        .await
        .unwrap_or_else(|err| {
            panic!("set_com_param({com_param_id:#x}=<bytes>) should succeed: {err}")
        });
}

/// Reads a single Unum32 ComParam from `cll_handle`'s Working set via
/// `GetComParam` (`GetComParam` always reads Working -- ADR-067's writeback
/// tests use this to confirm Working now equals Active after a successful
/// `temp_param_update` call). Returns `0` when the param was never set,
/// matching `rpc_get_com_param`'s own default.
pub(crate) async fn get_com_param_unum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
) -> u32 {
    let response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(vci_service_interface::get_com_param_request::Param::ParamId(com_param_id)),
        })
        .await
        .unwrap_or_else(|err| panic!("get_com_param({com_param_id:#x}) should succeed: {err}"))
        .into_inner();
    match response.param_item.and_then(|p| p.param_data) {
        Some(param_item::ParamData::Unum32(v)) => v,
        other => {
            panic!("get_com_param({com_param_id:#x}) returned unexpected param_data: {other:?}")
        }
    }
}

/// Sets a single `PDU_PC_COM`-class Unum32 ComParam on `cll_handle` by its
/// D-PDU shortname (`ParamName`) rather than a numeric id -- exercises
/// `names.rs::map_comparam_name`'s resolution path directly (used by
/// ADR-181's CP_W1-W4 Min/Max collision-fix tests, where the point under
/// test is name-to-id resolution itself, not just raw-id storage).
pub(crate) async fn set_com_param_by_name(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    name: &str,
    value: u32,
) {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamName(
                    name.to_owned(),
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(value)),
            }),
        })
        .await
        .unwrap_or_else(|err| {
            panic!("set_com_param(ParamName({name:?})={value}) should succeed: {err}")
        });
}

/// Reads a single Unum32 ComParam from `cll_handle`'s Working set by its
/// D-PDU shortname (`ParamName`) -- see [`set_com_param_by_name`].
pub(crate) async fn get_com_param_by_name_unum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    name: &str,
) -> u32 {
    let response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(
                vci_service_interface::get_com_param_request::Param::ParamName(name.to_owned()),
            ),
        })
        .await
        .unwrap_or_else(|err| panic!("get_com_param(ParamName({name:?})) should succeed: {err}"))
        .into_inner();
    match response.param_item.and_then(|p| p.param_data) {
        Some(param_item::ParamData::Unum32(v)) => v,
        other => {
            panic!("get_com_param(ParamName({name:?})) returned unexpected param_data: {other:?}")
        }
    }
}

pub(crate) async fn create_and_connect_cll(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll(client, protocol_id, 1).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Like [`create_cll`], but sets `cll_create_flag_bits` to
/// `CLL_CREATE_FLAG_RAW_MODE` (ADR-196 Decision item 1) -- for exercising
/// Phase 1 RawMode=ON behavior on a protocol Phase 1's allowlist accepts.
/// Rejection cases (an unsupported protocol, software ISO-TP, a malformed
/// flag) construct `CreateComLogicalLinkRequest` directly instead, one per
/// test, since each needs a different `cll_create_flag` shape.
pub(crate) async fn create_cll_raw_mode(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(protocol_id),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
                }),
            ),
        })
        .await
        .expect("create_com_logical_link with RawMode=ON should succeed on an allowlisted protocol")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_and_connect_cll`], but creates the CLL with RawMode=ON
/// (ADR-196) via [`create_cll_raw_mode`].
pub(crate) async fn create_and_connect_cll_raw_mode(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll_raw_mode(client, protocol_id, 1).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Like [`create_cll_raw_mode`], but also sets `CLL_CREATE_FLAG_CHECKSUM_MODE`
/// when `checksum_mode` is `true` (ADR-198 Phase 2) -- for exercising the
/// K-line (ISO9141/ISO14230) RawMode/ChecksumMode combinations Phase 2 gives
/// real semantics. `checksum_mode = false` produces the exact same
/// `cll_create_flag_bits` shape [`create_cll_raw_mode`] does.
pub(crate) async fn create_cll_raw_checksum_mode(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    _cll_tag: u64,
    checksum_mode: bool,
) -> ComLogicalLinkHandle {
    let mut bits = vec![CllCreateFlagBit::CllCreateFlagRawMode as i32];
    if checksum_mode {
        bits.push(CllCreateFlagBit::CllCreateFlagChecksumMode as i32);
    }
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(protocol_id),
            )),
            cll_create_flag: Some(
                create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(CllCreateFlag {
                    bits,
                }),
            ),
        })
        .await
        .expect("create_com_logical_link with RawMode=ON should succeed on an allowlisted protocol")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_and_connect_cll_raw_mode`], but via
/// [`create_cll_raw_checksum_mode`] -- see that helper's doc comment.
pub(crate) async fn create_and_connect_cll_raw_checksum_mode(
    client: &mut VciServiceClient<Channel>,
    protocol_id: u32,
    checksum_mode: bool,
    cll_tag: u64,
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle =
        create_cll_raw_checksum_mode(client, protocol_id, cll_tag, checksum_mode).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// Like [`create_cll`], but for a `module_handle` other than
/// [`MOCK_MODULE_HANDLE`] -- for ADR-107 multi-module configs
/// (`TestServer::start_with_modules`) where a CLL must be addressed to a
/// specific configured module rather than the fixed default.
pub(crate) async fn create_cll_for_module(
    client: &mut VciServiceClient<Channel>,
    module_handle: u32,
    protocol_id: u32,
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(protocol_id),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_and_connect_cll`], but for a `module_handle` other than
/// [`MOCK_MODULE_HANDLE`] -- see [`create_cll_for_module`].
pub(crate) async fn create_and_connect_cll_for_module(
    client: &mut VciServiceClient<Channel>,
    module_handle: u32,
    protocol_id: u32,
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll_for_module(client, module_handle, protocol_id, 1).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// ADR-100 Decision §5 (S8): registers a `NumSendCycles == 0` (ADR-059)
/// receive-only ComPrimitive with a vacuous (empty mask/pattern)
/// `ExpectedResponseStructure` and `NumReceiveCycles == -1` on `cll_handle` --
/// the spec's own migration path for a client that wants to keep observing
/// any frame that would otherwise now be discarded as unbound. Many tests
/// written before ADR-100 inject a frame with no COP or tester-present
/// listening at all, relying on the old blanket unsolicited-delivery
/// behavior to observe it; this helper restores that observability under the
/// new attribution-and-discard model without changing what any of those
/// tests actually assert (`acceptance_id: 0` matches the pre-ADR-100
/// unattributed convention already used elsewhere for an unbound/indication
/// frame's `ResultData`). Tester-present's own discard (ADR-100 Decision §3
/// step 3) still outranks this tier-2 registrant, so a test proving tester-
/// present discards a frame is unaffected by also arming this monitor.
///
/// `cop_data` is a dummy non-empty payload (`[0x00]`), never actually
/// transmitted (`NumSendCycles == 0`): `rpc_start_com_primitive` resolves
/// and size-validates the constructed TX message unconditionally for
/// `CoptSendrecv`, even when nothing will be sent, so an empty payload
/// (ISO 22900-2 §9.2.6.3.4 RECEIVE ONLY's own NOTE 2: this ComPrimitive type
/// carries no pCopData bytes) is rejected on any protocol whose
/// minimum TX size is nonzero -- a pre-existing gap, out of scope for
/// ADR-100 to fix, worked around the same way `cop_ctrl_cycles.rs`'s own
/// `sendrecv_zero_send_cycles_skips_transmission_and_only_receives` already
/// does. This resolution also still requires CAN-family TX addressing to be
/// resolvable (a UniqueRespIdTable entry with `CP_CanPhysReqId`, or
/// functional addressing) even for a receive-only COP -- callers on a CLL
/// with neither configured cannot use this helper as-is.
pub(crate) async fn arm_receive_only_monitor(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 0,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, receive-only monitor) should succeed");
}

/// Sends `data` via `CoptSendrecv`, optionally with named `tx_flag` bits, and
/// waits for `PduCopstFinished` (via `GetEventItem` polling would work too,
/// but a short sleep is simpler here since the mock's write path is
/// synchronous and near-instant).
pub(crate) async fn send_data(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    data: Vec<u8>,
    tx_flag_bits: Vec<TxFlagBit>,
) {
    let tx_flag = if tx_flag_bits.is_empty() {
        None
    } else {
        Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
            bits: tx_flag_bits.into_iter().map(|b| b as i32).collect(),
        }))
    };

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // The poll task dispatches PassThruWriteMsgs asynchronously; give it a
    // moment to run before inspecting the mock's written-message log.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

/// Like `send_data`, but sets `tx_flag_raw` (the ISO 22900-2 D.2.1 Table D.4
/// 4-byte `TxFlag` byte-array layout, byte 0 first -- ADR-116) instead of
/// named `TxFlagBits`.
pub(crate) async fn send_data_with_raw_tx_flag(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    data: Vec<u8>,
    tx_flag_raw: Vec<u8>,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: Some(com_primitive_ctrl_data::TxFlag::TxFlagRaw(tx_flag_raw)),
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // The poll task dispatches PassThruWriteMsgs asynchronously; give it a
    // moment to run before inspecting the mock's written-message log.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

/// Like `send_data`, but for a `cop_data` this service is expected to reject
/// on ComParam-dependent grounds (missing addressing, TX size range,
/// ISO15765-2 Single Frame limit -- ADR-049/ADR-055). ADR-067 reverts
/// ADR-064's deferral of this validation to the poll task: resolution binds
/// against a call-time ComParam snapshot in `rpc_start_com_primitive` itself,
/// so a resolution failure is once again a synchronous `StartComPrimitive`
/// `INVALID_ARGUMENT`, as it was before ADR-064.
pub(crate) async fn send_data_expect_rejected(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    data: Vec<u8>,
) -> tonic::Status {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: data,
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "start_com_primitive should reject cop_data outside the protocol's TX size range",
        )
}

/// Like [`send_data_expect_rejected`], but also sets `tx_flag_bits`
/// (mirroring [`send_data`]'s own `tx_flag_bits` handling) -- for a
/// rejection whose reason depends on a client-set TxFlag bit (e.g. a
/// RawMode CLL's client-authoritative `TxFlagIso15765AddrType`).
pub(crate) async fn send_data_expect_rejected_with_flags(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    data: Vec<u8>,
    tx_flag_bits: Vec<TxFlagBit>,
) -> tonic::Status {
    let tx_flag = if tx_flag_bits.is_empty() {
        None
    } else {
        Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
            bits: tx_flag_bits.into_iter().map(|b| b as i32).collect(),
        }))
    };

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag,
            }),
        })
        .await
        .expect_err(
            "start_com_primitive should reject cop_data outside the protocol's TX size range",
        )
}

/// Configures `cll_handle`'s Working UniqueRespIdTable with a single entry
/// whose only address is `CP_CanPhysReqId = phys_req_id` (normal
/// addressing). The service constructs the outgoing CAN ID from this entry
/// (ADR-050). Stages Working only -- see `set_unique_resp_table`'s doc
/// comment (ADR-068).
pub(crate) async fn set_can_phys_req_id(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    phys_req_id: u32,
) {
    set_unique_resp_table(
        client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![unum32_param(CP_CAN_PHYS_REQ_ID, phys_req_id)],
        )],
    )
    .await;
}

/// Like `set_can_phys_req_id`, but also promotes the table to Active via
/// `CoptUpdateparam` (ADR-068) -- see `set_unique_resp_table_and_promote`.
pub(crate) async fn set_can_phys_req_id_and_promote(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    phys_req_id: u32,
) {
    set_can_phys_req_id(client, cll_handle, phys_req_id).await;
    promote_via_update_param(client, cll_handle).await;
}

/// Polls `events` until `predicate` returns `true` for an `EventItem`, or
/// `timeout_ms` elapses. Returns `false` on timeout, stream end, or a stream
/// error, in which case the caller should treat the wait as failed.
pub(crate) async fn wait_for_event<F>(
    events: &mut tonic::Streaming<EventNotification>,
    timeout_ms: u64,
    mut predicate: F,
) -> bool
where
    F: FnMut(&vci_service_interface::EventItem) -> bool,
{
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok(poll_result) = tokio::time::timeout(remaining, events.message()).await else {
            return false; // timed out waiting for the next message
        };
        let Ok(Some(notification)) = poll_result else {
            return false; // stream ended or errored
        };
        if let Some(event_notification::EventData::Item(item)) = notification.event_data
            && predicate(&item)
        {
            return true;
        }
    }
}

/// D-PDU `CP_Can{PhysReq,RespUSDT}{Id,Format,ExtAddr}` ComParam IDs
/// (service-internal `PDU_PC_UNIQUE_ID` scheme; see
/// `j2534-0404-service/src/service.rs`). Not exported by the crate, so
/// duplicated here as literals for the test.
pub(crate) const CP_CAN_PHYS_REQ_EXT_ADDR: u32 = 0x8060;

pub(crate) const CP_CAN_PHYS_REQ_FORMAT: u32 = 0x8061;

pub(crate) const CP_CAN_PHYS_REQ_ID: u32 = 0x8062;

pub(crate) const CP_CAN_RESP_USDT_EXT_ADDR: u32 = 0x8063;

pub(crate) const CP_CAN_RESP_USDT_FORMAT: u32 = 0x8064;

/// D-PDU `CP_Bs` ComParam ID (`PDU_PC_COM` class, Unum32): the ISO 15765-2
/// N_Bs (FlowControl wait) timeout used by the software ISO-TP TX driver
/// (ADR-046). Stored in µs per the D-PDU API timing-param convention (like
/// `CP_P2Max`/`CP_TesterPresentTime`) and converted to ms by
/// `isotp_n_bs_timeout_ms` -- a prior version of this comment described a
/// (buggy) direct-as-ms reading that has since been fixed to actually
/// convert. Default 1000 ms when unset/zero.
pub(crate) const CP_N_BS: u32 = 0x8046;

pub(crate) const CP_CAN_RESP_USDT_ID: u32 = 0x8065;

pub(crate) const CP_CAN_RESP_UUDT_EXT_ADDR: u32 = 0x8066;

pub(crate) const CP_CAN_RESP_UUDT_FORMAT: u32 = 0x8067;

pub(crate) const CP_CAN_RESP_UUDT_ID: u32 = 0x8068;

pub(crate) const CP_CAN_FUNC_REQ_ID: u32 = 0x806B;

/// D-PDU `CP_CanFuncReqFormat` ComParam ID (`PDU_PC_COM` class, link-level —
/// functional requests are not per-ECU): Table B.13 bit 1 (29-bit CAN Id).
pub(crate) const CP_CAN_FUNC_REQ_FORMAT: u32 = 0x806A;

/// D-PDU `CP_RequestAddrMode` ComParam ID (`PDU_PC_COM` class): `1` =
/// physical (default), `2` = functional (ADR-054).
pub(crate) const CP_REQUEST_ADDR_MODE: u32 = 0x8078;

/// D-PDU `CP_FuncReqFormatPriorityType` ComParam ID (`PDU_PC_COM` class for
/// CAN/J1850, `PDU_PC_UNIQUE_ID` for KWP -- KWP/J1850 physical addressing;
/// see `service_params::PARAM_FUNC_REQ_FORMAT_PRIORITY`): the functional
/// (broadcast) request's format/priority byte, read by
/// `tx_header::kwp_header_bytes`/`j1850_header_bytes` when
/// `CP_RequestAddrMode`/`CP_TesterPresentAddrMode` select functional
/// addressing (ADR-054/ADR-138). Promoted here from a per-file local const
/// once a second test module needed it (this suite's convention already
/// promotes a ComParam ID once more than one file needs it -- see
/// `CP_REQUEST_ADDR_MODE` above).
pub(crate) const CP_FUNC_REQ_FORMAT_PRIORITY: u32 = 0x8071;

/// D-PDU `CP_FuncReqTargetAddr` ComParam ID (`service_params::
/// PARAM_FUNC_REQ_TARGET_ADDR`): the functional (broadcast) request's target
/// address, read by the same `tx_header.rs` builders as
/// [`CP_FUNC_REQ_FORMAT_PRIORITY`].
pub(crate) const CP_FUNC_REQ_TARGET_ADDR: u32 = 0x8072;

/// D-PDU `CP_PhysReqFormatPriorityType` ComParam ID (`service_params::
/// PARAM_PHYS_REQ_FORMAT_PRIORITY`): the physical request's format/priority
/// byte, read by `tx_header::kwp_header_bytes`/`j1850_header_bytes` under
/// physical (default) addressing.
pub(crate) const CP_PHYS_REQ_FORMAT_PRIORITY: u32 = 0x8075;

/// D-PDU `CP_PhysReqTargetAddr` ComParam ID (`service_params::
/// PARAM_PHYS_REQ_TARGET_ADDR`): the physical request's fallback target
/// address (used when the CLL's first UniqueRespIdTable entry has no
/// `CP_EcuRespSourceAddress`), read by the same `tx_header.rs` builders as
/// [`CP_PHYS_REQ_FORMAT_PRIORITY`].
pub(crate) const CP_PHYS_REQ_TARGET_ADDR: u32 = 0x8076;

/// D-PDU `CP_K_L_LineInit` ComParam ID (`PDU_PC_COM` class, ISO9141/ISO14230):
/// `0` = K and L line (default), `1` = K line only.
pub(crate) const CP_K_L_LINE_INIT: u32 = 0x80A6;

/// Service-only `CP_ModifyTiming` ComParam ID (`PDU_PC_COM` class, ADR-146/
/// ADR-150): `0` = disabled (default), nonzero = enable the KWP Access
/// Timing Parameter / UDS DiagnosticSessionControl live-exchange config
/// (`service::PARAM_MODIFY_TIMING`).
pub(crate) const CP_MODIFY_TIMING: u32 = 0x8014;

/// Service-only `CP_TesterPresentMessage` ComParam ID (`PDU_PC_COM` class,
/// Bytefield): the `CoptStartcomm` periodic tester-present payload
/// (payload-only, ADR-050). Empty/unset = no tester-present.
pub(crate) const CP_TESTER_PRESENT_MESSAGE: u32 = 0x8001;

/// Service-only `CP_TesterPresentTime` ComParam ID (`PDU_PC_COM` class,
/// Unum32): the `CoptStartcomm` tester-present send interval, in
/// microseconds (`0` = disabled even when a message is configured).
/// `resolve_tester_present` converts this to native J2534 ms resolution
/// (ADR-083, round-to-nearest via `us_to_ms`).
pub(crate) const CP_TESTER_PRESENT_TIME: u32 = 0x8002;

/// D-PDU `CP_TesterPresentAddrMode` ComParam ID (`PDU_PC_COM` class,
/// Unum32): addressing mode for periodic/idle-triggered tester-present,
/// independent of `CP_RequestAddrMode` (ADR-138). `0`/absent = physical
/// (default for every protocol identity except `ISO_15765_3`), `1` =
/// functional.
pub(crate) const CP_TESTER_PRESENT_ADDR_MODE: u32 = 0x8003;

/// D-PDU `CP_TesterPresentHandling` ComParam ID (`PDU_PC_COM` class,
/// Unum32): the master enable switch for periodic/idle-triggered
/// tester-present, independent of `CP_TesterPresentTime`'s own
/// cadence-disable sentinel. `0` = disabled, `1` = enabled (ADR-137).
pub(crate) const CP_TESTER_PRESENT_HANDLING: u32 = 0x8006;

/// D-PDU `CP_TesterPresentSendType` ComParam ID (`PDU_PC_COM` class,
/// Unum32): `0` = periodic (hardware-autonomous `PassThruStartPeriodicMsg`,
/// default), `1` = idle-triggered (software-driven, sent once the bus has
/// been idle for `CP_TesterPresentTime`, reset by any bus activity;
/// ADR-083).
pub(crate) const CP_TESTER_PRESENT_SEND_TYPE: u32 = 0x8008;

/// Service-only `CP_TesterPresentReqRsp` ComParam ID (`PDU_PC_COM` class,
/// Unum32): `0` = no ECU response is returned for a tester-present message
/// (default), `1` = a response is expected and is discarded by this module
/// rather than delivered to the client (ADR-088).
pub(crate) const CP_TESTER_PRESENT_REQ_RSP: u32 = 0x8007;

/// Service-only `CP_TesterPresentExpPosResp` ComParam ID (`PDU_PC_COM` class,
/// Bytefield): the expected positive-response byte prefix (e.g. UDS `0x7E`)
/// used to recognize -- and, when `CP_TesterPresentReqRsp == 1`, discard --
/// the ECU's tester-present reply (ADR-088).
pub(crate) const CP_TESTER_PRESENT_EXP_POS_RESP: u32 = 0x8004;

/// Service-only `CP_TesterPresentExpNegResp` ComParam ID (`PDU_PC_COM` class,
/// Bytefield): same role as `CP_TESTER_PRESENT_EXP_POS_RESP` for the
/// negative-response case (e.g. UDS `0x7F 0x3E`, ADR-088).
pub(crate) const CP_TESTER_PRESENT_EXP_NEG_RESP: u32 = 0x8005;

/// D-PDU `CP_StartMsgIndEnable` ComParam ID (`PDU_PC_COM` class, Unum32):
/// `0` = no SOM indication `ResultData` item is generated for this CLL
/// (default for every protocol), `1` = enabled (ADR-151).
pub(crate) const CP_START_MSG_IND_ENABLE: u32 = 0x8035;

/// D-PDU `CP_TransmitIndEnable` ComParam ID (`PDU_PC_COM` class, Unum32):
/// `0` = no TxDone indication `ResultData` item is generated for this CLL
/// (default for every protocol), `1` = enabled (ADR-151).
pub(crate) const CP_TRANSMIT_IND_ENABLE: u32 = 0x8036;

/// D-PDU `CP_InitializationSettings` ComParam ID (`PDU_PC_COM` class,
/// Unum32, ISO9141/ISO14230): `1` = 5-baud init, `2` = fast-init, `3` = no
/// init sequence (ADR-074). Drives `CoptStartcomm`'s K-line init selection;
/// absent from the bound ComParam set falls back to the legacy heuristic.
pub(crate) const CP_INIT_SETTINGS: u32 = 0x8090;

/// D-PDU `CP_5BaudAddressFunc` ComParam ID (`PDU_PC_COM` class, Unum32,
/// ISO9141/ISO14230): the functional-addressing target address byte for the
/// spec-mandated 5-baud init contract (`CP_InitializationSettings == 1`),
/// used when `CP_RequestAddrMode == 2` (ADR-076). Default `0x33` when unset.
pub(crate) const CP_5BAUD_ADDR_FUNC: u32 = 0x807F;

/// D-PDU `CP_5BaudAddressPhys` ComParam ID (`PDU_PC_COM` class, Unum32,
/// ISO9141/ISO14230): the physical-addressing target address byte for the
/// spec-mandated 5-baud init contract, used whenever `CP_RequestAddrMode` is
/// not `2` (ADR-076). Default `0x01` when unset.
pub(crate) const CP_5BAUD_ADDR_PHYS: u32 = 0x8080;

/// D-PDU `CP_CANFDBaudrate` ComParam ID (`PDU_PC_COM` class, Unum32, CAN
/// family): the CAN FD data-phase baud rate. Together with
/// `CP_CANFDTxMaxDataLength`, drives SAE J2534-2 clause 21 CAN FD's
/// connect-time protocol substitution trigger (ADR-158): `TX_DL > 8 ||
/// CP_CANFDBaudrate != 0`.
pub(crate) const CP_CANFD_BAUDRATE: u32 = 0x80AA;

/// D-PDU `CP_CANFDTxMaxDataLength` ComParam ID (`PDU_PC_COM` class, Unum32,
/// CAN family): the CAN FD maximum TX data length in bytes. See
/// [`CP_CANFD_BAUDRATE`]'s doc comment for the ADR-158 FD-mode trigger this
/// feeds.
pub(crate) const CP_CANFD_TX_MAX_DATA_LENGTH: u32 = 0x80BA;

/// D-PDU `CP_CanFillerByte` ComParam ID (`PDU_PC_COM` class, Unum32, CAN
/// family): the byte value used to pad a message up to a hardware-accepted
/// length. ADR-158 correction (Codex review PR #30 round 4) reuses this for
/// CAN FD TX padding up to the nearest SAE J2534-2 Table 91 DLC-encoded
/// length, the same ComParam ISO15765 frame padding already used.
pub(crate) const CP_CAN_FILLER_BYTE: u32 = 0x806D;

/// D-PDU `CP_Cr` ComParam ID (`PDU_PC_COM` class, Unum32, ISO15765): the
/// consecutive-frame timeout, microsecond resolution. ADR-159/Phase 3b
/// forwards this to the native `CONFIG_N_CR_MAX` SET_CONFIG target on an
/// `FD_ISO15765_PS` link (ms resolution, `us_to_ms` conversion).
pub(crate) const CP_N_CR: u32 = 0x8048;

/// Project-minted `CP_AnalogSampleRate` ComParam ID (`PDU_PC_COM` class,
/// Unum32, ADR-178): SAE J2534-2 clause 10 Analog Inputs' acquisition sample
/// rate, staged via `SetComParam` and resolved at `ConnectComLogicalLink`
/// time -- replaces the removed `CreateComLogicalLinkRequest.analog_sample_rate`
/// request field. No ISO 22900-2 source; the next free service-level id
/// after `CP_SCIEcuSimulator` (`0x80C3`).
pub(crate) const CP_ANALOG_SAMPLE_RATE: u32 = 0x80C4;

/// Project-minted Analog Inputs remaining-parameter ComParam ids (ADR-216):
/// the four writable ones, then the three genuinely read-only capability
/// ones. Minted at the next free `0x8000`-range ids after `CP_NdisPinOption`
/// (`0x80D2`).
pub(crate) const CP_ANALOG_ACTIVE_CHANNELS: u32 = 0x80D3;
pub(crate) const CP_ANALOG_SAMPLES_PER_READING: u32 = 0x80D4;
pub(crate) const CP_ANALOG_READINGS_PER_MSG: u32 = 0x80D5;
pub(crate) const CP_ANALOG_AVERAGING_METHOD: u32 = 0x80D6;
pub(crate) const CP_ANALOG_SAMPLE_RESOLUTION: u32 = 0x80D7;
pub(crate) const CP_ANALOG_INPUT_RANGE_LOW: u32 = 0x80D8;
pub(crate) const CP_ANALOG_INPUT_RANGE_HIGH: u32 = 0x80D9;

/// Project-minted UART Echo Byte timing ComParam ids (ADR-216), native
/// whole-millisecond values -- `0x80DA`-`0x80E3`. Only `CP_UEB_T1_MAX` is
/// currently exercised by a test (`uart_echo_byte.rs`'s round-trip/range/
/// `_CHx`-forwarding coverage is representative of all ten, mirroring this
/// file's own convention of predefining a full sibling-id set even when not
/// every one is individually exercised, e.g. the `PARAM_TP20_*` group above)
/// -- `#[allow(dead_code)]` so a `-D warnings` clippy run doesn't fail on the
/// other nine.
#[allow(dead_code)]
pub(crate) const CP_UEB_T0_MIN: u32 = 0x80DA;
pub(crate) const CP_UEB_T1_MAX: u32 = 0x80DB;
#[allow(dead_code)]
pub(crate) const CP_UEB_T2_MAX: u32 = 0x80DC;
#[allow(dead_code)]
pub(crate) const CP_UEB_T3_MAX: u32 = 0x80DD;
#[allow(dead_code)]
pub(crate) const CP_UEB_T4_MIN: u32 = 0x80DE;
#[allow(dead_code)]
pub(crate) const CP_UEB_T5_MAX: u32 = 0x80DF;
#[allow(dead_code)]
pub(crate) const CP_UEB_T6_MAX: u32 = 0x80E0;
#[allow(dead_code)]
pub(crate) const CP_UEB_T7_MIN: u32 = 0x80E1;
#[allow(dead_code)]
pub(crate) const CP_UEB_T7_MAX: u32 = 0x80E2;
#[allow(dead_code)]
pub(crate) const CP_UEB_T9_MIN: u32 = 0x80E3;

/// Fallible counterpart to [`set_com_param_unum32`] -- returns the raw
/// `Result` instead of panicking on failure, for a test that expects
/// `SetComParam` to be rejected (ADR-216's range/read-only validation tests).
pub(crate) async fn try_set_com_param_unum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
    value: u32,
) -> Result<(), tonic::Status> {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(value)),
            }),
        })
        .await
        .map(|_| ())
}

/// Fallible `SetComParam` sending the `Snum32` oneof variant -- for a test
/// confirming a client that round-trips a `GetComParam(CP_AnalogInputRangeLow/
/// High)` response (reported via `Snum32`, ADR-216 Decision item 8) straight
/// back through `SetComParam` still hits the read-only rejection, not just a
/// client that sends `Unum32` instead (edge-case-hunter finding, round 2
/// correction).
pub(crate) async fn try_set_com_param_snum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
    value: i32,
) -> Result<(), tonic::Status> {
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Snum32(value)),
            }),
        })
        .await
        .map(|_| ())
}

/// Reads a single Snum32 ComParam from `cll_handle`'s Working set via
/// `GetComParam` -- the signed counterpart to [`get_com_param_unum32`], for
/// `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh` (ADR-216 Decision item
/// 8, the first signed-value-reporting ComParams in this codebase).
pub(crate) async fn get_com_param_snum32(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
) -> i32 {
    let response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(vci_service_interface::get_com_param_request::Param::ParamId(com_param_id)),
        })
        .await
        .unwrap_or_else(|err| panic!("get_com_param({com_param_id:#x}) should succeed: {err}"))
        .into_inner();
    match response.param_item.and_then(|p| p.param_data) {
        Some(param_item::ParamData::Snum32(v)) => v,
        other => {
            panic!("get_com_param({com_param_id:#x}) returned unexpected param_data: {other:?}")
        }
    }
}

/// Baud rate the mock's `IOCTL_FIVE_BAUD_INIT` simulates as "negotiated"
/// during the 5-baud init sequence -- mirrors `j2534-0404-mock`'s private
/// `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` (ADR-076); a subsequent
/// `GetComParam(CP_Baudrate)` should return this value after a successful
/// 5-baud init.
pub(crate) const MOCK_FIVE_BAUD_NEGOTIATED_BAUD: u32 = 10_400;

/// Table B.13 `CP_Can*Format` bit values used by the tests below.
pub(crate) mod can_id_format {
    /// Bit 2 (USDT) only, bit 0 (Flow Control) cleared: normal 11-bit
    /// addressing with flow control disabled.
    pub const NORMAL_FC_DISABLED: u32 = 0b0000_0100;
    /// Bit 3 (extended addressing) + bit 2 (USDT) + bit 1 (29-bit CAN Id) +
    /// bit 0 (flow control enabled).
    pub const EXTENDED_29BIT_FC_ENABLED: u32 = 0b0000_1111;
    /// Bit 3 (extended addressing) only, on an 11-bit CAN Id — the only bit
    /// `isotp::Addressing::from_format` consults (ADR-046 addendum).
    pub const EXTENDED_11BIT: u32 = 0b0000_1000;
}

pub(crate) fn unum32_param(com_param_id: u32, value: u32) -> ParamItem {
    ParamItem {
        id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
        com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    }
}

/// Builds one UniqueRespIdTable entry from an identifier and its
/// `PDU_PC_UNIQUE_ID`-class addressing params (see [`unum32_param`]).
pub(crate) fn ecu_entry(unique_resp_identifier: u32, params: Vec<ParamItem>) -> EcuUniqueRespData {
    EcuUniqueRespData {
        unique_resp_identifier,
        params,
    }
}

/// Packs a SAE J2534-2 clause 14 Repeat Messaging `PDU_IOCTL_START_
/// REPEAT_MESSAGE` setup into ADR-178's `DataItem.bytearray_data` byte
/// layout: `u32 time_interval`, `u32 condition`, then three
/// length-prefixed byte spans (`u32 len` + that many bytes) for
/// `repeat_msg_data`/`mask_data`/`pattern_data`, then `u32
/// tx_flag_bits_count` followed by that many `u32`s (each a `TxFlagBit`
/// enum's raw wire value), all little-endian. Callers wrap the returned
/// bytes in `DataItem { data: Some(data_item::Data::BytearrayData(IoBytearray
/// { data })) }`.
///
/// A small local duplicate of `j2534-0404-service/src/service/rpc_misc.rs`'s
/// private `pack_repeat_message_setup` (not reachable from this
/// integration-test binary, matching PR #2's own
/// `parse_device_config_list_output` precedent) -- shared here rather than
/// duplicated per file since several test modules
/// (`repeat_message.rs`/`uart_echo_byte.rs`/`honda_diagh.rs`/`j1708.rs`/
/// `analog_inputs.rs`) each need to build this same payload shape.
pub(crate) fn pack_repeat_message_setup(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: &[u8],
    mask_data: &[u8],
    pattern_data: &[u8],
    tx_flag_bits: &[i32],
) -> Vec<u8> {
    pack_repeat_message_setup_with_response_tx_flag_bits(
        time_interval,
        condition,
        repeat_msg_data,
        mask_data,
        pattern_data,
        tx_flag_bits,
        None,
    )
}

/// Like [`pack_repeat_message_setup`], but also encodes ADR-214's optional
/// v2 trailing section (`u32 response_tx_flag_bits_count` + that many
/// `u32`s) when `response_tx_flag_bits` is `Some` -- the mask/pattern
/// response template's own, independent addressing basis. `None` writes
/// nothing further, producing the byte-for-byte identical v1 shape
/// [`pack_repeat_message_setup`] itself always produces (which delegates to
/// this function with `None`, exactly mirroring
/// `rpc_misc.rs`'s own `unpack_repeat_message_setup`/
/// `RepeatMessageSetup::response_tx_flag_bits` shape).
pub(crate) fn pack_repeat_message_setup_with_response_tx_flag_bits(
    time_interval: u32,
    condition: u32,
    repeat_msg_data: &[u8],
    mask_data: &[u8],
    pattern_data: &[u8],
    tx_flag_bits: &[i32],
    response_tx_flag_bits: Option<&[i32]>,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(
        8 + 4
            + repeat_msg_data.len()
            + 4
            + mask_data.len()
            + 4
            + pattern_data.len()
            + 4
            + tx_flag_bits.len() * 4
            + response_tx_flag_bits.map_or(0, |bits| 4 + bits.len() * 4),
    );
    bytes.extend_from_slice(&time_interval.to_le_bytes());
    bytes.extend_from_slice(&condition.to_le_bytes());
    for span in [repeat_msg_data, mask_data, pattern_data] {
        bytes.extend_from_slice(&(span.len() as u32).to_le_bytes());
        bytes.extend_from_slice(span);
    }
    bytes.extend_from_slice(&(tx_flag_bits.len() as u32).to_le_bytes());
    for bit in tx_flag_bits {
        bytes.extend_from_slice(&bit.to_le_bytes());
    }
    if let Some(bits) = response_tx_flag_bits {
        bytes.extend_from_slice(&(bits.len() as u32).to_le_bytes());
        for bit in bits {
            bytes.extend_from_slice(&bit.to_le_bytes());
        }
    }
    bytes
}

/// Polls the mock until `channel_id` has at least `count` written messages,
/// or panics after ~2 s.
pub(crate) async fn wait_for_written_count(server: &TestServer, channel_id: u32, count: usize) {
    for _ in 0..200 {
        if server.backdoor.written_count(channel_id) >= count {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    panic!(
        "expected {count} written messages on channel {channel_id}, got {}",
        server.backdoor.written_count(channel_id)
    );
}

/// Polls `MockBackdoor::become_master_hold_engaged` until the currently-armed
/// `IOCTL_BECOME_MASTER` hold (`MockBackdoor::arm_become_master_hold`) has
/// actually been reached, or panics after ~2 s. Deterministic stand-in for a
/// fixed sleep (ADR-149) -- a test needing to know "the BECOME_MASTER call is
/// genuinely in flight" (e.g. before attempting a sibling connect it expects
/// to be rejected) should await this rather than guess a timing margin.
pub(crate) async fn wait_for_become_master_hold_engaged(server: &TestServer) {
    for _ in 0..200 {
        if server.backdoor.become_master_hold_engaged() {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    panic!("IOCTL_BECOME_MASTER hold was never engaged within ~2s");
}

/// Polls (short steps) `MockBackdoor::repeat_message_status` for `msg_id` on
/// `MOCK_CHANNEL_ID` until it reports `expected_status`, or `max_wait_ms`
/// elapses. Returns whether that status was observed within the bound.
/// ADR-173 Decision 3's Table 53 polarity: `1` live, `0`
/// terminated-but-unstopped -- a status this direct, bypass-the-service
/// probe can observe well before any client-visible gRPC signal would.
pub(crate) async fn wait_for_repeat_status(
    server: &TestServer,
    msg_id: u32,
    expected_status: u32,
    max_wait_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);
    while std::time::Instant::now() < deadline {
        if server
            .backdoor
            .repeat_message_status(MOCK_CHANNEL_ID, msg_id)
            == Some(expected_status)
        {
            return true;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    false
}

/// Polls `GetEventItem(cll)` until a `ResultData` event arrives, or panics
/// after ~2 s.
///
/// ADR-115 single-consumer correction: only use this for a poll-only caller
/// with no live `SubscribeEvent` subscription on `cll_handle` at the time
/// the awaited item is produced -- once a subscriber is attached, it IS the
/// queue's drain, so a matching item is delivered live instead of landing in
/// `rx_buf`, and this will spin until its own ~2 s timeout finding nothing.
/// A caller with a live subscription open should read the notification off
/// the stream instead (e.g. via [`wait_for_cop_finished_and_result_data`]
/// or a direct [`wait_for_event`] capture).
pub(crate) async fn wait_for_result_data(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> vci_service_interface::ResultData {
    for _ in 0..200 {
        let response = client
            .get_event_item(GetEventItemRequest {
                handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        if let Some(item) = response.event_item
            && let Some(event_item::Data::ResultData(result)) = item.data
        {
            return result;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    panic!("no ResultData event arrived for the CLL");
}

/// Polls a live `SubscribeEvent` stream for a COP's `PduCopstFinished`
/// status AND its `ResultData` notification, in a single pass that does not
/// assume which arrives first -- panics after `timeout_ms` if either is
/// still missing once the deadline elapses. The live-stream counterpart to
/// [`wait_for_result_data`] (ADR-115 single-consumer correction): once a
/// subscriber is attached, it IS this CLL's event-queue drain, so a result
/// item produced while the subscription is live is delivered on the stream
/// instead of landing in `rx_buf` for `GetEventItem` to find -- and a
/// naive two-call sequence (`wait_for_event` for `Finished`, then a second
/// wait for `ResultData`) can silently discard the result if it happens to
/// arrive first, since `wait_for_event` drops every notification that does
/// not match its own predicate.
pub(crate) async fn wait_for_cop_finished_and_result_data(
    events: &mut tonic::Streaming<EventNotification>,
    timeout_ms: u64,
) -> vci_service_interface::ResultData {
    let mut finished = false;
    let mut result = None;
    assert!(
        wait_for_event(events, timeout_ms, |item| {
            match &item.data {
                Some(event_item::Data::ResultData(r)) => result = Some(r.clone()),
                Some(event_item::Data::CopStatus(status))
                    if *status
                        == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished
                            as i32 =>
                {
                    finished = true;
                }
                _ => {}
            }
            finished && result.is_some()
        })
        .await,
        "CoptStartcomm should finish and deliver its ResultData on the live SubscribeEvent \
         stream (finished={finished}, result_data_seen={})",
        result.is_some()
    );
    result.expect("captured by the predicate above")
}

/// ADR-051: asserts `result.data_bytes` equals `payload` and, when `header`
/// or `footer` is non-empty, that `result.extra_info.header_bytes`/
/// `footer_bytes` equal them — the CAN ID (CAN/ISO15765), KWP2000
/// format/address/length bytes (ISO9141/ISO14230) plus checksum, or J1850
/// format/target/source plus CRC split out of the raw frame. Pass empty
/// `header`/`footer` for a protocol this split does not apply to (SCI), or
/// a frame with no footer present, where the corresponding half of
/// `extra_info` is absent (and `extra_info` itself is `None` when both are
/// empty) and `data_bytes` carries the frame as-is.
pub(crate) fn assert_result_data(
    result: &vci_service_interface::ResultData,
    header: &[u8],
    footer: &[u8],
    payload: &[u8],
) {
    assert_eq!(result.data_bytes, payload);
    // ADR-098: `rx_flag` only ever carries a non-empty value when at least
    // one of the 5 low RxStatus bits (TX_MSG_TYPE, START_OF_MESSAGE,
    // RX_BREAK, TX_INDICATION, ISO15765_PADDING_ERROR) is set (see
    // `assert_result_data_with_rx_flag`); every other frame -- which is
    // every frame this helper is used for -- gets `Vec::new()` (no flags
    // asserted).
    assert!(
        result.rx_flag.is_empty(),
        "rx_flag should be empty for a Normal Message frame (ADR-098)"
    );
    if header.is_empty() && footer.is_empty() {
        assert!(result.extra_info.is_none(), "extra_info should be absent");
    } else {
        let extra_info = result
            .extra_info
            .as_ref()
            .expect("extra_info should carry the header/footer");
        assert_eq!(extra_info.header_bytes, header);
        assert_eq!(extra_info.footer_bytes, footer);
    }
}

/// Like `assert_result_data`, but also asserts `result.rx_flag` against an
/// explicit expected value (ADR-098) instead of assuming it must be empty --
/// used by tests for any of the 5 low RxStatus bits, where `rx_flag` is
/// expected to carry a non-empty `[0, 0, 0, <bits>]` encoding.
pub(crate) fn assert_result_data_with_rx_flag(
    result: &vci_service_interface::ResultData,
    header: &[u8],
    footer: &[u8],
    payload: &[u8],
    rx_flag: &[u8],
) {
    assert_eq!(result.data_bytes, payload);
    assert_eq!(result.rx_flag, rx_flag, "rx_flag mismatch (ADR-098)");
    if header.is_empty() && footer.is_empty() {
        assert!(result.extra_info.is_none(), "extra_info should be absent");
    } else {
        let extra_info = result
            .extra_info
            .as_ref()
            .expect("extra_info should carry the header/footer");
        assert_eq!(extra_info.header_bytes, header);
        assert_eq!(extra_info.footer_bytes, footer);
    }
}

/// Configures `cll_handle`'s Working UniqueRespIdTable with the given
/// entries (ADR-068: `SetUniqueRespIdTable` stages Working only -- it no
/// longer takes effect for TX/RX/filters until promoted, either by this
/// CLL's own `ConnectComLogicalLink` or by `CoptUpdateparam`; see
/// `promote_via_update_param`).
pub(crate) async fn set_unique_resp_table(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    unique_data: Vec<EcuUniqueRespData>,
) {
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem { unique_data }),
        })
        .await
        .expect("set_unique_resp_id_table should succeed");
}

/// Reads `cll_handle`'s Working UniqueRespIdTable via `GetUniqueRespIdTable`
/// (always Working, mirroring `GetComParam` -- ADR-068) and returns the raw
/// entry list. In template mode (no table ever set) this returns the single
/// `PDU_ID_UNDEF` template entry (ISO 22900-2 §9.3.3.6), not an empty list.
pub(crate) async fn get_unique_resp_table(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> Vec<EcuUniqueRespData> {
    client
        .get_unique_resp_id_table(GetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("get_unique_resp_id_table should succeed")
        .into_inner()
        .unique_resp_id_table
        .map(|t| t.unique_data)
        .unwrap_or_default()
}

/// Issues `CoptUpdateparam` on `cll_handle` and waits for the poll task to
/// apply it -- promoting both the Working ComParam set and the Working
/// UniqueRespIdTable to Active (ADR-068, mirroring ADR-067's ComParamSet
/// promotion), including reconciling ISO15765 `FLOW_CONTROL_FILTER`s from
/// any table change since the last promotion. Uses a short sleep rather than
/// an event subscription, like `send_data`, since the mock applies
/// `SET_CONFIG`/filter I/O synchronously and near-instantly.
pub(crate) async fn promote_via_update_param(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

/// Like `set_unique_resp_table`, but also promotes the table to Active via
/// `CoptUpdateparam` -- for tests exercising TX/RX/addressing behavior that
/// is not itself about the Working/Active split (ADR-068), and therefore
/// expect a post-connect `SetUniqueRespIdTable` call to take effect
/// immediately, matching this codebase's pre-ADR-068 test conventions.
pub(crate) async fn set_unique_resp_table_and_promote(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    unique_data: Vec<EcuUniqueRespData>,
) {
    set_unique_resp_table(client, cll_handle, unique_data).await;
    promote_via_update_param(client, cll_handle).await;
}

/// Asserts that no `ResultData` event is pending for `cll_handle`, after
/// giving the poll task ~100 ms to fan out anything the caller injected —
/// i.e. UniqueRespIdTable routing dropped the frame(s) for this CLL (ADR-007).
pub(crate) async fn assert_no_result_data(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    context: &str,
) {
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    loop {
        let response = client
            .get_event_item(GetEventItemRequest {
                handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        let Some(item) = response.event_item else {
            return;
        };
        if let Some(event_item::Data::ResultData(result)) = item.data {
            panic!("{context}: unexpected ResultData delivered to the CLL: {result:?}");
        }
    }
}

/// Builds a raw frame in the mock's on-wire layout: 4-byte big-endian CAN ID
/// followed by the payload.
pub(crate) fn can_frame(can_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = can_id.to_be_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}
