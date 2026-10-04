use tonic::{Code, Status};
use tracing::error;
use vci_service_interface::{
    ComPrimitiveHandle, ErrorDetail, ErrorEventData, PduError, PduErrorEvent,
    status_with_error_detail,
};

use crate::service::{DEFAULT_MODULE_HANDLE, TrackedError};

/// Constructs a client-facing [`Status`], logging the full error (which may
/// contain local filesystem paths or raw OS error text, e.g. from a failed
/// library load) server-side only.
pub fn map_construct_error(error: impl std::fmt::Display) -> Status {
    error!(%error, "failed to construct J2534-0404 API");
    Status::internal("failed to construct J2534-0404 API")
}

/// Maps a registry/device-discovery lookup failure to a client-facing
/// [`Status`], logging the full error (which may contain raw OS/registry
/// error text) server-side only.
pub fn map_registry_error(error: j2534_0404_registry::RegistryError) -> Status {
    use j2534_0404_registry::RegistryError;

    match error {
        RegistryError::Io(e) => {
            error!(error = %e, "I/O error while searching for J2534 devices");
            Status::internal("I/O error while searching for J2534 devices")
        }
        RegistryError::RegistryUnsupported => {
            Status::internal("registry lookup is only supported on Windows")
        }
        RegistryError::NotFound(name) => {
            Status::not_found(format!("no J2534 device found with name '{name}'"))
        }
    }
}

/// Maps a native J2534 status code to the closest ISO 22900-2 `PDUError`
/// function-return code it represents.
///
/// This is a best-effort mapping: J2534-1 and ISO 22900-2 are different
/// standards with different granularities, so several native codes fold onto
/// the same `PDUError` (or onto the generic `PDU_ERR_FCT_FAILED` when J2534
/// has no closer analog). Vendor-specific/unrecognized codes also fall back
/// to `PDU_ERR_FCT_FAILED`.
pub(crate) fn pdu_error_for(code: j2534_0404::StatusCode) -> PduError {
    match code.as_u32() {
        j2534_0404::ERR_FAILED => PduError::PduErrFctFailed,
        j2534_0404::ERR_NULL_PARAMETER
        | j2534_0404::ERR_INVALID_FLAGS
        | j2534_0404::ERR_INVALID_MSG
        | j2534_0404::ERR_INVALID_TIME_INTERVAL
        | j2534_0404::ERR_MSG_PROTOCOL_ID
        | j2534_0404::ERR_NOT_UNIQUE => PduError::PduErrInvalidParameters,
        // Device dropped after a successful open, not "never connected" --
        // that case is the adapter's own PDU_ERR_MODULE_NOT_CONNECTED state
        // guard, checked before any native call is attempted.
        j2534_0404::ERR_DEVICE_NOT_CONNECTED => PduError::PduErrCommPcToVciFailed,
        // ISO 22900-2 has no sync function-return timeout code; async
        // timeouts are PDU_ERR_EVT_RX_TIMEOUT on the event queue, unrelated.
        j2534_0404::ERR_TIMEOUT => PduError::PduErrFctFailed,
        j2534_0404::ERR_BUFFER_FULL => PduError::PduErrTxQueueFull,
        j2534_0404::ERR_BUFFER_EMPTY => PduError::PduErrEventQueueEmpty,
        j2534_0404::ERR_CHANNEL_IN_USE => PduError::PduErrResourceBusy,
        // SAE J2534-2 clause 15: pin 9 Short-to-Ground state-conflict codes
        // (same-pin ERR_PIN_IN_USE, cross-pin ERR_VOLTAGE_IN_USE against pin
        // 15) are both "a resource is occupied by a conflicting state",
        // matching the ERR_CHANNEL_IN_USE precedent above. No call site
        // currently reaches this generic table for these two codes --
        // `ioctl_set_prog_voltage` uses `map_native_error_as` with an
        // explicit override (A2-21/Phase 13) -- but this keeps the generic
        // mapping correct for any other/future call site.
        j2534_0404::ERR_PIN_IN_USE | j2534_0404::ERR_VOLTAGE_IN_USE => PduError::PduErrResourceBusy,
        j2534_0404::ERR_DEVICE_IN_USE => PduError::PduErrSharingViolation,
        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
        // `ERR_NO_CONNECTION_ESTABLISHED` is deliberately NOT mapped here.
        // This native code already has a different, unrelated meaning at the
        // mock's TP2.0 oversized-write path (`j2534-0404-mock/src/lib.rs`,
        // ADR-188), which reaches this generic table via
        // `rpc_primitive.rs`'s `map_native_error_for_link` call sites -- a
        // global entry here would silently repurpose that path's mapping
        // too. The Ethernet_NDIS `PassThruConnect` call site
        // (`rpc_link.rs`'s `connect_new_physical_channel`) maps this code to
        // `PDU_ERR_NO_CABLE_DETECTED` itself, scoped to that one call site,
        // via `map_native_error_as` with an explicit override (mirroring
        // `ioctl_set_prog_voltage`'s own A2-21 precedent). Every other call
        // site (including the TP2.0 write path above) falls through to the
        // generic `PDU_ERR_FCT_FAILED` catch-all below, unchanged from
        // before this phase.
        // J2534's ERR_NOT_SUPPORTED is function-scoped, ISO 22900-2's
        // PDU_ERR_ID_NOT_SUPPORTED is id-scoped -- not the same thing.
        j2534_0404::ERR_NOT_SUPPORTED => PduError::PduErrFctFailed,
        // SAE J2534-2 clause 18 (ADR-176/Phase 14): a GET/SET_DEVICE_CONFIG
        // parameter_id outside the ten valid NON_VOLATILE_STORE_x slots --
        // the same "unrecognized id for an otherwise-understood request"
        // category as ERR_INVALID_IOCTL_ID/ERR_INVALID_PROTOCOL_ID below.
        j2534_0404::ERR_INVALID_IOCTL_ID
        | j2534_0404::ERR_INVALID_PROTOCOL_ID
        | j2534_0404::ERR_INVALID_IOCTL_PARAM_ID => PduError::PduErrIdNotSupported,
        j2534_0404::ERR_EXCEEDED_LIMIT | j2534_0404::ERR_BUFFER_OVERFLOW => {
            PduError::PduErrResourceError
        }
        j2534_0404::ERR_PIN_INVALID => PduError::PduErrPinNotConnected,
        j2534_0404::ERR_INVALID_IOCTL_VALUE | j2534_0404::ERR_INVALID_BAUDRATE => {
            PduError::PduErrValueNotSupported
        }
        j2534_0404::ERR_INVALID_CHANNEL_ID
        | j2534_0404::ERR_INVALID_DEVICE_ID
        | j2534_0404::ERR_INVALID_MSG_ID
        | j2534_0404::ERR_INVALID_FILTER_ID => PduError::PduErrInvalidHandle,
        // Escaping to the client would indicate an adapter filter-management
        // bug per ADR-039/048, not a client-actionable condition.
        j2534_0404::ERR_NO_FLOW_CONTROL => PduError::PduErrFctFailed,
        _ => PduError::PduErrFctFailed,
    }
}

fn native_pdu_error(error: &j2534_0404::Error) -> PduError {
    match error {
        j2534_0404::Error::ApiStatus { code, .. } => pdu_error_for(*code),
        _ => PduError::PduErrFctFailed,
    }
}

/// Builds an [`ErrorEventData`] from an already-tracked CLL/module
/// [`TrackedError`], if one is set (and its event isn't `PDU_ERR_EVT_NOERROR`
/// -- not a real tracked error). `last_error` should be the value already
/// read from `LogicalLinkState::last_error` or `ModuleState::last_error` --
/// this never makes a native call to fetch it. Reports the real time the
/// error was recorded, not the time of the RPC surfacing it.
fn error_event_data_for(last_error: Option<TrackedError>) -> Option<ErrorEventData> {
    let tracked = last_error.filter(|t| t.event != PduErrorEvent::PduErrEvtNoerror)?;
    Some(ErrorEventData {
        error_event: tracked.event as i32,
        cop_handle: tracked.cop.map(|c| ComPrimitiveHandle {
            module_handle: DEFAULT_MODULE_HANDLE,
            cll_handle: c.cll_handle,
            cop_handle: c.cop_handle,
        }),
        timestamp: tracked.timestamp,
        extra_error_info: 0,
    })
}

/// Maps a failing native J2534 call to a client-facing [`Status`] carrying an
/// [`ErrorDetail`], additionally attaching `error_event_data` from
/// `last_error` -- the already-tracked `LogicalLinkState::last_error` /
/// `ModuleState::last_error` for the Module/CLL handle in scope at the
/// failure site, if any (pass `None` when no handle is in scope). `context`
/// is prepended to the native error's `Display` text, matching this crate's
/// pre-existing `"{context} failed: {err}"`-style messages so client-visible
/// text does not regress.
pub(crate) fn map_native_error_for_link(
    context: &str,
    error: &j2534_0404::Error,
    last_error: Option<TrackedError>,
) -> Status {
    map_native_error_as(context, error, native_pdu_error(error), last_error)
}

/// Same as [`map_native_error_for_link`], but overrides the `PDUError` this
/// native failure maps to instead of deriving it from the `StatusCode` via
/// [`pdu_error_for`]. Most call sites use this for IOCTLs where any native
/// failure means one specific thing regardless of the raw status code (e.g.
/// `PDU_IOCTL_STOP_MSG_FILTER`/`PDU_IOCTL_CLEAR_MSG_FILTER` failing to stop an
/// underlying filter always maps to `PDU_ERR_FCT_FAILED`). `PDU_IOCTL_SET_PROG_VOLTAGE`
/// is a partial exception (A2-21): it still calls through here, but its call
/// site picks between `PDU_ERR_VOLTAGE_NOT_SUPPORTED` and
/// `PDU_ERR_MUX_RSC_NOT_SUPPORTED` based on the native status code before
/// invoking this function, since ISO 22900-2:2009 Table 49 distinguishes an
/// unsupported voltage from an invalid pin/resource.
///
/// The outer gRPC code is `Code::Internal` for every `PDUError` except
/// `PDU_ERR_INVALID_HANDLE` (A3-4): that one specifically means the
/// referenced handle does not resolve to anything, the same condition
/// [`unknown_handle_status`] reports for this adapter's own (emulated)
/// handle checks -- so both paths use `Code::NotFound`, keeping the outer
/// code consistent for one `PDUError` regardless of whether the native
/// driver or this adapter's own state caught the invalid reference first.
pub(crate) fn map_native_error_as(
    context: &str,
    error: &j2534_0404::Error,
    pdu_error: PduError,
    last_error: Option<TrackedError>,
) -> Status {
    let detail = ErrorDetail {
        pdu_error: pdu_error as i32,
        error_event_data: error_event_data_for(last_error),
        detail_text: Some(format!("{context}: {error}")),
    };
    let code = if pdu_error == PduError::PduErrInvalidHandle {
        Code::NotFound
    } else {
        Code::Internal
    };
    status_with_error_detail(code, format!("{context} failed: {error}"), detail)
}

/// Builds a `NotFound` [`Status`] carrying a `PDU_ERR_INVALID_HANDLE`
/// [`ErrorDetail`] for a reference to a Module/CLL/COP handle this service
/// does not recognize. `message` is used verbatim as the client-visible
/// [`Status`] message, matching this crate's pre-existing "unknown ...
/// handle" messages so client-visible text does not regress.
pub(crate) fn unknown_handle_status(message: impl Into<String>) -> Status {
    let detail = ErrorDetail {
        pdu_error: PduError::PduErrInvalidHandle as i32,
        error_event_data: None,
        detail_text: None,
    };
    status_with_error_detail(Code::NotFound, message, detail)
}

/// Builds a [`Status`] for an emulated D-PDU-level state-guard rejection
/// (CLL not connected/started, a physical resource lock held by another
/// CLL, a TX-queue-busy/queue-full condition, ...), carrying `pdu_error` as
/// an [`ErrorDetail`]. `code` and `message` are unchanged from this crate's
/// pre-existing call sites -- only the `ErrorDetail` is new. `last_error`
/// should be the value already read from `LogicalLinkState::last_error` /
/// `ModuleState::last_error` for the Module/CLL handle in scope at the
/// guard site, if any (pass `None` when the guard concerns a handle this
/// service does not recognize as a real Module/CLL) -- same contract as
/// [`map_native_error_for_link`], and read from the SAME lock acquisition
/// the guard decision itself was made from, to avoid the stale-snapshot
/// race fixed for native-call failures.
pub(crate) fn state_guard_status(
    code: Code,
    message: impl Into<String>,
    pdu_error: PduError,
    last_error: Option<TrackedError>,
) -> Status {
    let detail = ErrorDetail {
        pdu_error: pdu_error as i32,
        error_event_data: error_event_data_for(last_error),
        detail_text: None,
    };
    status_with_error_detail(code, message, detail)
}

/// Shared `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted` rejection
/// for the SAE J2534-2 clause 11 GM UART shared-physical-channel
/// precautions (ADR-189/Phase 8, Codex review P2 fix, PR #98):
/// `rpc_misc.rs`'s `ioctl_set_poll_response`/`ioctl_become_master` own
/// `ref_count != 1` cases (and `ioctl_become_master`'s new
/// `become_master_in_flight`-already-set case), plus `rpc_link.rs`'s own
/// join-side rejection of a connect attempt landing while a
/// `become_master_in_flight` bid is outstanding. Both IOCTLs used to
/// silently `Ok(())` no-op instead of rejecting when the channel was
/// already shared -- a lie, since the caller sees the same success
/// response as an IOCTL that actually ran. Mirrors the exact `PduError`/
/// `Code` pairing this crate's other shared-physical-channel/lock
/// rejections already use for the identical resource (e.g. `rpc_link.rs`'s
/// `dead`-channel and filter-conflict join rejections, `rpc_misc.rs`'s own
/// `LOCK_PHYSICAL_TX_QUEUE` rejection).
pub(crate) fn gm_uart_shared_channel_locked_status(
    ioctl_name: &str,
    reason: &str,
    last_error: Option<TrackedError>,
) -> Status {
    state_guard_status(
        Code::ResourceExhausted,
        format!("PDU_ERR_RSC_LOCKED_BY_OTHER_CLL: {ioctl_name} cannot proceed -- {reason}"),
        PduError::PduErrRscLockedByOtherCll,
        last_error,
    )
}

#[cfg(test)]
mod tests {
    use vci_service_interface::error_detail_from_status;

    use super::*;

    #[test]
    fn pdu_error_for_maps_representative_status_codes() {
        assert_eq!(
            pdu_error_for(j2534_0404::StatusCode(j2534_0404::ERR_BUFFER_FULL)),
            PduError::PduErrTxQueueFull
        );
        assert_eq!(
            pdu_error_for(j2534_0404::StatusCode(j2534_0404::ERR_INVALID_CHANNEL_ID)),
            PduError::PduErrInvalidHandle
        );
        assert_eq!(
            pdu_error_for(j2534_0404::StatusCode(j2534_0404::ERR_DEVICE_NOT_CONNECTED)),
            PduError::PduErrCommPcToVciFailed
        );
        // ADR-194/Phase 16: `ERR_NO_CONNECTION_ESTABLISHED` is deliberately
        // NOT given a global mapping here -- it already has a different,
        // unrelated meaning at the mock's TP2.0 oversized-write path
        // (ADR-188), which reaches this generic table too. It falls through
        // to the generic PDU_ERR_FCT_FAILED catch-all here; the Ethernet_NDIS
        // `PassThruConnect` call site maps it to PDU_ERR_NO_CABLE_DETECTED
        // itself, scoped to that one call site via `map_native_error_as`
        // (see `rpc_link.rs`'s `connect_new_physical_channel`).
        assert_eq!(
            pdu_error_for(j2534_0404::StatusCode(
                j2534_0404::ERR_NO_CONNECTION_ESTABLISHED
            )),
            PduError::PduErrFctFailed
        );
        // Vendor-specific/unrecognized code falls back to PDU_ERR_FCT_FAILED.
        assert_eq!(
            pdu_error_for(j2534_0404::StatusCode(0xDEAD_BEEF)),
            PduError::PduErrFctFailed
        );
    }

    #[test]
    fn map_native_error_for_link_attaches_error_event_data_only_when_a_real_error_is_tracked() {
        let native = j2534_0404::Error::ApiStatus {
            code: j2534_0404::StatusCode(j2534_0404::ERR_BUFFER_FULL),
            description: None,
        };

        let with_tracked_error = map_native_error_for_link(
            "PassThruIoctl CLEAR_RX_BUFFER",
            &native,
            Some(TrackedError {
                event: PduErrorEvent::PduErrEvtLostCommToVci,
                timestamp: 12_345,
                cop: None,
                cop_tag: None,
            }),
        );
        assert_eq!(with_tracked_error.code(), Code::Internal);
        assert!(
            with_tracked_error
                .message()
                .contains("PassThruIoctl CLEAR_RX_BUFFER failed")
        );
        let detail =
            error_detail_from_status(&with_tracked_error).expect("ErrorDetail should be attached");
        assert_eq!(detail.pdu_error, PduError::PduErrTxQueueFull as i32);
        let event_data = detail.error_event_data.expect("event data should be set");
        assert_eq!(
            event_data.error_event,
            PduErrorEvent::PduErrEvtLostCommToVci as i32
        );
        assert_eq!(
            event_data.timestamp, 12_345,
            "the real time the error was recorded should be reported, not a placeholder"
        );
        assert!(
            event_data.cop_handle.is_none(),
            "a module/CLL-scoped tracked error (cop: None, PDU_HANDLE_UNDEF) must not \
             fabricate a ComPrimitiveHandle"
        );

        let with_noerror_tracked = map_native_error_for_link(
            "PassThruIoctl CLEAR_RX_BUFFER",
            &native,
            Some(TrackedError {
                event: PduErrorEvent::PduErrEvtNoerror,
                timestamp: 12_345,
                cop: None,
                cop_tag: None,
            }),
        );
        assert!(
            error_detail_from_status(&with_noerror_tracked)
                .unwrap()
                .error_event_data
                .is_none(),
            "PDU_ERR_EVT_NOERROR is not a real tracked error and should not be surfaced"
        );

        let with_no_context = map_native_error_for_link("PassThruOpen", &native, None);
        assert!(
            error_detail_from_status(&with_no_context)
                .unwrap()
                .error_event_data
                .is_none()
        );
    }

    /// A2-1 (ISO 22900-2 §9.4.7 c) / §9.6.2): a `TrackedError` recorded with
    /// a `cop` attribution (e.g. an N_Bs-style receive timeout that hit a
    /// specific executing `CoptSendrecv`) must surface that same
    /// `(cll_handle, cop_handle)` pair through the RPC-fallback
    /// `ErrorDetail.error_event_data.cop_handle`, not just through the async
    /// event queue/`SubscribeEvent` path -- this is `error_event_data_for`'s
    /// only other caller besides the queued-event constructors in
    /// `service/events.rs` and `service/rpc_primitive.rs`, both covered by
    /// `grpc_mock` integration tests.
    #[test]
    fn map_native_error_for_link_propagates_cop_handle_when_tracked_error_is_cop_scoped() {
        use vci_service_interface::ComPrimitiveHandle;

        let native = j2534_0404::Error::ApiStatus {
            code: j2534_0404::StatusCode(j2534_0404::ERR_BUFFER_FULL),
            description: None,
        };

        let status = map_native_error_for_link(
            "PassThruIoctl CLEAR_RX_BUFFER",
            &native,
            Some(TrackedError {
                event: PduErrorEvent::PduErrEvtRxTimeout,
                timestamp: 99,
                cop: Some(crate::service::CopRef {
                    cll_handle: 7,
                    cop_handle: 42,
                }),
                cop_tag: None,
            }),
        );
        let detail = error_detail_from_status(&status).expect("ErrorDetail should be attached");
        let event_data = detail.error_event_data.expect("event data should be set");
        assert_eq!(
            event_data.cop_handle,
            Some(ComPrimitiveHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: 7,
                cop_handle: 42,
            }),
            "a COP-scoped tracked error must surface the same COP's handle through the \
             RPC-fallback path, not just the async event queue"
        );
    }

    #[test]
    fn map_native_error_as_overrides_the_derived_pdu_error() {
        let native = j2534_0404::Error::ApiStatus {
            code: j2534_0404::StatusCode(j2534_0404::ERR_FAILED),
            description: None,
        };
        let status = map_native_error_as(
            "PDU_ERR_VOLTAGE_NOT_SUPPORTED: PassThruSetProgrammingVoltage",
            &native,
            PduError::PduErrVoltageNotSupported,
            None,
        );
        assert_eq!(
            status.code(),
            Code::Internal,
            "every PDUError other than PduErrInvalidHandle keeps the blanket Internal code"
        );
        let detail = error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrVoltageNotSupported as i32);
    }

    /// A3-4: a native call failure that maps to `PDU_ERR_INVALID_HANDLE`
    /// (e.g. the driver itself rejects a channel/device/msg/filter id) must
    /// carry the same outer gRPC code (`NotFound`) as `unknown_handle_status`
    /// -- this adapter's own emulated "handle not recognized" rejection --
    /// rather than the blanket `Internal` every other native failure gets.
    #[test]
    fn map_native_error_for_link_uses_not_found_for_invalid_handle() {
        let native = j2534_0404::Error::ApiStatus {
            code: j2534_0404::StatusCode(j2534_0404::ERR_INVALID_CHANNEL_ID),
            description: None,
        };
        let status = map_native_error_for_link("PassThruDisconnect", &native, None);
        assert_eq!(status.code(), Code::NotFound);
        let detail = error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrInvalidHandle as i32);
    }

    #[test]
    fn unknown_handle_status_is_not_found_with_invalid_handle() {
        let status = unknown_handle_status("unknown cll_handle 7");
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "unknown cll_handle 7");
        let detail = error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrInvalidHandle as i32);
        assert!(detail.error_event_data.is_none());
    }

    #[test]
    fn state_guard_status_carries_the_given_code_and_pdu_error() {
        let status = state_guard_status(
            Code::ResourceExhausted,
            "physical ComParam lock is held by another ComLogicalLink on this resource",
            PduError::PduErrRscLockedByOtherCll,
            None,
        );
        assert_eq!(status.code(), Code::ResourceExhausted);
        let detail = error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrRscLockedByOtherCll as i32);
        assert!(detail.error_event_data.is_none());
    }

    #[test]
    fn state_guard_status_attaches_a_tracked_error_when_given_one() {
        let status = state_guard_status(
            Code::FailedPrecondition,
            "logical link must be connected before starting a primitive",
            PduError::PduErrCllNotConnected,
            Some(TrackedError {
                event: PduErrorEvent::PduErrEvtLostCommToVci,
                timestamp: 42,
                cop: None,
                cop_tag: None,
            }),
        );
        let detail = error_detail_from_status(&status).unwrap();
        let event_data = detail.error_event_data.expect("event data should be set");
        assert_eq!(
            event_data.error_event,
            PduErrorEvent::PduErrEvtLostCommToVci as i32
        );
        assert_eq!(event_data.timestamp, 42);
    }
}
