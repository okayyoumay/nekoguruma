//! What the host matches the answer to a generic send-receive against (`Op::SendReceive`,
//! ADR-321, ADR-322).
//!
//! A send-receive carries its whole request on the stack and names the declared service whose
//! answer it expects. The job's document gives the rest:
//! - the service's request SID, which the request must start with, so the policy can check the
//!   request before the program runs (ADR-322 item 1);
//! - the start of its positive response;
//! - the negative-response shape of the variant's protocols, checked against what the workers
//!   support (ADR-309 item 5) before it is used.

use diag_ir::Op;
use diag_ir::declaration::EcuVariant;

use crate::document::JobDocument;
use crate::host::HostError;

/// The response-pending codes the link has the worker absorb: 0x78
/// (requestCorrectlyReceived-ResponsePending) only, since the link turns on `CP_RC78Handling`
/// alone (`link.rs`). The worker could also absorb 0x21 and 0x23 (ADR-309 item 5), but a code
/// the link does not turn on would reach the host, which would skip it and end in `NoResponse`,
/// and a list without 0x78 would let the worker's 0x78 through as the answer (ADR-322 item 3).
const SUPPORTED_PENDING: &[u8] = &[0x78];

/// How a negative response looks: the marker, then the request's SID when it is echoed, with
/// the code at `code_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NegativeShape {
    pub(crate) marker: u8,
    pub(crate) echoes_sid: bool,
    pub(crate) code_at: u8,
    /// The codes that mean the answer is still pending.
    pub(crate) pending: Vec<u8>,
}

impl NegativeShape {
    /// UDS's shape: `7F`, the SID, then the code; 0x78 is pending (ISO 14229-1:2026 annex A.1).
    pub(crate) fn uds() -> Self {
        Self {
            marker: 0x7F,
            echoes_sid: true,
            code_at: 2,
            pending: vec![0x78],
        }
    }

    /// Whether `response` is a complete negative response to a request with service `sid`.
    pub(crate) fn is_negative(&self, sid: u8, response: &[u8]) -> bool {
        response.first() == Some(&self.marker)
            && (!self.echoes_sid || response.get(1) == Some(&sid))
            && response.len() > usize::from(self.code_at)
    }

    /// Whether `response` is a negative response to `sid` whose code means it is still pending.
    pub(crate) fn is_pending(&self, sid: u8, response: &[u8]) -> bool {
        self.is_negative(sid, response)
            && self.pending.contains(&response[usize::from(self.code_at)])
    }

    /// The bytes a negative response to `sid` starts with, which the worker matches.
    pub(crate) fn pattern(&self, sid: u8) -> Vec<u8> {
        if self.echoes_sid {
            vec![self.marker, sid]
        } else {
            vec![self.marker]
        }
    }
}

/// What the answer to one send-receive must look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expectation {
    /// The declared request SID, which the request must start with.
    pub(crate) sid: u8,
    /// The declared positive response's SID and prefix, which the answer must start with.
    pub(crate) positive: Vec<u8>,
    pub(crate) negative: NegativeShape,
}

/// The request SID of service `service`, when `variant` declares it with a request and a
/// positive response, the two a send-receive needs (ADR-322 item 1).
pub fn declared_sid(variant: &EcuVariant<'_>, service: u32) -> Option<u8> {
    let declared = variant.services()?.iter().find(|s| s.id() == service)?;
    declared.positive()?;
    Some(declared.request()?.sid())
}

/// The negative-response shape of `variant`'s protocols (ADR-321 item 4), refused unless each
/// protocol declares one, all the same, and it is one the workers support (ADR-309 item 5): a
/// `7F` marker, the request's SID after it and the code at the next byte, with the pending
/// codes the link turns on (`SUPPORTED_PENDING`). The document comes from the server, so it is
/// not trusted to be consistent (ADR-322 item 3).
pub(crate) fn negative_shape(variant: &EcuVariant<'_>) -> Result<NegativeShape, HostError> {
    let mut shape: Option<NegativeShape> = None;
    for protocol in variant.protocols().into_iter().flatten() {
        let declared = protocol
            .negative_shape()
            .ok_or(HostError::BadNegativeShape(
                "a protocol declares no negative-response shape",
            ))?;
        let declared = NegativeShape {
            marker: declared.marker(),
            echoes_sid: declared.echoes_sid(),
            code_at: declared.code_at(),
            pending: declared
                .pending()
                .map(|p| p.iter().collect())
                .unwrap_or_default(),
        };
        match &shape {
            Some(first) if *first != declared => {
                return Err(HostError::BadNegativeShape(
                    "the protocols declare different negative-response shapes",
                ));
            }
            Some(_) => {}
            None => shape = Some(declared),
        }
    }
    let shape = shape.ok_or(HostError::BadNegativeShape(
        "the variant declares no protocol",
    ))?;
    // The code follows the marker and any echoed SID.
    if usize::from(shape.code_at) < 1 + usize::from(shape.echoes_sid) {
        return Err(HostError::BadNegativeShape(
            "the code is placed on the marker or the echoed service identifier",
        ));
    }
    if shape.marker != 0x7F || !shape.echoes_sid || shape.code_at != 2 {
        return Err(HostError::BadNegativeShape(
            "only a 7F marker, the echoed service identifier and the code after it are supported",
        ));
    }
    if shape.pending != SUPPORTED_PENDING {
        return Err(HostError::BadNegativeShape(
            "the pending codes must be 0x78 alone, the one the link has the worker absorb",
        ));
    }
    Ok(shape)
}

/// What the answer to a send-receive of `service` must look like, from `variant`.
pub(crate) fn expectation(
    variant: &EcuVariant<'_>,
    service: u32,
) -> Result<Expectation, HostError> {
    let undeclared = HostError::UndeclaredService(service);
    let declared = variant
        .services()
        .and_then(|services| services.iter().find(|s| s.id() == service))
        .ok_or(undeclared)?;
    let (Some(request), Some(positive)) = (declared.request(), declared.positive()) else {
        return Err(HostError::UndeclaredService(service));
    };
    let mut start = vec![positive.sid()];
    start.extend(positive.prefix().into_iter().flatten());
    Ok(Expectation {
        sid: request.sid(),
        positive: start,
        negative: negative_shape(variant)?,
    })
}

/// The checks before a send-receive of `service` sends `request`, and what its answer must look
/// like: the request is not empty and starts with the service's declared SID (ADR-322 item 1),
/// that SID passes the flash-plan rules (item 2, `transfer` as for [`check_flash_rules`]), and
/// the document declares the service and a usable negative-response shape (item 3).
pub(crate) fn prepare(
    document: Option<&JobDocument>,
    service: u32,
    request: &[u8],
    transfer: bool,
) -> Result<Expectation, HostError> {
    let &sent = request.first().ok_or(HostError::EmptyRequest)?;
    let document = document.ok_or(HostError::UndeclaredService(service))?;
    let expected = expectation(&document.variant(), service)?;
    if sent != expected.sid {
        return Err(HostError::WrongService {
            service,
            declared: expected.sid,
            sent,
        });
    }
    check_flash_rules(sent, transfer)?;
    Ok(expected)
}

/// Refuses a send-receive of a session change, a reset, an upload or a file transfer inside a
/// recovery plan's transfer, from after its RequestDownload to its transfer exit, where
/// `Program::validate` refuses the same `ServiceRequest` (ADR-250, ADR-322 item 2). The SID is
/// the declared one (item 1), so this holds before the program runs; refused only at run time,
/// it would fail after the erase.
pub(crate) fn check_plans(
    program: &diag_ir::Program,
    variant: &EcuVariant<'_>,
) -> Result<(), (u32, HostError)> {
    for plan in &program.flash {
        let b = &plan.boundaries;
        let mut downloaded = false;
        for pc in b.erase_pc..=b.transfer_exit_pc {
            match program.code.get(pc as usize) {
                Some(Op::ServiceRequest { service: 0x34 }) => downloaded = true,
                Some(Op::SendReceive { service }) if downloaded => {
                    if let Some(sid) = declared_sid(variant, *service) {
                        check_flash_rules(sid, true).map_err(|error| (pc, error))?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The first `Op::SendReceive` of `code`, and its index.
pub(crate) fn first_send_receive(code: &[Op]) -> Option<u32> {
    code.iter()
        .position(|op| matches!(op, Op::SendReceive { .. }))
        .map(|pc| u32::try_from(pc).unwrap_or(u32::MAX))
}

/// Refuses a send-receive whose SID `Program::validate` would refuse as a `ServiceRequest` at
/// the same place (ADR-245, ADR-250, ADR-322 item 2). `Program::validate` sees no SID in a
/// send-receive, so the policy applies this to the declared SID and the host to the sent one.
/// `transfer` is whether the host tracks a transfer.
/// - RequestDownload (0x34) and RequestTransferExit (0x37) belong at a recovery plan's
///   declared boundaries, which only a `ServiceRequest` can be, so they are refused anywhere.
/// - TransferData (0x36) goes through `FlashTransfer`'s block count (ADR-250).
/// - DiagnosticSessionControl (0x10), ECUReset (0x11), RequestUpload (0x35) and
///   RequestFileTransfer (0x38) end the transfer the host tracks, so they are refused while
///   one is.
pub(crate) fn check_flash_rules(sid: u8, transfer: bool) -> Result<(), HostError> {
    match sid {
        0x34 | 0x37 => Err(HostError::DownloadBySendReceive(sid)),
        0x36 => Err(HostError::UseFlashTransfer),
        0x10 | 0x11 | 0x35 | 0x38 if transfer => Err(HostError::TransferInterrupted(sid)),
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use diag_ir::IR_SCHEMA_VERSION;
    use diag_ir::declaration::{
        EcuDocumentT, EcuVariantT, NegativeShapeT, PositiveResponseT, ProtocolT, RequestT,
        ServiceT, encode_ecu_document,
    };

    use super::*;

    /// UDS's negative-response shape as the frontend declares it.
    pub(crate) fn uds_shape() -> NegativeShapeT {
        shape(0x7F, true, 2, &[0x78])
    }

    fn shape(marker: u8, echoes_sid: bool, code_at: u8, pending: &[u8]) -> NegativeShapeT {
        let mut shape = NegativeShapeT::default();
        shape.marker = marker;
        shape.echoes_sid = echoes_sid;
        shape.code_at = code_at;
        shape.pending = Some(pending.to_vec());
        shape
    }

    /// Services of [`document`], by id: ReadDTCInformation reportDTCByStatusMask (4), its
    /// request `19 02` and its positive response `59 02`; WriteDataByIdentifier (5);
    /// RequestDownload (6); DiagnosticSessionControl (7); ECUReset (8); RequestTransferExit (9);
    /// TransferData (10); and a positive response with no request (11).
    pub(crate) const READ_DTC: u32 = 4;
    pub(crate) const WRITE_DID: u32 = 5;
    pub(crate) const DOWNLOAD: u32 = 6;
    pub(crate) const SESSION: u32 = 7;
    pub(crate) const RESET: u32 = 8;
    pub(crate) const TRANSFER_EXIT: u32 = 9;
    pub(crate) const TRANSFER_DATA: u32 = 10;
    pub(crate) const NO_REQUEST: u32 = 11;

    /// A document with the services above, one protocol per shape in `shapes` (`None`: a
    /// protocol without one), and `procedure` as its procedure.
    pub(crate) fn document_with(
        shapes: Vec<Option<NegativeShapeT>>,
        procedure: Option<&diag_ir::Program>,
    ) -> JobDocument {
        let service = |id, sid: u8, prefix: Vec<u8>| {
            let mut service = ServiceT::default();
            service.id = id;
            service.short_name = Some(format!("service{id}"));
            let mut request = RequestT::default();
            request.sid = sid;
            request.prefix = Some(prefix.clone());
            service.request = Some(Box::new(request));
            let mut positive = PositiveResponseT::default();
            positive.sid = sid.wrapping_add(0x40);
            positive.prefix = Some(prefix);
            service.positive = Some(Box::new(positive));
            service
        };
        let mut no_request = service(NO_REQUEST, 0x22, vec![0xF1, 0x90]);
        no_request.request = None;
        let mut variant = EcuVariantT::default();
        variant.id = 1;
        variant.short_name = Some("v".into());
        variant.protocols = Some(
            shapes
                .into_iter()
                .map(|shape| {
                    let mut protocol = ProtocolT::default();
                    protocol.short_name = Some("ISO15765".into());
                    protocol.application_layer = Some("UDS".into());
                    protocol.negative_shape = shape.map(Box::new);
                    protocol
                })
                .collect(),
        );
        variant.services = Some(vec![
            service(READ_DTC, 0x19, vec![0x02]),
            service(WRITE_DID, 0x2E, vec![0xF1, 0x90]),
            service(DOWNLOAD, 0x34, Vec::new()),
            service(SESSION, 0x10, Vec::new()),
            service(RESET, 0x11, Vec::new()),
            service(TRANSFER_EXIT, 0x37, Vec::new()),
            service(TRANSFER_DATA, 0x36, Vec::new()),
            no_request,
        ]);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        document.procedure = procedure.map(|p| postcard::to_allocvec(p).unwrap());
        JobDocument::new(encode_ecu_document(&document)).expect("the document loads")
    }

    /// [`document_with`] with one UDS protocol and no procedure.
    pub(crate) fn document() -> JobDocument {
        document_with(vec![Some(uds_shape())], None)
    }

    #[test]
    fn a_send_receive_needs_a_declared_request_and_positive_response() {
        let document = document();
        let variant = document.variant();
        assert_eq!(declared_sid(&variant, READ_DTC), Some(0x19));
        assert_eq!(declared_sid(&variant, NO_REQUEST), None);
        assert_eq!(declared_sid(&variant, 99), None);
        let expected = expectation(&variant, READ_DTC).unwrap();
        assert_eq!(expected.sid, 0x19);
        assert_eq!(expected.positive, [0x59, 0x02]);
        assert_eq!(expected.negative, NegativeShape::uds());
        for service in [NO_REQUEST, 99] {
            assert!(matches!(
                expectation(&variant, service),
                Err(HostError::UndeclaredService(s)) if s == service
            ));
        }
    }

    #[test]
    fn the_declared_shape_is_checked_before_it_is_used() {
        // Accepted: the UDS shape, also on several protocols.
        for shapes in [
            vec![Some(uds_shape())],
            vec![Some(uds_shape()), Some(uds_shape())],
        ] {
            let document = document_with(shapes.clone(), None);
            negative_shape(&document.variant()).unwrap_or_else(|e| panic!("{shapes:?}: {e}"));
        }
        for shapes in [
            Vec::new(),
            vec![None],
            vec![Some(uds_shape()), None],
            vec![Some(uds_shape()), Some(shape(0x7F, true, 2, &[0x78, 0x21]))],
            // The code on the marker or the echoed SID.
            vec![Some(shape(0x7F, true, 1, &[0x78]))],
            vec![Some(shape(0x7F, false, 0, &[0x78]))],
            // Consistent, but not what the workers support.
            vec![Some(shape(0x7E, true, 2, &[0x78]))],
            vec![Some(shape(0x7F, false, 1, &[0x78]))],
            vec![Some(shape(0x7F, true, 3, &[0x78]))],
            vec![Some(shape(0x7F, true, 2, &[0x78, 0x22]))],
            // Codes the link does not have the worker absorb, or none.
            vec![Some(shape(0x7F, true, 2, &[0x78, 0x21, 0x23]))],
            vec![Some(shape(0x7F, true, 2, &[0x21]))],
            vec![Some(shape(0x7F, true, 2, &[]))],
            // An absent shape reads as all zeros.
            vec![Some(NegativeShapeT::default())],
        ] {
            let document = document_with(shapes.clone(), None);
            assert!(
                matches!(
                    negative_shape(&document.variant()),
                    Err(HostError::BadNegativeShape(_))
                ),
                "{shapes:?}"
            );
            // The expectation of any service is refused with it.
            assert!(matches!(
                expectation(&document.variant(), READ_DTC),
                Err(HostError::BadNegativeShape(_))
            ));
        }
    }

    #[test]
    fn a_negative_answer_is_matched_by_the_declared_shape() {
        let uds = NegativeShape::uds();
        assert!(uds.is_negative(0x19, &[0x7F, 0x19, 0x31]));
        assert!(!uds.is_negative(0x19, &[0x7F, 0x22, 0x31]));
        assert!(!uds.is_negative(0x19, &[0x7F, 0x19]));
        assert!(!uds.is_negative(0x19, &[0x59, 0x02]));
        assert!(uds.is_pending(0x19, &[0x7F, 0x19, 0x78]));
        assert!(!uds.is_pending(0x19, &[0x7F, 0x19, 0x21]));
        assert!(!uds.is_pending(0x19, &[0x7F, 0x22, 0x78]));
        assert_eq!(uds.pattern(0x19), [0x7F, 0x19]);
        let busy = NegativeShape {
            pending: vec![0x78, 0x21],
            ..NegativeShape::uds()
        };
        assert!(busy.is_pending(0x19, &[0x7F, 0x19, 0x21]));
    }

    #[test]
    fn a_request_is_checked_before_it_is_sent() {
        let document = document();
        let prepared = |service, request: &[u8], transfer| {
            prepare(Some(&document), service, request, transfer)
        };
        assert_eq!(
            prepared(READ_DTC, &[0x19, 0x02, 0x08], false)
                .unwrap()
                .positive,
            [0x59, 0x02]
        );
        assert!(prepared(READ_DTC, &[0x19, 0x02, 0x08], true).is_ok());
        assert!(matches!(
            prepared(READ_DTC, &[], false),
            Err(HostError::EmptyRequest)
        ));
        // The request must be the declared service's.
        assert!(matches!(
            prepared(READ_DTC, &[0x2E, 0xF1, 0x90], false),
            Err(HostError::WrongService {
                service: READ_DTC,
                declared: 0x19,
                sent: 0x2E
            })
        ));
        assert!(matches!(
            prepared(99, &[0x19, 0x02], false),
            Err(HostError::UndeclaredService(99))
        ));
        assert!(matches!(
            prepare(None, READ_DTC, &[0x19, 0x02], false),
            Err(HostError::UndeclaredService(READ_DTC))
        ));
        // A session change or a reset during a transfer, and any download.
        assert!(prepared(SESSION, &[0x10, 0x02], false).is_ok());
        assert!(matches!(
            prepared(SESSION, &[0x10, 0x02], true),
            Err(HostError::TransferInterrupted(0x10))
        ));
        assert!(matches!(
            prepared(RESET, &[0x11, 0x01], true),
            Err(HostError::TransferInterrupted(0x11))
        ));
        assert!(matches!(
            prepared(DOWNLOAD, &[0x34, 0x00, 0x44], false),
            Err(HostError::DownloadBySendReceive(0x34))
        ));
        assert!(matches!(
            prepared(TRANSFER_DATA, &[0x36, 0x01], true),
            Err(HostError::UseFlashTransfer)
        ));
        // An unusable declared shape, before anything is sent.
        let document = document_with(vec![Some(shape(0x7F, true, 1, &[0x78]))], None);
        assert!(matches!(
            prepare(Some(&document), READ_DTC, &[0x19, 0x02], false),
            Err(HostError::BadNegativeShape(_))
        ));
    }

    #[test]
    fn the_flash_plan_rules_apply_to_the_sent_service() {
        for sid in [0x34, 0x37] {
            for transfer in [false, true] {
                assert!(matches!(
                    check_flash_rules(sid, transfer),
                    Err(HostError::DownloadBySendReceive(s)) if s == sid
                ));
            }
        }
        for transfer in [false, true] {
            assert!(matches!(
                check_flash_rules(0x36, transfer),
                Err(HostError::UseFlashTransfer)
            ));
        }
        for sid in [0x10, 0x11, 0x35, 0x38] {
            check_flash_rules(sid, false).unwrap();
            assert!(matches!(
                check_flash_rules(sid, true),
                Err(HostError::TransferInterrupted(s)) if s == sid
            ));
        }
        for sid in [0x19, 0x22, 0x2E, 0x31] {
            check_flash_rules(sid, false).unwrap();
            check_flash_rules(sid, true).unwrap();
        }
    }
}
