//! What the agent's `tracing` calls may log (design 12.2, ADR-348 item 6; the rule is
//! `docs/operational-log.md`).
//!
//! An operational log entry never holds a vehicle's identity or a value read from it, key or
//! credential material, personal data or a diagnostic exchange's payload. An error's own text
//! can carry any of these (a response's bytes, a worker's status message), so a `tracing` call
//! logs an error through [`LogSafe`]: its NGR-written part as the `error` field, and text NGR
//! does not write itself, escaped, as the `external` field:
//!
//! ```ignore
//! tracing::warn!(error = %error.log_text(), external = error.log_external(), "...");
//! ```

use tracing::field::DisplayValue;

pub use vci_service_config::External;

/// An `external` field's value: `text`, escaped ([`External`]).
pub type ExternalField = DisplayValue<External<String>>;

/// An error as an operational log entry may carry it.
pub trait LogSafe {
    /// The error in NGR's own words, with codes, positions, counts and lengths, but no value
    /// read from the vehicle, no payload and no text NGR does not write itself.
    fn log_text(&self) -> String;

    /// Text the error carries that NGR does not write itself (a worker's or a vendor's text, a
    /// file path, a VCI name), escaped for the entry's `external` field; `None` when it carries
    /// none. A field set to `None` is not recorded.
    fn log_external(&self) -> Option<ExternalField> {
        None
    }
}

/// `text` as an `external` field.
pub fn external(text: impl std::fmt::Display) -> ExternalField {
    tracing::field::display(External(text.to_string()))
}

/// A [`diag_ir::VmError`]'s text without the run-time values some variants print (an index, a
/// byte, a shift amount or a wait duration the procedure computed, which may come from the
/// vehicle).
pub fn vm_error_text(error: &diag_ir::VmError) -> String {
    use diag_ir::VmError;
    match error {
        VmError::ShiftOutOfRange(_) => "shift amount out of range".to_owned(),
        VmError::IndexOutOfRange(_) => "index out of range".to_owned(),
        VmError::ByteOutOfRange(_) => "byte value out of range".to_owned(),
        VmError::WaitOutOfRange(_) => "wait duration out of range".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_error_s_run_time_value_is_left_out() {
        for error in [
            diag_ir::VmError::ShiftOutOfRange(4711),
            diag_ir::VmError::IndexOutOfRange(4711),
            diag_ir::VmError::ByteOutOfRange(4711),
            diag_ir::VmError::WaitOutOfRange(4711),
        ] {
            let text = vm_error_text(&error);
            assert!(!text.contains("4711"), "{text}");
            assert!(error.to_string().starts_with(&text), "{text}");
        }
        assert_eq!(
            vm_error_text(&diag_ir::VmError::StackUnderflow),
            "stack underflow"
        );
    }

    /// A failed RPC logs its status code, and the worker's message only in `external`,
    /// escaped.
    #[test]
    fn a_failed_rpc_s_message_is_external() {
        let error = crate::HostError::Rpc {
            rpc: "StartComPrimitive",
            status: Box::new(tonic::Status::invalid_argument(
                "vendor said\nWBA0000000000001",
            )),
        };
        assert_eq!(
            error.log_text(),
            "StartComPrimitive failed: InvalidArgument"
        );
        let external = error.log_external().expect("the message").to_string();
        assert_eq!(external, r"vendor said\nWBA0000000000001");
        let job = crate::runner::JobError::Host {
            pc: 7,
            source: error,
        };
        assert_eq!(
            job.log_text(),
            "primitive failed at 7: StartComPrimitive failed: InvalidArgument"
        );
        assert_eq!(job.log_external().map(|e| e.to_string()), Some(external));
    }

    /// An error that prints response bytes logs at most their NRC.
    #[test]
    fn response_bytes_are_left_out() {
        use crate::HostError;
        let cases = [
            (
                HostError::TransferRefused {
                    block: 3,
                    response: vec![0x7F, 0x36, 0x73],
                },
                "the ECU refused TransferData block 3 with NRC 0x73",
            ),
            (
                HostError::TransferRefused {
                    block: 3,
                    response: vec![0xAB, 0xCD],
                },
                "the ECU refused TransferData block 3",
            ),
            (
                HostError::TransferEchoMismatch {
                    block: 3,
                    counter: 4,
                    response: vec![0x76, 0xAB],
                },
                "the ECU's TransferData response to block 3 (counter 0x04) is wrong",
            ),
            (
                HostError::NoBlockLength {
                    block: 3,
                    response: vec![0x74, 0xAB],
                },
                "the ECU's RequestDownload response has no readable maxNumberOfBlockLength, so \
                 TransferData block 3 is not sent (ADR-336)",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.log_text(), text);
            assert!(error.to_string().contains("AB") || text.contains("NRC"));
            assert!(error.log_external().is_none());
        }
    }

    /// A procedure failure logs its code, not its detail.
    #[test]
    fn a_procedure_failure_s_detail_is_left_out() {
        let error = crate::runner::JobError::ProcedureFailed {
            pc: 12,
            code: "E42".to_owned(),
            detail: "VIN WBA0000000000001".to_owned(),
        };
        assert_eq!(
            error.log_text(),
            r#"the procedure ended as failed at 12: "E42""#
        );
        assert!(error.log_external().is_none());
    }

    /// A VCI name and a lock path go in `external`.
    #[test]
    fn a_vci_name_and_a_path_are_external() {
        use crate::guards::GuardError;
        let error = GuardError::InvalidVci("Ann's VCI".to_owned());
        assert!(!error.log_text().contains("Ann"), "{}", error.log_text());
        assert_eq!(
            error.log_external().map(|e| e.to_string()).as_deref(),
            Some("Ann's VCI")
        );
        let error =
            crate::runner::JobError::VehicleLock(GuardError::NotAFile("/home/ann/locks/x".into()));
        assert_eq!(
            error.log_text(),
            "the per-vehicle lock could not be taken: a lock file is not a regular file"
        );
        assert_eq!(
            error.log_external().map(|e| e.to_string()).as_deref(),
            Some("/home/ann/locks/x")
        );
    }

    #[test]
    fn an_external_field_is_escaped() {
        assert_eq!(external("a\nb\u{7}").to_string(), r"a\nb\u{7}");
    }
}
