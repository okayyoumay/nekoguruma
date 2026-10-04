use iso22900::{ComLogicalLinkHandle, DPduApi, DPduApiError, LastErrorInfo, ModuleHandle};
use tonic::{Code, Status};
use tracing::{error, warn};
use vci_service_interface::{
    ErrorDetail, ErrorEventData, PduError, PduErrorEvent, status_with_error_detail,
};

/// Logs the full error server-side only (see ADR-089) and returns a generic,
/// fixed message safe to return to a client -- no interpolated path or raw
/// OS error text.
fn sanitized_message(context: &str, error: impl std::fmt::Display) -> String {
    error!(%error, "{context}");
    context.to_string()
}

/// Constructs a client-facing [`Status`] for an internal-error category,
/// logging the full error (which may contain local filesystem paths or raw
/// OS error text, e.g. from a failed library load) server-side only.
fn sanitized_internal(context: &str, error: impl std::fmt::Display) -> Status {
    Status::internal(sanitized_message(context, error))
}

pub fn map_construct_error(error: DPduApiError) -> Status {
    sanitized_internal("failed to construct ISO22900 API", error)
}

/// Builds a `FailedPrecondition` [`Status`] for a link-scoped RPC called
/// before `ModuleConnect` or after the API has been torn down. Carries a
/// `PDU_ERR_PDUAPI_NOT_CONSTRUCTED` [`ErrorDetail`] -- this is a real ISO
/// 22900-2 state, not request-shape validation, and with `GetLastError`
/// removed the client otherwise has no structured way to identify it
/// (ADR-105). `message` is used verbatim as the client-visible [`Status`]
/// message, matching this crate's pre-existing `API_NOT_INITIALIZED` text so
/// client-visible text does not regress.
pub(crate) fn api_not_initialized_status(message: impl Into<String>) -> Status {
    let detail = ErrorDetail {
        pdu_error: PduError::PduErrPduapiNotConstructed as i32,
        error_event_data: None,
        detail_text: None,
    };
    status_with_error_detail(Code::FailedPrecondition, message, detail)
}

pub fn map_registry_error(error: iso22900_registry::RegistryError) -> Status {
    match error {
        iso22900_registry::RegistryError::Io(e) => {
            sanitized_internal("I/O error while searching for PDU libraries", e)
        }
        iso22900_registry::RegistryError::InvalidUri(e) => sanitized_internal(
            "invalid URI in registry file while searching for PDU libraries",
            e,
        ),
        iso22900_registry::RegistryError::NotFound(name) => {
            Status::not_found(format!("no PDU library found with name '{name}'"))
        }
        iso22900_registry::RegistryError::Url(e) => sanitized_internal(
            "failed to parse registry file while searching for PDU libraries",
            e,
        ),
        iso22900_registry::RegistryError::Xml(e) => sanitized_internal(
            "failed to parse registry file while searching for PDU libraries",
            e,
        ),
    }
}

/// Builds the `(code, message, ErrorDetail)` parts for a failed native
/// D-PDU API call (see ADR-105). Every failure that reaches the native API
/// has an ISO 22900-2 `PDUError` return code to report:
/// [`DPduApiError::PduError`] carries it directly (the numeric code and the
/// generated `PduError` proto enum share the same ISO 22900-2 hex values, so
/// no lossy conversion is needed). Other variants (library load errors,
/// etc.) do not occur once construction has already succeeded (ADR-089),
/// but are mapped to `PDU_ERR_FCT_FAILED` as a safe fallback, with the
/// underlying error sanitized out of the client-facing message exactly like
/// `map_construct_error`/`map_registry_error`.
fn runtime_error_parts(error: &DPduApiError) -> (Code, String, ErrorDetail) {
    match error {
        DPduApiError::PduError(code) => (
            Code::Internal,
            format!("ISO22900 call failed: {error}"),
            ErrorDetail {
                pdu_error: *code as i32,
                error_event_data: None,
                detail_text: None,
            },
        ),
        other => (
            Code::Internal,
            sanitized_message("ISO22900 call failed", other),
            ErrorDetail {
                pdu_error: PduError::PduErrFctFailed as i32,
                error_event_data: None,
                detail_text: None,
            },
        ),
    }
}

pub fn map_runtime_error(error: DPduApiError) -> Status {
    let (code, message, detail) = runtime_error_parts(&error);
    status_with_error_detail(code, message, detail)
}

/// Builds an [`ErrorEventData`] from a `PDUGetLastError` result, unless it
/// reports `PDU_ERR_EVT_NOERROR` -- a legitimate "nothing tracked" result,
/// not a real async error worth surfacing (mirrors
/// `j2534-0404-service::error::error_event_data_for`'s same filter).
fn error_event_data_for(
    last_error: LastErrorInfo,
    module: ModuleHandle,
    cll: ComLogicalLinkHandle,
) -> Option<ErrorEventData> {
    if last_error.error_code.0 == PduErrorEvent::PduErrEvtNoerror as i32 as u32 {
        return None;
    }
    Some(ErrorEventData {
        error_event: last_error.error_code.0 as i32,
        cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
            module_handle: module.0,
            cll_handle: cll.0,
            cop_handle: last_error.com_primitive_handle.0,
        }),
        timestamp: last_error.timestamp,
        extra_error_info: last_error.extra_error_info,
    })
}

/// Like [`map_runtime_error`], but for a failure that occurred with a
/// Module+CLL handle already in scope: best-effort fetches the native
/// `PDUGetLastError`-equivalent (the lock on `api` is already held by the
/// caller) to fill in `ErrorDetail.error_event_data`. If that fetch itself
/// fails, the original error is still returned -- just without
/// `error_event_data` -- rather than masking it.
pub(crate) fn map_runtime_error_for_link(
    api: &DPduApi,
    module: ModuleHandle,
    cll: ComLogicalLinkHandle,
    error: DPduApiError,
) -> Status {
    let (code, message, mut detail) = runtime_error_parts(&error);
    match api.get_last_error(module, cll) {
        Ok(last_error) => {
            detail.error_event_data = error_event_data_for(last_error, module, cll);
        }
        Err(fetch_error) => {
            warn!(
                module_handle = module.0,
                cll_handle = cll.0,
                %fetch_error,
                "failed to fetch last-error event data for a failed RPC"
            );
        }
    }
    status_with_error_detail(code, message, detail)
}

#[cfg(test)]
mod tests {
    use iso22900::{ComPrimitiveHandle, ErrorEventCode};

    use super::*;

    #[test]
    fn error_event_data_for_omits_noerror() {
        let last_error = LastErrorInfo {
            error_code: ErrorEventCode(PduErrorEvent::PduErrEvtNoerror as i32 as u32),
            com_primitive_handle: ComPrimitiveHandle(0),
            timestamp: 0,
            extra_error_info: 0,
        };
        assert_eq!(
            error_event_data_for(last_error, ModuleHandle(1), ComLogicalLinkHandle(2)),
            None,
            "PDU_ERR_EVT_NOERROR is not a real tracked error and must not be surfaced"
        );
    }

    #[test]
    fn error_event_data_for_surfaces_a_real_tracked_error() {
        let last_error = LastErrorInfo {
            error_code: ErrorEventCode(PduErrorEvent::PduErrEvtLostCommToVci as i32 as u32),
            com_primitive_handle: ComPrimitiveHandle(9),
            timestamp: 42,
            extra_error_info: 7,
        };
        let data = error_event_data_for(last_error, ModuleHandle(1), ComLogicalLinkHandle(2))
            .expect("a real tracked error should be surfaced");
        assert_eq!(
            data.error_event,
            PduErrorEvent::PduErrEvtLostCommToVci as i32
        );
        assert_eq!(data.timestamp, 42);
        assert_eq!(data.extra_error_info, 7);
        let cop_handle = data.cop_handle.expect("cop_handle should be set");
        assert_eq!(cop_handle.module_handle, 1);
        assert_eq!(cop_handle.cll_handle, 2);
        assert_eq!(cop_handle.cop_handle, 9);
    }

    #[test]
    fn api_not_initialized_status_carries_error_detail() {
        use vci_service_interface::error_detail_from_status;

        let status = api_not_initialized_status("API not initialized");
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "API not initialized");
        let detail = error_detail_from_status(&status).expect("ErrorDetail should be attached");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrPduapiNotConstructed as i32
        );
        assert!(detail.error_event_data.is_none());
    }
}
