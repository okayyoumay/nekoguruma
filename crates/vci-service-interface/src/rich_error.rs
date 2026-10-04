//! Rich gRPC error model helpers.
//!
//! Attaches an [`ErrorDetail`] to a failing RPC's [`Status`] via the
//! `grpc-status-details-bin` trailer, following the standard gRPC rich error
//! model (a serialized `google.rpc.Status` carrying `details` as
//! `google.protobuf.Any`). `google.rpc.Status` and `google.protobuf.Any` are
//! hand-written below rather than vendored from a second `.proto` compilation
//! unit: both are fixed, external, two/three-field schemas, and a
//! hand-written `prost::Message` impl produces byte-identical wire output to
//! a generated one, so any standard gRPC client can still decode it.
//!
//! See `docs/adr/ADR-105-rich-error-model-replaces-get-last-error.md`.

use prost::Message;
use tonic::{Code, Status};

use crate::ErrorDetail;

const ERROR_DETAIL_TYPE_URL: &str = "type.googleapis.com/vci.service.ErrorDetail";

/// Hand-written mirror of `google.protobuf.Any`.
#[derive(Clone, PartialEq, Message)]
struct GoogleProtobufAny {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

/// Hand-written mirror of `google.rpc.Status`, the payload of the
/// `grpc-status-details-bin` trailer under the rich error model.
#[derive(Clone, PartialEq, Message)]
struct GoogleRpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<GoogleProtobufAny>,
}

/// Builds a [`Status`] carrying `detail` as a rich error detail, so a client
/// gets everything a follow-up `GetLastError` call would have provided in
/// this same failing response.
pub fn status_with_error_detail(
    code: Code,
    message: impl Into<String>,
    detail: ErrorDetail,
) -> Status {
    let message = message.into();
    let any = GoogleProtobufAny {
        type_url: ERROR_DETAIL_TYPE_URL.to_string(),
        value: detail.encode_to_vec(),
    };
    let rpc_status = GoogleRpcStatus {
        code: code as i32,
        message: message.clone(),
        details: vec![any],
    };
    Status::with_details(code, message, rpc_status.encode_to_vec().into())
}

/// Extracts the [`ErrorDetail`] attached to `status`, if any.
pub fn error_detail_from_status(status: &Status) -> Option<ErrorDetail> {
    let rpc_status = GoogleRpcStatus::decode(status.details()).ok()?;
    rpc_status
        .details
        .into_iter()
        .find(|any| any.type_url.ends_with("/vci.service.ErrorDetail"))
        .and_then(|any| ErrorDetail::decode(any.value.as_slice()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PduError, PduErrorEvent};

    #[test]
    fn round_trips_error_detail_through_status() {
        let detail = ErrorDetail {
            pdu_error: PduError::PduErrRscLocked as i32,
            error_event_data: Some(crate::ErrorEventData {
                error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                cop_handle: None,
                timestamp: 42,
                extra_error_info: 7,
            }),
            detail_text: Some("resource locked by another CLL".to_string()),
        };

        let status =
            status_with_error_detail(Code::FailedPrecondition, "resource locked", detail.clone());
        let round_tripped =
            error_detail_from_status(&status).expect("error detail should round-trip");

        assert_eq!(round_tripped, detail);
    }

    #[test]
    fn round_trips_unknown_pdu_error_value() {
        // Unknown/vendor-specific codes must survive as an open i32.
        let detail = ErrorDetail {
            pdu_error: 0x1234,
            error_event_data: None,
            detail_text: None,
        };

        let status = status_with_error_detail(Code::Internal, "unmapped", detail.clone());
        let round_tripped =
            error_detail_from_status(&status).expect("error detail should round-trip");

        assert_eq!(round_tripped, detail);
    }

    #[test]
    fn missing_error_detail_returns_none() {
        let status = Status::invalid_argument("plain validation failure");
        assert_eq!(error_detail_from_status(&status), None);
    }
}
