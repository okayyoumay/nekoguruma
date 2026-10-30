//! [`DiagHost`] on top of the worker gRPC client (ADR-235).
//!
//! The VM is synchronous and the worker client asynchronous. The runner builds this host on the
//! job's blocking thread, and each primitive blocks on its gRPC calls through the runtime's
//! [`Handle`] (item 1).

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use crate::client::WorkerApi;
use diag_ir::DiagHost;
use tokio::runtime::Handle;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComOperationType, ComPrimitiveCtrlData, ComPrimitiveHandle,
    ErrorData, EventItem, ExpectedResponseData, GetTimestampRequest, PduComPrimitiveStatus,
    PduErrorEvent, StartComPrimitiveRequest, TimestampClock, event_item, event_notification,
};

use crate::comm_log::exchange::{
    AgentTimes, At, Clock, ClockReading, Detail, End, EventsLost, Evidence, Expected,
    LinkErrorKind, Origin, Pending, PendingVisibility, RefusalCause, Rejected, RejectedReason,
    SectionAttrs, SourcePos, Target, TransmitEvidence, UnknownCause, VciTime, VciTimes,
};
use crate::comm_log::{AgentNow, EndError, EndItem, EndRead, UnattachedData, UnattachedItem};
use crate::link::{Link, LinkConfig};
use crate::send_receive::{Expectation, NegativeShape};

/// The `link` of the host's exchange entries and of the log's `link_opened`: a job has one
/// link (design 8.11).
pub(crate) const LOG_LINK: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("{rpc} failed: {status}")]
    Rpc {
        rpc: &'static str,
        status: Box<tonic::Status>,
    },
    #[error("the worker did not answer {0} in time")]
    WorkerUnresponsive(&'static str),
    /// A primitive the host stopped waiting for still holds the link, and the worker did not
    /// cancel it: the request was not started (design 8.11 "Settlement").
    #[error("an earlier primitive the worker did not cancel still holds the link")]
    LinkHeld,
    #[error("the worker's event stream ended")]
    EventStreamEnded,
    #[error("no response from the ECU")]
    NoResponse,
    /// The worker dropped events of the link, so the primitive's outcome is unknown: the
    /// request may have been answered.
    #[error("the worker lost events of the link")]
    EventsLost,
    /// The VCI was lost (`PDU_ERR_EVT_LOST_COMM_TO_VCI`), and the link can no longer reach the
    /// ECU. Not an ECU that stayed silent. Two cases:
    /// - the worker reported the loss while the primitive was pending (ADR-289): the request may
    ///   or may not have reached the ECU;
    /// - the loss came before the primitive, and the worker refused to start it on the link it
    ///   had taken offline (ADR-294): the request was not sent.
    ///
    /// `refused_after_loss` tells them apart: `true` for the second.
    #[error("the VCI was lost")]
    VciLost { refused_after_loss: bool },
    /// The primitive ended without a response after the worker reported this error event
    /// for it (for example a transmit error), so the request may not have been sent.
    #[error("the primitive failed: {}", error_event_name(*.0))]
    PrimitiveFailed(i32),
    #[error("service {0:#x} is not a UDS service ID")]
    BadService(u16),
    #[error(
        "service {0:#x} is not allowed: the agent's request policy refuses it (ADR-235 item 8, ADR-247)"
    )]
    NotAllowed(u16),
    /// A FlashTransfer with no tracked transfer: no positive RequestDownload sent through this
    /// host before it, or the transfer has ended, so there is no block counter to derive
    /// (ADR-250).
    #[error("FlashTransfer needs a positive RequestDownload sent first (ADR-250)")]
    NoTransferActive,
    /// The ECU answered TransferData with a negative response.
    #[error("the ECU refused TransferData block {block}: {response:02X?}")]
    TransferRefused { block: u64, response: Vec<u8> },
    /// The positive TransferData response did not echo the counter that was sent.
    #[error(
        "the ECU's TransferData response to block {block} (counter {counter:#04x}) is wrong: {response:02X?}"
    )]
    TransferEchoMismatch {
        block: u64,
        counter: u8,
        response: Vec<u8>,
    },
    /// The TransferData request for this block, SID and counter included, would be longer than
    /// the maxNumberOfBlockLength the ECU gave in its positive RequestDownload response
    /// (ADR-336 item 12). Nothing was sent, and the tracked transfer is unchanged.
    #[error(
        "TransferData block {block} would be {length} bytes, longer than the ECU's maxNumberOfBlockLength of {max} (ADR-336)"
    )]
    BlockTooLong { block: u64, length: u64, max: u64 },
    /// The positive RequestDownload response that began the transfer carries no readable
    /// maxNumberOfBlockLength (clause 14.2.3), so no block is sent (ADR-336 item 12).
    #[error(
        "the ECU's RequestDownload response {response:02X?} has no readable maxNumberOfBlockLength, so TransferData block {block} is not sent (ADR-336)"
    )]
    NoBlockLength { block: u64, response: Vec<u8> },
    /// A `ServiceRequest` for TransferData: the block counter belongs to `FlashTransfer`.
    #[error("TransferData (0x36) must be sent with FlashTransfer, not ServiceRequest (ADR-250)")]
    UseFlashTransfer,
    /// An `Op::FlashBlock` ran on a host whose job has no artifact.
    #[error("the job has no flash artifact (ADR-336)")]
    NoArtifact,
    /// The range an `Op::FlashBlock` asked for is not inside the job's artifact.
    #[error("{len} artifact bytes from offset {offset} are outside the flash artifact (ADR-336)")]
    ArtifactRange { offset: u64, len: u32 },
    #[error("{0} is not supported by this agent yet")]
    Unsupported(&'static str),
    /// A `SendReceive` with no request bytes (ADR-321).
    #[error("a SendReceive has an empty request")]
    EmptyRequest,
    /// A `SendReceive` naming a service the job's document does not declare with a request and
    /// a positive response, or a job with no document (ADR-322 item 1).
    #[error(
        "SendReceive names service {0}, which the job's document does not declare with a request and a positive response"
    )]
    UndeclaredService(u32),
    /// A `SendReceive` whose request does not start with its declared service's SID (ADR-322
    /// item 1).
    #[error(
        "SendReceive of service {service} sends SID {sent:#04x}, but the service declares {declared:#04x}"
    )]
    WrongService {
        service: u32,
        declared: u8,
        sent: u8,
    },
    /// The link the job's document declares cannot be used (ADR-324).
    #[error("the document's link cannot be used: {0}")]
    DeclaredLink(String),
    /// The variant's negative-response shape cannot be used (ADR-322 item 3).
    #[error("the document's negative-response shape cannot be used: {0}")]
    BadNegativeShape(&'static str),
    /// A `SendReceive` of RequestDownload or RequestTransferExit, which belong at a recovery
    /// plan's declared boundaries (ADR-322 item 2).
    #[error(
        "service {0:#04x} must be sent with ServiceRequest at a recovery plan's boundary, not SendReceive"
    )]
    DownloadBySendReceive(u8),
    /// A `SendReceive` that would end the transfer in progress (ADR-250, ADR-322 item 2).
    #[error("service {0:#04x} would end the transfer in progress")]
    TransferInterrupted(u8),
    #[error("link setup: {0}")]
    Setup(&'static str),
    /// A `ReadInput` of a runtime input the agent cannot establish: it has no source on this VCI,
    /// the reading could not be decoded, or it is not an integer. Nothing was changed, so the job
    /// can be resumed once the input can be read (ADR-338, ADR-288).
    #[error("the runtime input {0:?} cannot be established")]
    InputNotEstablished(diag_ir::RuntimeInput),
}

/// Whether a failed send-receive's request may have reached the ECU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    /// A refusal before the primitive started proves nothing was sent: the agent refused the
    /// request itself, an earlier primitive still held the link, or the worker refused the start
    /// with a reason or because the VCI was already lost (ADR-294, ADR-350 item 2). Only proof
    /// gives this state.
    NotSent,
    /// The request may have been sent: the primitive started, or the start's outcome is unknown
    /// (a start that timed out or failed without a refusal). Not a promise that it was sent.
    MayHaveReached,
}

impl HostError {
    /// Whether the request of the send-receive that failed with this error may have reached the
    /// ECU. `NotSent` only when a refusal before the start proves nothing was sent; every other
    /// error, and any error added later, is `MayHaveReached` until it is judged otherwise.
    ///
    /// It judges the errors of a request to the ECU (`service_request`, `send_receive`,
    /// `routine_control`, `flash_transfer` and the other primitives). An error of another
    /// step, such as opening the link or reading a runtime input, sent no request whatever this
    /// says.
    pub fn send_state(&self) -> SendState {
        match self {
            // The agent refused before the start RPC.
            HostError::LinkHeld
            | HostError::BadService(_)
            | HostError::NotAllowed(_)
            | HostError::EmptyRequest
            | HostError::UndeclaredService(_)
            | HostError::WrongService { .. }
            | HostError::BadNegativeShape(_)
            | HostError::DownloadBySendReceive(_)
            | HostError::UseFlashTransfer
            | HostError::TransferInterrupted(_)
            | HostError::NoTransferActive
            | HostError::BlockTooLong { .. }
            | HostError::NoBlockLength { .. }
            | HostError::NoArtifact
            | HostError::ArtifactRange { .. }
            | HostError::Unsupported(_)
            | HostError::DeclaredLink(_)
            | HostError::InputNotEstablished(_) => SendState::NotSent,
            // The worker refused the start after a lost VCI (ADR-294).
            HostError::VciLost {
                refused_after_loss: true,
            } => SendState::NotSent,
            // The worker refused the start and named a reason (ADR-350 item 2).
            HostError::Rpc { rpc, status }
                if *rpc == "StartComPrimitive" && refusal_reason(status).is_some() =>
            {
                SendState::NotSent
            }
            // A start that timed out or failed without a refusal: neither a refusal nor a
            // success was seen (rpc-api-guide "Reading a Worker's Answer").
            HostError::Rpc { .. }
            | HostError::WorkerUnresponsive(_)
            | HostError::Setup(_)
            | HostError::VciLost {
                refused_after_loss: false,
            }
            | HostError::NoResponse
            | HostError::EventsLost
            | HostError::EventStreamEnded
            | HostError::PrimitiveFailed(_)
            | HostError::TransferRefused { .. }
            | HostError::TransferEchoMismatch { .. } => SendState::MayHaveReached,
        }
    }
}

/// The name of an error event, or its code when it is not one the interface lists (a
/// vendor's), as an error message and the communication log's `detail` give it.
fn error_event_name(code: i32) -> String {
    PduErrorEvent::try_from(code)
        .map(|event| event.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("error event {code:#x}"))
}

/// A primitive refused on a link the worker took offline after a lost VCI is the lost VCI
/// (ADR-294): the worker refuses it as `FailedPrecondition` and attaches the link's last error
/// event, `PDU_ERR_EVT_LOST_COMM_TO_VCI`, to the status (ADR-105). The loss came between two
/// primitives, so no pending primitive saw its event. Any other error is returned as it is.
fn lost_vci_between_primitives(error: HostError) -> HostError {
    match &error {
        HostError::Rpc { status, .. }
            if status.code() == tonic::Code::FailedPrecondition
                && vci_service_interface::error_detail_from_status(status)
                    .and_then(|detail| detail.error_event_data)
                    .is_some_and(|data| {
                        data.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32
                    }) =>
        {
            // The start was refused after the loss (ADR-294).
            HostError::VciLost {
                refused_after_loss: true,
            }
        }
        _ => error,
    }
}

/// Whether the worker refused a call because it knows no object with the handle given
/// (`PDU_ERR_INVALID_HANDLE`, ISO 22900-2:2022 D.3): for a cancel, a primitive that has ended.
fn names_invalid_handle(status: &tonic::Status) -> bool {
    vci_service_interface::error_detail_from_status(status).is_some_and(|detail| {
        detail.pdu_error == vci_service_interface::PduError::PduErrInvalidHandle as i32
    })
}

/// Calls a unary RPC with a deadline (ADR-235 item 6).
pub(crate) async fn unary<T>(
    deadline: Duration,
    rpc: &'static str,
    call: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<T, HostError> {
    match tokio::time::timeout(deadline, call).await {
        Err(_) => Err(HostError::WorkerUnresponsive(rpc)),
        Ok(Err(status)) => Err(HostError::Rpc {
            rpc,
            status: Box::new(status),
        }),
        Ok(Ok(response)) => Ok(response.into_inner()),
    }
}

/// Deadlines of the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// Every unary RPC to the worker.
    pub unary: Duration,
    /// A whole send-receive, from the request to the end of the primitive.
    pub send_recv: Duration,
}

impl Timings {
    /// 5 s for unary calls, and the link's send-receive ceiling.
    pub fn for_link(config: &LinkConfig) -> Self {
        Self {
            unary: Duration::from_secs(5),
            send_recv: config.send_recv_ceiling(),
        }
    }
}

/// The request a `ServiceRequest` sends: the SID, then the payload (ADR-235 item 2).
pub fn service_request_bytes(service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
    let sid = u8::try_from(service).map_err(|_| HostError::BadService(service))?;
    let mut request = Vec::with_capacity(1 + payload.len());
    request.push(sid);
    request.extend_from_slice(payload);
    Ok(request)
}

/// RoutineControl (0x31): sub-function, routine identifier, option record; ISO 14229-1:2026
/// clause 13.2.
pub fn routine_control_bytes(routine: u16, sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut request = vec![0x31, sub];
    request.extend_from_slice(&routine.to_be_bytes());
    request.extend_from_slice(payload);
    request
}

/// The running count of the blocks of one transfer, from which the wire block sequence counter
/// is derived (ADR-250).
///
/// ISO 14229-1:2026 clause 14.4: the counter is 1 for the first TransferData after a
/// RequestDownload (or RequestUpload), rises by one per request, and after 0xFF continues at
/// 0x00. The host begins a transfer only on a positive RequestDownload.
/// The IR's `FlashTransfer` operand is constant per instruction, so it cannot carry this.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TransferCounter {
    /// Blocks the ECU confirmed since the last RequestDownload; `None` when no transfer is
    /// tracked.
    accepted: Option<u64>,
    /// The maxNumberOfBlockLength of the positive RequestDownload response that began the
    /// transfer, or that response when it carries none readable ([`max_block_length`]). It
    /// counts only while `accepted` is `Some`: every RequestDownload sets both.
    max_block_length: Option<Result<u64, Vec<u8>>>,
}

impl TransferCounter {
    /// Updates the tracked transfer after a `ServiceRequest` was sent; `outcome` is the whole
    /// response, or `Err` when there was none or sending failed.
    ///
    /// - RequestDownload (0x34) answered positively begins a transfer at count 0, so a second
    ///   one mid-transfer starts over, and the response's maxNumberOfBlockLength becomes the
    ///   transfer's limit. Any other outcome leaves no transfer.
    /// - RequestUpload (0x35), RequestTransferExit (0x37), RequestFileTransfer (0x38),
    ///   DiagnosticSessionControl (0x10) and ECUReset (0x11) end the tracked transfer whatever
    ///   the response: FlashTransfer would discard upload data, and the other four end or
    ///   invalidate the server's transfer (ADR-250).
    /// - Other services leave it as it is.
    fn after_request(&mut self, request: &[u8], outcome: Result<&[u8], ()>) {
        match request.first() {
            Some(0x34) => {
                (self.accepted, self.max_block_length) = match outcome {
                    Ok(response @ [0x74, ..]) => (
                        Some(0),
                        Some(max_block_length(response).ok_or_else(|| response.to_vec())),
                    ),
                    _ => (None, None),
                };
            }
            Some(0x35 | 0x37 | 0x38 | 0x10 | 0x11) => self.accepted = None,
            _ => {}
        }
    }

    /// The 1-based index of the block to send next, and its wire counter.
    fn next(&self) -> Result<(u64, u8), HostError> {
        let index = self.accepted.ok_or(HostError::NoTransferActive)? + 1;
        Ok((index, wire_counter(index)))
    }

    /// Refuses block `index` when its TransferData request, `2 + data_len` bytes, exceeds the
    /// transfer's maxNumberOfBlockLength, or when the RequestDownload response carried none
    /// readable (ADR-336 item 12). The tracked transfer is left as it is, since nothing is sent.
    fn check_length(&self, index: u64, data_len: usize) -> Result<(), HostError> {
        let length = (data_len as u64).saturating_add(2);
        match &self.max_block_length {
            Some(Ok(max)) if length > *max => Err(HostError::BlockTooLong {
                block: index,
                length,
                max: *max,
            }),
            Some(Err(response)) => Err(HostError::NoBlockLength {
                block: index,
                response: response.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// Takes the outcome of sending block `index` with counter `counter`.
    ///
    /// - A positive response echoing the counter counts the block.
    /// - Anything else ends the tracked transfer, and a new RequestDownload is needed: a
    ///   negative response, a positive one that does not echo the counter, and no response
    ///   at all (a timeout, lost events, a transport failure). Whether the ECU kept the data
    ///   is unknown in each case (a missing response may hide a negative one), and the ECU may
    ///   acknowledge a repeat of the previous counter without writing it (clause 14.4), so a
    ///   same-counter retry could leave a gap.
    fn after_block(
        &mut self,
        index: u64,
        counter: u8,
        outcome: Result<Vec<u8>, HostError>,
    ) -> Result<(), HostError> {
        match outcome {
            Ok(response) => match check_transfer_response(index, counter, response) {
                Ok(()) => {
                    self.accepted = Some(index);
                    Ok(())
                }
                Err(error) => {
                    self.accepted = None;
                    Err(error)
                }
            },
            Err(error) => {
                self.accepted = None;
                Err(error)
            }
        }
    }
}

/// The maxNumberOfBlockLength of a positive RequestDownload response (ISO 14229-1:2026 clause
/// 14.2.3): the high nibble of the lengthFormatIdentifier after the SID gives its length in
/// bytes, and the value counts the whole TransferData request, SID and counter included.
///
/// `None` when the response is not that shape: no lengthFormatIdentifier, a length of 0, a
/// reserved low nibble that is not 0, or a response that is not exactly the identifier and
/// the length it names. Then no block is sent ([`HostError::NoBlockLength`]). A value wider
/// than 64 bits counts as `u64::MAX`, which no block reaches.
fn max_block_length(response: &[u8]) -> Option<u64> {
    let format = *response.get(1)?;
    let width = usize::from(format >> 4);
    if width == 0 || format & 0x0F != 0 || response.len() != 2 + width {
        return None;
    }
    let value = &response[2..];
    Some(value.iter().fold(0u64, |max, &byte| {
        max.checked_mul(256)
            .map_or(u64::MAX, |max| max | u64::from(byte))
    }))
}

/// The wire block sequence counter of the `index`-th block (1-based) of a transfer: 1, 2, ...,
/// 0xFF, 0x00, 0x01, ... (clause 14.4).
fn wire_counter(index: u64) -> u8 {
    (index % 256) as u8
}

/// A `ServiceRequest` must not carry TransferData: a repeated counter could be acknowledged
/// without the data being written, so every block goes through `flash_transfer`'s count.
fn refuse_transfer_data(request: &[u8]) -> Result<(), HostError> {
    match request.first() {
        Some(0x36) => Err(HostError::UseFlashTransfer),
        _ => Ok(()),
    }
}

/// TransferData (0x36): the counter, then the data; clause 14.4.
pub fn transfer_data_bytes(counter: u8, data: &[u8]) -> Vec<u8> {
    let mut request = Vec::with_capacity(2 + data.len());
    request.push(0x36);
    request.push(counter);
    request.extend_from_slice(data);
    request
}

/// Requires the positive TransferData response to echo `counter` (clause 14.4).
fn check_transfer_response(block: u64, counter: u8, response: Vec<u8>) -> Result<(), HostError> {
    match response.as_slice() {
        [0x76, echoed, ..] if *echoed == counter => Ok(()),
        [0x76] | [0x76, _, ..] => Err(HostError::TransferEchoMismatch {
            block,
            counter,
            response,
        }),
        _ => Err(HostError::TransferRefused { block, response }),
    }
}

/// The VM's host for one job on one link.
pub struct WorkerHost<C: WorkerApi> {
    handle: Handle,
    client: C,
    link: Link,
    timings: Timings,
    /// When each `Wait` inquiry started.
    waits: HashMap<u64, Instant>,
    /// The worker's `PDU_IOCTL_READ_VBATT` id, or that it has none, once looked up.
    vbatt_id: crate::inputs::VbattId,
    /// The blocks of the transfer in progress (ADR-250).
    transfer: TransferCounter,
    /// The job's document, which the VM decodes response fields with (ADR-316).
    document: Option<crate::document::JobDocument>,
    /// The job's flash artifact, which `Op::FlashBlock` reads its blocks from (ADR-336).
    artifact: Option<crate::artifact::Artifact>,
    /// The job's communication log, which the exchange entries go to (design 8.11, ADR-347).
    comm_log: Option<crate::comm_log::CommLog>,
    /// The link's request and response identifiers, which the entries name.
    link_ids: Option<(u32, u32)>,
    /// The VM position of the step running ([`ProcedurePosition`]): a request sent during a
    /// step gets an entry with `origin` `procedure` at this position.
    at: Option<At>,
    /// The runner's or the restart's own reads running ([`ProcedurePosition::own_reads`]): a
    /// request sent outside a step gets an entry with their origin, and none without one.
    own: Option<OwnRead>,
    /// The `seq` of the entry of the last RequestDownload, which a block refused for its
    /// response names (`caused_by`).
    download_seq: Option<u64>,
    /// A primitive the host stopped waiting for before it ended (its own deadline, lost events,
    /// a failed event stream), which may still hold the link: it is cancelled before the link's
    /// next request, whose wait reads its end if it had one (design 8.11 "Settlement",
    /// ADR-367).
    stray: Option<Stray>,
    /// How long [`WorkerHost::settle_at_end`] keeps the link open for such a primitive
    /// (`LinkConfig::job_end_wait`, ADR-358).
    job_end_wait: Duration,
}

impl<C: WorkerApi> WorkerHost<C> {
    pub fn new(handle: Handle, client: C, link: Link, timings: Timings) -> Self {
        Self {
            handle,
            client,
            link,
            timings,
            waits: HashMap::new(),
            vbatt_id: crate::inputs::VbattId::Unknown,
            transfer: TransferCounter::default(),
            document: None,
            artifact: None,
            comm_log: None,
            link_ids: None,
            at: None,
            own: None,
            download_seq: None,
            stray: None,
            job_end_wait: crate::link::JOB_END_WAIT_MAX,
        }
    }

    /// Sets how long the host waits at job end for a primitive past its deadline
    /// ([`WorkerHost::settle_at_end`]): the link's `LinkConfig::job_end_wait`.
    pub(crate) fn set_job_end_wait(&mut self, wait: Duration) {
        self.job_end_wait = wait;
    }

    /// At job end, settles a primitive the host stopped waiting for (design 8.11 "Settlement",
    /// ADR-358): keeps the link open until the primitive ends or the job-end wait passes after
    /// the agent's own deadline for it, then cancels it if it has not ended, so the link is not
    /// closed under a running primitive. The agent's deadline lies past every limit in force, so
    /// a primitive left by lost events before it is waited for to that deadline first. A lost
    /// VCI ends it too, since the worker then cancels the link's primitives; an event stream
    /// that fails or ends leaves its end unknown, so it is cancelled at once, and so is one whose
    /// cancel before a request already failed (the worker may not answer). A failed cancel is
    /// logged and does not change the job's result. The primitive's end, or the agent's cancel,
    /// is written as its exchange's `primitive_ended` entry ([`WorkerHost::log_primitive_end`]).
    /// Every result and error item of the link read meanwhile, of the primitive, of none or of an
    /// earlier one, goes to the log with it, in read order (ADR-386).
    pub(crate) fn settle_at_end(&mut self) {
        let Some(stray) = self.stray.take() else {
            return;
        };
        let handle = self.handle.clone();
        let until = stray.deadline.max(tokio::time::Instant::now()) + self.job_end_wait;
        let cop = stray.cop;
        let log = self.comm_log.as_ref().filter(|log| log.is_open());
        let events = &mut self.link.events;
        // Its items are read as its exchange's were, so its end is the one the exchange would
        // have had (design 8.11 `primitive_ended`).
        let mut progress = stray.progress();
        // `Some(None)` when the event stream failed or ended, `None` when the wait passed.
        let ended = if stray.cancel_failed {
            None
        } else {
            handle.block_on(async {
                tokio::time::timeout_at(until, async {
                    loop {
                        match events.message().await {
                            Ok(Some(notification)) => {
                                // A lost-events notice ends the wait as it does an exchange's.
                                let lost = matches!(
                                    notification.event_data,
                                    Some(event_notification::EventData::Lost(_))
                                );
                                // Every result and error item of the link is read, whatever
                                // primitive it names or none: an indication of no primitive may
                                // be the exchange's, and a link error of none or of an earlier
                                // one reaches the log (ADR-386). The status of another
                                // primitive is not (`Progress::on_event` keeps the rest as
                                // unbound).
                                let ours = matches!(
                                    &notification.event_data,
                                    Some(event_notification::EventData::Item(item))
                                        if item.cop_handle == Some(cop)
                                            || ends_primitive(item, cop)
                                            || matches!(
                                                &item.data,
                                                Some(
                                                    event_item::Data::ResultData(_)
                                                        | event_item::Data::ErrorData(_)
                                                )
                                            )
                                );
                                if (lost || ours)
                                    && let Some(result) = progress.on_read(
                                        notification.event_data,
                                        cop,
                                        log.map(|log| log.now()),
                                    )
                                {
                                    return Some(Some(result));
                                }
                            }
                            Ok(None) | Err(_) => return Some(None),
                        }
                    }
                })
                .await
                .unwrap_or(None)
            })
        };
        match &ended {
            // Lost events leave the primitive's end unknown, and it may still run.
            Some(Some(result @ Err(HostError::EventsLost))) => {
                self.log_primitive_end(&stray, &mut progress, StrayEnd::Ended(result));
            }
            Some(Some(result)) => {
                self.log_primitive_end(&stray, &mut progress, StrayEnd::Ended(result));
                return;
            }
            // The stream failed or ended before it did: its end is unknown, with every item read
            // before that carried as for any other end.
            Some(None) => self.log_primitive_end(
                &stray,
                &mut progress,
                StrayEnd::Ended(&Err(HostError::EventStreamEnded)),
            ),
            // The wait passed after the primitive's end was read, before its finished status:
            // that end settles it, and the primitive is still cancelled.
            None => {
                if let Some(result) = progress.end_read() {
                    self.log_primitive_end(&stray, &mut progress, StrayEnd::Ended(&result));
                }
            }
        }
        let cancelled = handle.block_on(unary(
            self.timings.unary,
            "CancelComPrimitive",
            self.client.cancel_com_primitive(CancelComPrimitiveRequest {
                cop_handle: Some(cop),
            }),
        ));
        match cancelled.map_err(lost_vci_between_primitives) {
            // Only a cancel the worker carried out is the agent's: one whose handle had ended,
            // or that failed, leaves the end unknown and the exchange as it was.
            Ok(_) if ended.is_none() && progress.end_read().is_none() => {
                self.log_primitive_end(&stray, &mut progress, StrayEnd::Cancelled)
            }
            Ok(_) => {}
            Err(HostError::VciLost { .. }) => {}
            Err(HostError::Rpc { status, .. }) if names_invalid_handle(&status) => {}
            Err(error) => tracing::warn!(
                error = %crate::oplog::LogSafe::log_text(&error),
                "could not cancel a primitive past its deadline at job end"
            ),
        }
        // What was read and no entry carries, because the primitive has none to write: its
        // handle was invalid, or the cancel failed with no end read.
        let rest = progress.take_reads(0, None);
        self.log_unattached(&rest, stray.seq);
    }

    /// Hands the link errors among `reads` to the log, for a job end that has no
    /// `primitive_ended` entry to write (design 8.11 `late_item`, ADR-386): those of no primitive
    /// or of an earlier one, and the primitive's own, which are its exchange's `seq` (the
    /// exchange the entry of the cancel would have followed), or no exchange's without one.
    /// Anything else read stays unwritten, as the entry of a primitive that has none takes it.
    fn log_unattached(&self, reads: &[EndRead], seq: Option<u64>) {
        let Some(log) = &self.comm_log else {
            return;
        };
        let items: Vec<UnattachedItem> = reads
            .iter()
            .filter_map(|read| match &read.item {
                EndItem::Unattached(item) => Some(item.clone()),
                // A lost VCI, which ends the primitive, is no link error.
                EndItem::Error(error) => Some(UnattachedItem {
                    link: LOG_LINK,
                    earlier_primitive: true,
                    owner: seq,
                    vci_us: u32::try_from(error.time.us).unwrap_or(u32::MAX),
                    clock: error.time.clock,
                    read: Some(read.read),
                    data: UnattachedData::Error {
                        event: error.event.clone(),
                        extra_error_info: error.extra_error_info,
                        extra_error_info_name: error.extra_error_info_name.clone(),
                        link_error: error.kind,
                    },
                }),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            return;
        }
        let logged =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.late_link_errors(items)));
        if logged.is_err() {
            tracing::warn!("writing late link errors panicked");
        }
    }

    /// Writes the `primitive_ended` entry of a primitive cancelled before the link's next request
    /// (design 8.11 "Settlement"), once that request's wait is over: the end read during it,
    /// an unknown end when the event stream failed or ended, else the end read before its
    /// deadline, else the agent's cancel when the worker carried it out. A primitive that had
    /// ended (its handle invalid) or that a lost VCI ended, with no end read, keeps its
    /// exchange's classification. What the log kept for the exchange is then released, since
    /// nothing else settles it. Returns the results and errors read for it that no entry
    /// carries (its end was not read), which go to the exchange in flight's unbound items of an
    /// earlier primitive: a link error among them goes to the stray's exchange once tagged with
    /// its `seq` (ADR-372).
    fn settle_cancelled(&self, settling: Settling, stream_failed: bool) -> Vec<Unbound> {
        let Settling {
            stray,
            mut progress,
            carried_out,
            end,
            items,
        } = settling;
        let end = match end {
            Some(end) => Some(end),
            None if stream_failed => Some(SettlingEnd::Ended(Err(HostError::EventStreamEnded))),
            None => match progress.end_read() {
                Some(result) => Some(SettlingEnd::Ended(result)),
                None if carried_out => Some(SettlingEnd::Cancelled),
                None => None,
            },
        };
        let left = match &end {
            Some(SettlingEnd::Ended(result)) => {
                self.log_primitive_end(&stray, &mut progress, StrayEnd::Ended(result));
                Vec::new()
            }
            Some(SettlingEnd::Cancelled) => {
                // The errors it read go on in `items`, to the exchange in flight as its own
                // (ADR-372): the cancel entry does not write them too.
                progress
                    .reads
                    .retain(|read| !matches!(read.item, EndItem::Error(_)));
                self.log_primitive_end(&stray, &mut progress, StrayEnd::Cancelled);
                items
            }
            None => items,
        };
        if let (Some(seq), Some(log)) = (stray.seq, &self.comm_log) {
            log.release_kept(seq);
        }
        left
    }

    /// Writes the `primitive_ended` entry of `stray`'s exchange, when its entry was written: its
    /// end as `progress` read it, or the agent's cancel when `ended` is `Cancelled` (design 8.11
    /// "Settlement", ADR-366), with every item read for it after the exchange's entry, in read
    /// order (ADR-386): the log places each once and writes the link errors of no primitive and
    /// of earlier primitives beside the entry. Both the job end and the wait of the next request
    /// (ADR-367) come through here. A primitive whose exchange has no entry has no entry to
    /// write: the link errors read are handed to the log alone.
    fn log_primitive_end(&self, stray: &Stray, progress: &mut Progress, ended: StrayEnd<'_>) {
        let Some(log) = &self.comm_log else {
            return;
        };
        let epoch = log.now().epoch;
        let Some(seq) = stray.seq else {
            let reads = progress.take_reads(epoch, None);
            self.log_unattached(&reads, None);
            return;
        };
        let primitive = match ended {
            StrayEnd::Ended(result) => {
                let (end, mut detail) =
                    sent_end(&stray.request, result, &progress.negative, progress.sid);
                // The event named and its extra error information, as for an exchange, from
                // every error the primitive reported: the one that ended it may have been read
                // before the deadline.
                named_events(
                    result,
                    &progress.errors,
                    progress.vci_lost.as_ref(),
                    progress.rx_timeout.as_ref(),
                    &mut detail,
                );
                crate::comm_log::PrimitiveEnded {
                    end: Some(end),
                    detail,
                    vci_ended: progress.rx_timeout.is_some(),
                    reads: progress.take_reads(epoch, Some(result)),
                }
            }
            StrayEnd::Cancelled => crate::comm_log::PrimitiveEnded {
                end: None,
                detail: Detail::default(),
                vci_ended: false,
                reads: progress.take_reads(epoch, None),
            },
        };
        let reads = primitive.reads.clone();
        let logged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            log.primitive_ended(seq, primitive)
        }));
        match logged {
            Err(_) => tracing::warn!("writing a primitive_ended entry panicked"),
            // The log keeps no such exchange: the link errors read for it are not lost
            // (ADR-386).
            Ok(None) if log.is_open() => self.log_unattached(&reads, Some(seq)),
            Ok(_) => {}
        }
    }

    /// Gives the host the job's document (`crate::document`).
    pub fn set_document(&mut self, document: Option<crate::document::JobDocument>) {
        self.document = document;
    }

    /// Gives the host the job's communication log, a clone of the one the runner writes the job's
    /// events to (ADR-347).
    pub fn set_comm_log(&mut self, comm_log: Option<crate::comm_log::CommLog>) {
        self.comm_log = comm_log;
    }

    /// Gives the host its link's request and response identifiers, which its exchange entries
    /// name (design 8.11, `target`).
    pub(crate) fn set_link_ids(&mut self, tx_id: u32, rx_id: u32) {
        self.link_ids = Some((tx_id, rx_id));
    }

    /// Gives the host the job's flash artifact, which `artifact_bytes` serves (ADR-336 item 12).
    pub fn set_artifact(&mut self, artifact: Option<crate::artifact::Artifact>) {
        self.artifact = artifact;
    }

    /// The number of TransferData blocks the ECU confirmed since the last RequestDownload sent
    /// through this host, which is also the 1-based index of the last confirmed block. Any
    /// failed block ends the transfer (`None`), so the journal must have recorded each block as
    /// it was confirmed; the ECU may hold one block more than the last one recorded. `Some(0)`
    /// means a transfer started with no confirmed block yet (it is not a block number); `None`
    /// means no transfer is tracked (none started, or it ended, ADR-250).
    ///
    /// This rising index, not the IR operand, is what the write-job journal records for a block
    /// (ADR-244). It is a `u64`, while `Journal::commit_block` takes a `u32`: the journaling
    /// runner must convert it and never commit 0.
    pub fn transfer_block_index(&self) -> Option<u64> {
        self.transfer.accepted
    }

    /// Gives back the client and the link, to close it.
    pub fn into_parts(self) -> (C, Link) {
        (self.client, self.link)
    }

    /// Sends `request` on the link and returns the whole final response (positive, or a
    /// negative `7F` response), or [`HostError::NoResponse`] if the ECU did not answer.
    ///
    /// Every request leaves through here, so the policy is checked here (ADR-235 item 8).
    fn send_recv(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let before = self.log_clock();
        let checked = crate::policy::check_request(
            &request,
            self.link.application_layer,
            self.link.permission,
        )
        .and_then(|()| Progress::for_request(&request));
        match checked {
            Ok(progress) => self.send(request, progress, before),
            Err(error) => {
                self.log_refused(&request, before, &error, None);
                Err(error)
            }
        }
    }

    /// [`send_recv`](Self::send_recv) for a generic send-receive, whose answer is matched
    /// against `expected` from the job's document instead of UDS's rules (ADR-321).
    fn send_expecting(
        &mut self,
        request: Vec<u8>,
        expected: Expectation,
    ) -> Result<Vec<u8>, HostError> {
        let before = self.log_clock();
        if let Err(error) = crate::policy::check_request(
            &request,
            self.link.application_layer,
            self.link.permission,
        ) {
            self.log_refused(&request, before, &error, None);
            return Err(error);
        }
        self.send(request, Progress::expecting(expected), before)
    }

    /// Sends `request`, which `progress` waits for the answer to, and writes its exchange entry
    /// when the host logs (`before` is the agent's time before it).
    fn send(
        &mut self,
        request: Vec<u8>,
        progress: Progress,
        before: Option<AgentNow>,
    ) -> Result<Vec<u8>, HostError> {
        let logged = before.map(|_| {
            let expected = Expected {
                positive_prefix: progress.positive_prefix.clone(),
                negative_pattern: progress.negative.pattern(progress.sid),
            };
            (
                request.clone(),
                expected,
                progress.negative.clone(),
                progress.sid,
            )
        });
        let handle = self.handle.clone();
        let sent = handle.block_on(self.send_recv_async(request, progress, logged.is_some()));
        if let Some((request, expected, negative, sid)) = logged {
            let (end, mut detail) = sent_end(&request, &sent.result, &negative, sid);
            let end = deadline_end(end, sent.agent_deadline);
            let end = unanswered_end(end, &sent);
            let end_rejected = matches!(end, End::PositiveUnusable(_)) && sent.result.is_err();
            let (link_errors, error_at) = events_seen(&sent, &mut detail);
            // A response's times only for an exchange that ended with that response: one read
            // before the agent's deadline, lost events or a lost VCI ended the wait is not the
            // entry's outcome.
            let response_times = if sent.result.is_ok() {
                sent.response_times
            } else {
                VciTimes::default()
            };
            // A transmit indication bound to the primitive confirms the send, with its time
            // (design 8.11 `sent_evidence`), over the transmit done the response carries.
            let vci = VciTimes {
                error: error_at,
                deadline: sent.deadline_reading,
                rx_timeout: sent.rx_timeout.as_ref().and_then(|timeout| timeout.at),
                tx_done: sent
                    .bound_tx
                    .into_iter()
                    .chain(response_times.tx_done)
                    .reduce(VciTime::prefer),
                ..response_times
            };
            let bound_tx = sent.bound_tx.is_some();
            let unbound = unattached_items(&sent.unbound);
            let before_answer = errors_before_answer(&sent);
            let vci_ended = sent.rx_timeout.is_some();
            let pending = pendings_of(&sent.pendings);
            let rx_id = self.link_ids.map_or(0, |(_, rx_id)| rx_id);
            let rejected = rejected_of(&sent.rejected, rx_id);
            let events_lost = sent.events_lost.flatten().map(|read| EventsLost {
                agent_us: read.monotonic_us,
                notifications: 1,
            });
            let agent_deadline = sent.agent_deadline;
            let seq = self.log(&request, before, end, detail, unbound, |evidence| {
                evidence.pending = pending;
                evidence.rejected = rejected;
                evidence.events_lost = events_lost;
                evidence.end_rejected = end_rejected;
                evidence.expected = expected;
                evidence.vci_ended = vci_ended;
                evidence.bound_start = sent.bound_start;
                if bound_tx {
                    evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
                }
                evidence.link_errors.extend(link_errors);
                evidence.link_errors_before_answer = before_answer;
                evidence.times.vci = (!vci.is_empty()).then_some(vci);
            });
            // The primitive left running belongs to this entry, which its end settles.
            if let Some(stray) = &mut self.stray
                && agent_deadline
                && stray.seq.is_none()
            {
                stray.seq = seq;
            }
        }
        sent.result
    }

    /// Sends `request` and waits for the primitive to end; `logged` when its entry is written,
    /// which alone takes a reading of the VCI's clock at the agent's deadline.
    async fn send_recv_async(
        &mut self,
        request: Vec<u8>,
        mut progress: Progress,
        logged: bool,
    ) -> Sent {
        let request_bytes = request.clone();
        let start = StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(self.link.cll_handle),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: request,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                // Only an answer to this request ends the primitive, so an unsolicited frame
                // or a late answer to an earlier request cannot.
                expected_response_array: expected_responses(&progress),
                tx_flag: None,
            }),
        };
        // A primitive the host stopped waiting for may still hold the link, which runs one
        // primitive at a time (ISO 22900-2:2022 8.4.17.2): cancel it first. It is gone when the
        // cancel succeeds, when the worker names its handle invalid (it had ended), or when the
        // VCI was lost (the worker cancelled it). Any other answer leaves the link held, so the
        // request is not started and the next one tries the cancel again.
        let mut settling = None;
        if let Some(stray) = self.stray.take() {
            let cancelled = unary(
                self.timings.unary,
                "CancelComPrimitive",
                self.client.cancel_com_primitive(CancelComPrimitiveRequest {
                    cop_handle: Some(stray.cop),
                }),
            )
            .await
            .map_err(lost_vci_between_primitives);
            let carried_out = match cancelled {
                Ok(_) => true,
                Err(HostError::VciLost { .. }) => false,
                Err(HostError::Rpc { status, .. }) if names_invalid_handle(&status) => false,
                Err(_) => {
                    self.stray = Some(Stray {
                        cancel_failed: true,
                        ..stray
                    });
                    return Sent::not_started(Err(HostError::LinkHeld));
                }
            };
            // An end it reached before the cancel, still on the event stream, is read during
            // this exchange (design 8.11 "Settlement"); only an exchange whose entry was
            // written has one to settle.
            if stray.seq.is_some() && self.comm_log.is_some() {
                settling = Some(Settling {
                    progress: stray.progress(),
                    stray,
                    carried_out,
                    end: None,
                    items: Vec::new(),
                });
            }
        }
        // One deadline from the request to the end of the primitive.
        let now = tokio::time::Instant::now();
        let deadline = now + self.timings.send_recv;
        // Starting the primitive counts against the same budget.
        let started = unary(
            self.timings.unary.min(self.timings.send_recv),
            "StartComPrimitive",
            self.client.start_com_primitive(start),
        )
        .await
        .map_err(lost_vci_between_primitives)
        .and_then(|response| {
            response
                .cop_handle
                // Without a handle, events of no primitive would match.
                .ok_or(HostError::Setup(
                    "StartComPrimitive returned no primitive handle",
                ))
        });
        let cop = match started {
            Ok(cop) => cop,
            Err(error) => {
                if let Some(settling) = settling {
                    // No wait read anything for it.
                    self.settle_cancelled(settling, false);
                }
                return Sent::not_started(Err(error));
            }
        };
        // A handle the library gave again names the new primitive's items, not the old one's.
        if let Some(reused) = settling.take_if(|settling| settling.stray.cop == cop) {
            self.settle_cancelled(reused, false);
        }
        // The cancelled primitive's items read after its end, or that no end of it carries, are
        // still its exchange's (ADR-372).
        let earlier = settling
            .as_ref()
            .and_then(|settling| Some((settling.stray.cop, settling.stray.seq?)));

        let events = &mut self.link.events;
        let log = self.comm_log.as_ref();
        let progress = &mut progress;
        let settling_ref = &mut settling;
        let wait = async {
            loop {
                let notification = events
                    .message()
                    .await
                    .map_err(|status| HostError::Rpc {
                        rpc: "SubscribeEvent",
                        status: Box::new(status),
                    })?
                    .ok_or(HostError::EventStreamEnded)?;
                // The cancelled primitive's own items go to its end; lost events and a lost
                // VCI end this exchange too.
                if let Some(settling) = settling_ref
                    .as_mut()
                    .filter(|settling| settling.takes(&notification.event_data))
                {
                    let read = log.filter(|log| log.is_open()).map(|log| log.now());
                    settling.read(notification.event_data.clone(), read);
                    if !shared_end(&notification.event_data) {
                        continue;
                    }
                }
                let (kept, kept_pendings, kept_rejected, kept_response) = (
                    progress.unbound.len(),
                    progress.pendings.len(),
                    progress.rejected.len(),
                    progress.response_times.rx_end,
                );
                let done = progress.on_notification(notification.event_data, cop);
                let lost = matches!(done, Some(Err(HostError::EventsLost)));
                let responded = progress.response_times.rx_end != kept_response;
                if progress.unbound.len() > kept
                    || progress.pendings.len() > kept_pendings
                    || progress.rejected.len() > kept_rejected
                    || lost
                    || responded
                {
                    let read = log.filter(|log| log.is_open()).map(|log| log.now());
                    if responded {
                        progress.response_times.response_read_us =
                            read.map(|read| read.monotonic_us);
                    }
                    for unbound in &mut progress.unbound[kept..] {
                        unbound.read = read;
                    }
                    for pending in &mut progress.pendings[kept_pendings..] {
                        pending.read = read;
                    }
                    for rejected in &mut progress.rejected[kept_rejected..] {
                        rejected.read = read;
                    }
                    if lost {
                        progress.events_lost = Some(read);
                    }
                }
                if let Some(done) = done {
                    return done;
                }
            }
        };
        let waited = tokio::time::timeout_at(deadline, wait).await;
        if let Some(settling) = settling {
            let stream_failed = matches!(
                &waited,
                Ok(Err(HostError::EventStreamEnded
                    | HostError::Rpc {
                        rpc: "SubscribeEvent",
                        ..
                    }))
            );
            let left = self.settle_cancelled(settling, stream_failed);
            progress.unbound.extend(left);
        }
        if let Some((earlier_cop, seq)) = earlier {
            for unbound in &mut progress.unbound {
                if unbound.item.cop_handle == Some(earlier_cop) {
                    unbound.owner = Some(seq);
                }
            }
        }
        // A receive timeout the worker already reported ended the VCI's wait, even when the
        // primitive's end had not been read before the deadline.
        let agent_deadline = waited.is_err() && progress.rx_timeout.is_none();
        // Design 8.11 "Settlement": the VCI's clock when the agent stopped waiting shows how far
        // the VCI's own limits had run. It is read after the deadline, so it costs one more
        // unary call's budget at most, so only an exchange whose entry is written takes it.
        let deadline_reading = if agent_deadline && logged {
            self.read_clock(deadline).await
        } else {
            None
        };
        // A primitive whose end was not read may still run; after a lost VCI the worker
        // cancelled it.
        if !progress.ended && progress.vci_lost.is_none() {
            self.stray = Some(Stray {
                cop,
                deadline,
                cancel_failed: false,
                request: request_bytes,
                expectation: crate::send_receive::Expectation {
                    sid: progress.sid,
                    positive: progress.positive_prefix.clone(),
                    negative: progress.negative.clone(),
                },
                seq: None,
                response: progress.response.clone(),
                response_times: progress.response_times,
                error: progress.error,
                errors: progress.errors.clone(),
            });
        }
        let result = waited
            .map_err(|_| HostError::NoResponse)
            .and_then(|outcome| outcome);
        Sent {
            result,
            error_events: std::mem::take(&mut progress.errors),
            errors_before_response: progress.errors_before_response.take(),
            rx_timeout: progress.rx_timeout.take(),
            vci_lost: progress.vci_lost.take(),
            unbound: std::mem::take(&mut progress.unbound),
            pendings: std::mem::take(&mut progress.pendings),
            rejected: std::mem::take(&mut progress.rejected),
            last_rejected: progress.last_rejected,
            bound_start: progress.bound_start,
            bound_tx: progress.bound_tx,
            events_lost: progress.events_lost.take(),
            agent_deadline,
            response_times: progress.response_times,
            deadline_reading,
        }
    }

    /// Reads the VCI's clock (`GetTimestamp` of the link's module), with the clock the worker
    /// says it is on and the time from `since`, the agent's deadline, to the read's answer, which
    /// bounds when after it the clock was read; `None` when the read fails.
    async fn read_clock(&mut self, since: tokio::time::Instant) -> Option<ClockReading> {
        let read = read_vci_clock(
            &mut self.client,
            self.link.module_handle,
            self.timings.unary,
        )
        .await;
        let round_trip = since.elapsed();
        read.map(|time| ClockReading {
            time,
            round_trip_us: u64::try_from(round_trip.as_micros()).unwrap_or(u64::MAX),
        })
    }

    /// The agent's time now, when the host logs the request it is about to send: it has a log,
    /// its link's identifiers, and a VM step or the agent's own reads running.
    fn log_clock(&self) -> Option<AgentNow> {
        match (&self.comm_log, self.origin(), self.link_ids) {
            // A log that failed takes no more lines, so nothing is built for it.
            (Some(log), Some(_), Some(_)) if log.is_open() => Some(log.now()),
            _ => None,
        }
    }

    /// The origin of a request sent now: the procedure's during a VM step, else that of the
    /// agent's own reads running, if any.
    fn origin(&self) -> Option<Origin> {
        match (&self.at, self.own) {
            (Some(_), _) => Some(Origin::Procedure),
            (None, Some(own)) => Some(own.origin()),
            (None, None) => None,
        }
    }

    /// Writes the exchange entry of `request`, which ended as `end`, when the host logs it
    /// (`before` is `Some`): with its origin ([`WorkerHost::origin`]), at the step's position
    /// for the procedure's, with the agent's times and `detail`, and what `complete` adds to the
    /// evidence, after the link's `unbound` items read during the exchange
    /// ([`CommLog::exchange_with`]). A failure of the log never reaches the job, a panic
    /// included (ADR-347).
    ///
    /// [`CommLog::exchange_with`]: crate::comm_log::CommLog::exchange_with
    fn log(
        &mut self,
        request: &[u8],
        before: Option<AgentNow>,
        end: End,
        detail: Detail,
        unbound: Vec<UnattachedItem>,
        complete: impl FnOnce(&mut Evidence),
    ) -> Option<u64> {
        let (Some(before), Some(log), Some(origin), Some((tx_id, rx_id))) =
            (before, &self.comm_log, self.origin(), self.link_ids)
        else {
            return None;
        };
        let target = Target::Endpoint {
            link: LOG_LINK,
            tx_id,
            rx_id,
        };
        let mut evidence = Evidence::new(origin, target, LOG_LINK, rx_id, request.to_vec(), end);
        evidence.at = self.at.clone();
        // Design 8.11 `pending`, ADR-355: forwarded only when the worker says the link passes
        // on every pending. The link has the API handle 0x78 alone (`send_receive`'s supported
        // pending codes), so no profile's 0x21 or 0x23 makes it absorb a busy answer the log
        // cannot see; a link that does stays `absorbed_by_vci`.
        evidence.pending_visibility = pending_visibility(self.link.forwards_pendings);
        evidence.suppressed = suppresses_positive_response(request);
        evidence.detail = detail;
        complete(&mut evidence);
        let logged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let now = log.now();
            evidence.times.agent = AgentTimes {
                epoch: before.epoch,
                before_us: before.monotonic_us,
                tx_confirmed_us: None,
                end_us: now.monotonic_us,
                before_wall_unix_ms: Some(before.wall_unix_ms),
                end_wall_unix_ms: Some(now.wall_unix_ms),
            };
            log.exchange_with(evidence, unbound)
        }));
        let seq = logged.unwrap_or_else(|_| {
            tracing::warn!("writing an exchange entry panicked");
            None
        });
        if request.first() == Some(&SID_REQUEST_DOWNLOAD) {
            self.download_seq = seq;
        }
        seq
    }

    /// Writes the entry of a request the agent refused before sending it, for `error`
    /// (`not_sent`, design 8.11). `caused_by` is the entry whose response caused the refusal.
    fn log_refused(
        &mut self,
        request: &[u8],
        before: Option<AgentNow>,
        error: &HostError,
        caused_by: Option<u64>,
    ) {
        // The log's `not_sent` and `send_state` are one judgement (ADR-389).
        debug_assert_eq!(error.send_state(), SendState::NotSent, "{error:?}");
        let end = End::NotSent(refusal_cause(request, error));
        let detail = Detail {
            host_error: Some(host_error_name(error)),
            ..Detail::default()
        };
        self.log(request, before, end, detail, Vec::new(), |evidence| {
            evidence.caused_by = caused_by;
        });
    }
}

/// The end an exchange's entry gives when the agent's own deadline (`agent_deadline`) ended the
/// wait: not the VCI's receive timeout, so the limit in force may not have run out and the VCI
/// did not end the exchange (design 8.11 `vci_did_not_end`).
fn deadline_end(end: End, agent_deadline: bool) -> End {
    match end {
        End::NoResponse if agent_deadline => End::VciDidNotEnd,
        end => end,
    }
}

/// The end an exchange's entry gives when its primitive ended with no answer and its last
/// result, with no pending or answer after it, was a rejected one of positive form (ADR-359): the response the worker took but the
/// agent cannot use, `positive_unusable` (design 8.11 `outcome`). An exchange the agent's
/// deadline or the VCI's receive timeout ended keeps its end.
fn unanswered_end(end: End, sent: &Sent) -> End {
    match (end, sent.rejected.last()) {
        (End::NoResponse, Some(last))
            if sent.last_rejected
                && sent.rx_timeout.is_none()
                && !sent.agent_deadline
                && last.reason == RejectedReason::WrongEcho =>
        {
            End::PositiveUnusable(last.bytes.clone())
        }
        (end, _) => end,
    }
}

/// An exchange's `pending_visibility` on a link whose worker said `forwards_pendings`: forwarded
/// only when it said the link forwards every pending (ADR-355).
fn pending_visibility(forwards_pendings: Option<bool>) -> PendingVisibility {
    if forwards_pendings == Some(true) {
        PendingVisibility::Forwarded
    } else {
        PendingVisibility::AbsorbedByVci
    }
}

/// The communication log's form of the rejected results: each read with a log, its response
/// identifier the header's, or the link's when the worker split none off.
fn rejected_of(rejected: &[RejectedRead], rx_id: u32) -> Vec<Rejected> {
    rejected
        .iter()
        .filter_map(|rejected| {
            Some(Rejected {
                rx_id: rejected.identifier.unwrap_or(rx_id),
                bytes: rejected.bytes.clone(),
                vci_us: Some(rejected.vci_us),
                clock: rejected.clock,
                epoch: None,
                agent_us: rejected.read?.monotonic_us,
                reason: rejected.reason,
            })
        })
        .collect()
}

/// The communication log's form of the pendings bound to the primitive (design 8.11 `pending`):
/// each with its VCI time on the clock its item names, and the agent's time when the host read
/// it. A pending read with no log open has no agent time and is left out; the log then writes
/// no entry either. The log gives each its interval from the transmit or the previous pending.
fn pendings_of(pendings: &[PendingRead]) -> Vec<Pending> {
    pendings
        .iter()
        .filter_map(|pending| {
            Some(Pending {
                vci_us: Some(pending.vci_us),
                clock: pending.clock,
                epoch: None,
                agent_us: pending.read?.monotonic_us,
                since_previous_ms: None,
            })
        })
        .collect()
}

/// An error event bound to a primitive in the form of a `primitive_ended` read (ADR-386):
/// `kind` is the link error it adds beside the end, if it adds one.
fn end_error(data: &ErrorData, time: VciTime, kind: Option<LinkErrorKind>) -> EndError {
    EndError {
        kind,
        time,
        named: false,
        event: error_event_name(data.error_event),
        extra_error_info: data.extra_error_info,
        extra_error_info_name: data.extra_error_info_name.clone(),
    }
}

/// The communication log's form of the link's `unbound` items: the result and error items, with
/// a result's CAN identifier taken from its header (four bytes, most significant first), where
/// the worker split one off.
fn unattached_items(unbound: &[Unbound]) -> Vec<UnattachedItem> {
    unbound.iter().filter_map(unattached_item).collect()
}

/// [`unattached_items`] for one item.
fn unattached_item(unbound: &Unbound) -> Option<UnattachedItem> {
    let Unbound { item, read, owner } = unbound;
    let data = match &item.data {
        Some(event_item::Data::ResultData(result)) => UnattachedData::Result {
            rx_flag: result.rx_flag.clone(),
            identifier: header_identifier(result),
            bytes: result.data_bytes.clone(),
        },
        Some(event_item::Data::ErrorData(error)) => UnattachedData::Error {
            event: error_event_name(error.error_event),
            extra_error_info: error.extra_error_info,
            extra_error_info_name: error.extra_error_info_name.clone(),
            // A receive timeout is an outcome, and a lost VCI ends the link: neither is
            // a link error (design 8.11 "Link error").
            link_error: (error.error_event != PduErrorEvent::PduErrEvtRxTimeout as i32
                && error.error_event != PduErrorEvent::PduErrEvtLostCommToVci as i32)
                .then(|| link_error_kind(error.error_event)),
        },
        _ => return None,
    };
    Some(UnattachedItem {
        link: LOG_LINK,
        earlier_primitive: item.cop_handle.is_some(),
        owner: *owner,
        vci_us: item.timestamp,
        clock: clock_of(item),
        read: *read,
        data,
    })
}

/// A result's CAN identifier from its header (four bytes, most significant first), where the
/// worker split one off.
fn header_identifier(result: &vci_service_interface::ResultData) -> Option<u32> {
    result
        .extra_info
        .as_ref()
        .and_then(|extra| <[u8; 4]>::try_from(extra.header_bytes.as_slice()).ok())
        .map(u32::from_be_bytes)
}

/// Whether a result is an indication or a copy of a message the link sent, not a message the
/// ECU sent (RxFlag byte 3, ISO 22900-2:2022 D.2.2): transmit and start-of-message indications,
/// a transmitted message and a break.
fn is_indication(result: &vci_service_interface::ResultData) -> bool {
    use crate::comm_log::{
        RX_FLAG_BREAK, RX_FLAG_START_OF_MESSAGE, RX_FLAG_TX_INDICATION, RX_FLAG_TX_MSG_TYPE,
    };
    result.rx_flag.get(3).is_some_and(|flags| {
        flags
            & (RX_FLAG_TX_MSG_TYPE
                | RX_FLAG_START_OF_MESSAGE
                | RX_FLAG_BREAK
                | RX_FLAG_TX_INDICATION)
            != 0
    })
}

/// Whether a result is a start-of-message indication: RxFlag `START_OF_MESSAGE`, and neither a
/// transmit indication nor a transmitted message (ISO 22900-2:2022 D.2.2).
fn is_start_indication(result: &vci_service_interface::ResultData) -> bool {
    use crate::comm_log::{RX_FLAG_START_OF_MESSAGE, RX_FLAG_TX_INDICATION, RX_FLAG_TX_MSG_TYPE};
    result.rx_flag.get(3).is_some_and(|flags| {
        flags & RX_FLAG_START_OF_MESSAGE != 0
            && flags & (RX_FLAG_TX_INDICATION | RX_FLAG_TX_MSG_TYPE) == 0
    })
}

/// Whether a result is a transmit indication: RxFlag `TX_INDICATION` (ISO 22900-2:2022 D.2.2).
fn is_tx_indication(result: &vci_service_interface::ResultData) -> bool {
    use crate::comm_log::RX_FLAG_TX_INDICATION;
    result
        .rx_flag
        .get(3)
        .is_some_and(|flags| flags & RX_FLAG_TX_INDICATION != 0)
}

/// A primitive the host stopped waiting for before it ended ([`WorkerHost::settle_at_end`]).
#[derive(Debug, Clone)]
struct Stray {
    cop: ComPrimitiveHandle,
    /// The agent's own deadline for it, past every limit in force.
    deadline: tokio::time::Instant,
    /// Its cancel before a request failed: the worker may not answer, so the job end does not
    /// wait for it again.
    cancel_failed: bool,
    /// Its request, and the answer it waits for, so its end can be read as its exchange's.
    request: Vec<u8>,
    expectation: crate::send_receive::Expectation,
    /// The `seq` of its exchange's entry, when one was written (design 8.11 `primitive_ended`):
    /// only for an exchange the agent's deadline ended.
    seq: Option<u64>,
    /// What the wait had read of its end before the deadline: a final response and its times,
    /// and the error events, so its end at job end is read on from there.
    response: Option<Vec<u8>>,
    response_times: VciTimes,
    error: Option<i32>,
    errors: Vec<ErrorSeen>,
}

impl Stray {
    /// A wait for its end that goes on from what its exchange's wait had read, so its end is the
    /// one the exchange would have had (design 8.11 `primitive_ended`).
    fn progress(&self) -> Progress {
        let mut progress = Progress::expecting(self.expectation.clone());
        progress.response.clone_from(&self.response);
        progress.response_times = self.response_times;
        progress.error = self.error;
        progress.errors.clone_from(&self.errors);
        // Its end is read for its exchange's `primitive_ended` entry (ADR-386). A final response
        // read before the deadline is not in the exchange's entry: it is the first read.
        progress.track_reads = true;
        progress.early_response = self
            .response_times
            .rx_end
            .filter(|_| self.response.is_some())
            .zip(self.response_times.response_read_us)
            .map(|(end, read_us)| {
                (
                    EndItem::Response {
                        end,
                        tx_done: self.response_times.tx_done,
                        rx_start: self.response_times.rx_start,
                    },
                    read_us,
                )
            });
        progress
    }
}

/// A stray primitive the host cancelled before the link's next request, whose end may still be
/// read during that request's exchange (design 8.11 "Settlement"): it may have ended before the
/// cancel, with its items still on the event stream.
struct Settling {
    stray: Stray,
    progress: Progress,
    /// The worker carried out the cancel: the primitive was still running.
    carried_out: bool,
    /// Its end, once read.
    end: Option<SettlingEnd>,
    /// Its results and error events read, with when the host read them: the exchange in
    /// flight's unbound items of an earlier primitive when no end of it is written.
    items: Vec<Unbound>,
}

/// How a [`Settling`] primitive ended.
enum SettlingEnd {
    /// It ended, as its exchange's wait would have read it.
    Ended(Result<Vec<u8>, HostError>),
    /// The agent's cancel ended it.
    Cancelled,
}

impl Settling {
    /// Whether `data` is for its end: its own items, lost events, and a lost VCI of no
    /// primitive, until its end is read. An indication of no primitive stays with the exchange
    /// in flight, which may own it.
    fn takes(&self, data: &Option<event_notification::EventData>) -> bool {
        self.end.is_none()
            && match data {
                Some(event_notification::EventData::Lost(_)) => true,
                Some(event_notification::EventData::Item(item)) => {
                    item.cop_handle == Some(self.stray.cop) || ends_primitive(item, self.stray.cop)
                }
                None => false,
            }
    }

    /// Reads one of its notifications, read by the host at `read`. Its cancelled status ends
    /// it with the end read before, as the job end's cancel does, else, when the worker carried
    /// out the agent's cancel, as that cancel; any other end, and a cancelled status the agent's
    /// cancel did not give, is the one its exchange's wait would have read.
    fn read(&mut self, data: Option<event_notification::EventData>, read: Option<AgentNow>) {
        if let Some(event_notification::EventData::Item(item)) = &data
            && item.cop_handle == Some(self.stray.cop)
            && matches!(
                item.data,
                Some(event_item::Data::ResultData(_) | event_item::Data::ErrorData(_))
            )
        {
            self.items.push(Unbound {
                item: item.clone(),
                read,
                // Tagged with its exchange once the request's wait is over.
                owner: None,
            });
        }
        let cancelled = matches!(
            &data,
            Some(event_notification::EventData::Item(EventItem {
                data: Some(event_item::Data::CopStatus(status)),
                ..
            })) if *status == PduComPrimitiveStatus::PduCopstCancelled as i32
        );
        let read_before = self.progress.end_read();
        if let Some(result) = self.progress.on_read(data, self.stray.cop, read) {
            self.end = Some(match (cancelled, read_before) {
                (true, Some(result)) => SettlingEnd::Ended(result),
                (true, None) if self.carried_out => SettlingEnd::Cancelled,
                _ => SettlingEnd::Ended(result),
            });
        }
    }
}

/// Whether `data` ends the exchange in flight as well as a [`Settling`] primitive: lost events,
/// or a lost VCI of no primitive.
fn shared_end(data: &Option<event_notification::EventData>) -> bool {
    match data {
        Some(event_notification::EventData::Lost(_)) => true,
        Some(event_notification::EventData::Item(item)) => {
            item.cop_handle.is_none()
                && matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(data))
                        if data.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32
                )
        }
        None => false,
    }
}

/// How a stray primitive ended, for its `primitive_ended` entry.
enum StrayEnd<'a> {
    /// It ended, as its exchange's wait would have read it.
    Ended(&'a Result<Vec<u8>, HostError>),
    /// The agent cancelled it.
    Cancelled,
}

/// Whether `item` shows that the primitive `cop` has ended: its own finished or cancelled
/// status, or a lost VCI, whose primitives the worker cancels (ADR-289).
fn ends_primitive(item: &EventItem, cop: ComPrimitiveHandle) -> bool {
    match &item.data {
        Some(event_item::Data::CopStatus(status)) => {
            item.cop_handle == Some(cop)
                && (*status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || *status == PduComPrimitiveStatus::PduCopstCancelled as i32)
        }
        Some(event_item::Data::ErrorData(data)) => {
            item.cop_handle.is_none_or(|handle| handle == cop)
                && data.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32
        }
        _ => false,
    }
}

/// The clock the worker says an item's time is on (design 8.11 "On one clock", ADR-353), or none
/// when it does not say. The agent takes it from the item alone, never from which worker or API
/// gave it (design principle 6).
fn clock_of(item: &EventItem) -> Option<Clock> {
    clock_named(item.timestamp_clock)
}

/// Reads the VCI's clock (`GetTimestamp` of `module_handle`), with the clock the worker says it
/// is on; `None` when the read fails.
pub(crate) async fn read_vci_clock<C: WorkerApi>(
    client: &mut C,
    module_handle: vci_service_interface::ModuleHandle,
    deadline: Duration,
) -> Option<VciTime> {
    let read = unary(
        deadline,
        "GetTimestamp",
        client.get_timestamp(GetTimestampRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    match read {
        Ok(reading) => Some(VciTime::new(
            u64::from(reading.timestamp),
            clock_named(reading.timestamp_clock),
        )),
        Err(error) => {
            tracing::debug!(?error, "reading the VCI's clock failed");
            None
        }
    }
}

/// The clock a worker's `TimestampClock` value names, or none when it names none.
fn clock_named(clock: i32) -> Option<Clock> {
    match TimestampClock::try_from(clock) {
        Ok(TimestampClock::Device) => Some(Clock::Device),
        Ok(TimestampClock::Worker) => Some(Clock::Worker),
        Ok(TimestampClock::Unspecified) | Err(_) => None,
    }
}

const SID_REQUEST_DOWNLOAD: u8 = 0x34;

/// One error event the worker reported: its code and its extra error information
/// (ISO 22900-2:2022 D.4.2), the code and the name the worker gives it (ADR-351), and its VCI
/// time with the clock its item names (ADR-353).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorSeen {
    event: i32,
    extra: Option<u32>,
    extra_name: Option<String>,
    at: Option<VciTime>,
}

impl ErrorSeen {
    fn of(data: &ErrorData, at: VciTime) -> Self {
        Self {
            event: data.error_event,
            extra: data.extra_error_info,
            extra_name: data.extra_error_info_name.clone(),
            at: Some(at),
        }
    }
}

/// An event item's VCI time, on the clock the item names.
fn vci_time(item: &EventItem) -> VciTime {
    VciTime::new(u64::from(item.timestamp), clock_of(item))
}

/// How a primitive ended: its outcome, the error events it reported before it ended, in order, the receive timeout if the
/// worker reported one, the lost VCI if the event stream reported it, and the items of no
/// primitive or of an earlier one read meanwhile.
struct Sent {
    result: Result<Vec<u8>, HostError>,
    error_events: Vec<ErrorSeen>,
    /// How many of `error_events` were read before the final response, when there was one.
    errors_before_response: Option<usize>,
    rx_timeout: Option<ErrorSeen>,
    vci_lost: Option<ErrorSeen>,
    unbound: Vec<Unbound>,
    /// The response pendings the worker passed on, bound to the primitive, in order.
    pendings: Vec<PendingRead>,
    /// See `Progress::rejected`.
    rejected: Vec<RejectedRead>,
    /// See `Progress::last_rejected`.
    last_rejected: bool,
    /// See `Progress::bound_start`.
    bound_start: bool,
    /// See `Progress::bound_tx`.
    bound_tx: Option<VciTime>,
    /// See `Progress::events_lost`.
    events_lost: Option<Option<AgentNow>>,
    /// The agent's own deadline ended the wait, not the worker: the VCI had not ended the
    /// exchange (design 8.11 `vci_did_not_end`).
    agent_deadline: bool,
    /// The VCI times the final response's item gave: its end, and the transmit done and start
    /// it may carry.
    response_times: VciTimes,
    /// The reading of the VCI's clock taken when the agent's deadline ended the wait, if the
    /// read succeeded.
    deadline_reading: Option<ClockReading>,
}

impl Sent {
    /// A request whose primitive did not start, ending as `result`.
    fn not_started(result: Result<Vec<u8>, HostError>) -> Self {
        Self {
            result,
            error_events: Vec::new(),
            errors_before_response: None,
            rx_timeout: None,
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            agent_deadline: false,
            response_times: VciTimes::default(),
            deadline_reading: None,
        }
    }
}

/// A response pending (0x78) bound to the primitive (design 8.11 `pending`): its VCI time and
/// clock as the worker gave them, and the agent's clocks when the host read it, if it has a log.
#[derive(Debug)]
struct PendingRead {
    vci_us: u64,
    clock: Option<Clock>,
    read: Option<AgentNow>,
}

/// A result bound to the primitive that does not answer its request (design 8.11 `rejected`,
/// ADR-359): its response identifier when the worker split one off, its bytes, its VCI time and
/// clock, why it was not taken, and the agent's clocks when the host read it, if it has a log.
#[derive(Debug)]
struct RejectedRead {
    identifier: Option<u32>,
    bytes: Vec<u8>,
    vci_us: u64,
    clock: Option<Clock>,
    reason: RejectedReason,
    read: Option<AgentNow>,
}

/// An item of the link read while a primitive ran that is not that primitive's: one of no
/// primitive or of an earlier one (design 8.11 "Link settings for the log"), with the agent's
/// clocks when it was read, if the host has a log.
#[derive(Debug)]
struct Unbound {
    item: EventItem,
    read: Option<AgentNow>,
    /// The `seq` of the exchange whose primitive it is bound to, when the host knows it: the
    /// stray primitive cancelled before this request (design 8.11 "Settlement", ADR-372).
    owner: Option<u64>,
}

/// The error events the worker reported in an exchange beside the one it ended on: faults seen
/// in the exchange (design 8.11 "Link error"), whether a response followed them or the
/// exchange then timed out or lost its events. A primitive that failed ends on the last.
fn also_seen<'a>(
    result: &Result<Vec<u8>, HostError>,
    error_events: &'a [ErrorSeen],
) -> &'a [ErrorSeen] {
    match (result, error_events) {
        (Err(HostError::PrimitiveFailed(_)), [earlier @ .., _]) => earlier,
        _ => error_events,
    }
}

/// Whether `sent` ended with a final response that every link error it reported came before, in
/// the order the items were read (design 8.11 "Matched answer"). The VCI's times are not used:
/// its clocks are not aligned with the agent's.
fn errors_before_answer(sent: &Sent) -> bool {
    sent.result.is_ok() && sent.errors_before_response == Some(sent.error_events.len())
}

/// The link errors of the events `sent` reported beside its end, every one, so the
/// classification takes the most severe; `detail` names the last unless it already names the
/// event that ended the exchange (a lost VCI, a failed primitive). A receive timeout is named
/// but is no link error. The named event's extra error information goes in too, its code and its
/// name as the worker gave them (ADR-351): the agent does not interpret either.
fn events_seen(sent: &Sent, detail: &mut Detail) -> (Vec<LinkErrorKind>, Option<VciTime>) {
    named_events(
        &sent.result,
        &sent.error_events,
        sent.vci_lost.as_ref(),
        sent.rx_timeout.as_ref(),
        detail,
    )
}

/// `events_seen` over an end's parts: `result`, the error events read beside it, the lost VCI
/// and the receive timeout.
fn named_events(
    result: &Result<Vec<u8>, HostError>,
    error_events: &[ErrorSeen],
    vci_lost: Option<&ErrorSeen>,
    rx_timeout: Option<&ErrorSeen>,
    detail: &mut Detail,
) -> (Vec<LinkErrorKind>, Option<VciTime>) {
    let seen = also_seen(result, error_events);
    // The event `sent_end` named, when it named one: the one that ended the exchange.
    let named = match result {
        Err(HostError::PrimitiveFailed(_)) => error_events.last(),
        Err(HostError::VciLost { .. }) => vci_lost,
        _ => None,
    };
    let named = if detail.error_event.is_some() {
        named
    } else {
        let named = seen.last().or(rx_timeout);
        detail.error_event = named.map(|seen| error_event_name(seen.event));
        named
    };
    detail.extra_error_info = named.and_then(|seen| seen.extra);
    detail.extra_error_info_name = named.and_then(|seen| seen.extra_name.clone());
    let kinds = seen
        .iter()
        .map(|seen| link_error_kind(seen.event))
        .collect();
    // The entry's error time is the named event's (design 8.11 `times`).
    (kinds, named.and_then(|seen| seen.at))
}

/// Whether `request` asks for its positive response to be suppressed: bit 7 of the
/// sub-function of a service that has one (ISO 14229-1:2026 8.2.2: DiagnosticSessionControl,
/// ECUReset, SecurityAccess, CommunicationControl, Authentication, ReadDTCInformation,
/// TesterPresent,
/// ControlDTCSetting, ResponseOnEvent, LinkControl, DynamicallyDefineDataIdentifier,
/// RoutineControl), which the ECU leaves unanswered when the request succeeds.
fn suppresses_positive_response(request: &[u8]) -> bool {
    matches!(
        request,
        [0x10 | 0x11 | 0x19 | 0x27 | 0x28 | 0x29 | 0x2C | 0x31 | 0x3E | 0x85 | 0x86 | 0x87, sub, ..]
            if sub & 0x80 != 0
    )
}

/// How a sent request ended, as its exchange entry gives it (design 8.11, `outcome`), and the
/// internal names behind it. Whether the request was sent comes from [`HostError::send_state`].
///
/// A positive response the agent cannot use is `positive_unusable`: a TransferData answer that
/// does not echo the block's counter, and a RequestDownload answer with no readable
/// maxNumberOfBlockLength (ISO 14229-1:2026 14.2.3, 14.4), which the transfer then refuses.
fn sent_end(
    request: &[u8],
    result: &Result<Vec<u8>, HostError>,
    negative: &NegativeShape,
    sid: u8,
) -> (End, Detail) {
    let error = match result {
        Ok(response) if negative.is_negative(sid, response) => {
            return (End::Negative(response.clone()), Detail::default());
        }
        Ok(response) => {
            let unusable = match (request, response.as_slice()) {
                ([0x36, counter, ..], [0x76, echoed, ..]) => echoed != counter,
                ([0x36, ..], _) => true,
                ([SID_REQUEST_DOWNLOAD, ..], [0x74, ..]) => max_block_length(response).is_none(),
                _ => false,
            };
            let end = if unusable {
                End::PositiveUnusable(response.clone())
            } else {
                End::Positive(response.clone())
            };
            return (end, Detail::default());
        }
        Err(error) => error,
    };
    let mut detail = Detail {
        host_error: Some(host_error_name(error)),
        ..Detail::default()
    };
    let end = match error {
        HostError::NoResponse => End::NoResponse,
        // The agent did not start it: an earlier primitive still holds the link.
        HostError::LinkHeld => End::NotSent(RefusalCause::LinkHeld),
        HostError::EventsLost => End::Unknown(UnknownCause::EventsLost),
        HostError::EventStreamEnded => End::Unknown(UnknownCause::EventsEnded),
        HostError::WorkerUnresponsive(_) => End::Unknown(UnknownCause::WorkerNotAnswering),
        HostError::Rpc { rpc, status } => {
            detail.grpc_status = Some(grpc_code_name(status.code()));
            // A start the worker refused and names a reason for was not sent (ADR-350 item 2):
            // the worker's own reasons, `system`. Without a reason the send stays unknown.
            if error.send_state() == SendState::NotSent {
                detail.worker_detail = refusal_reason(status).map(|r| r.as_str_name().to_owned());
                return (End::NotSent(RefusalCause::SystemOwnReasons), detail);
            }
            End::Unknown(if *rpc == "StartComPrimitive" {
                UnknownCause::PrimitiveFailedToStart
            } else {
                UnknownCause::RpcFailure
            })
        }
        HostError::Setup(_) => End::Unknown(UnknownCause::PrimitiveFailedToStart),
        HostError::VciLost { refused_after_loss } => {
            // Both cases of `VciLost` come from this event.
            detail.error_event = Some(error_event_name(
                PduErrorEvent::PduErrEvtLostCommToVci as i32,
            ));
            End::VciLost {
                refused_after_loss: *refused_after_loss,
            }
        }
        HostError::PrimitiveFailed(code) => {
            detail.error_event = Some(error_event_name(*code));
            End::LinkError(link_error_kind(*code))
        }
        // `send_recv_async` gives no other error.
        _ => End::Unknown(UnknownCause::RpcFailure),
    };
    (end, detail)
}

/// The reason a worker gave for refusing to start a primitive (ADR-350 item 2), when it gave
/// one.
fn refusal_reason(status: &tonic::Status) -> Option<vci_service_interface::RefusalReason> {
    vci_service_interface::error_detail_from_status(status)
        .and_then(|detail| detail.refusal)
        .and_then(|reason| vci_service_interface::RefusalReason::try_from(reason).ok())
        .filter(|reason| *reason != vci_service_interface::RefusalReason::Unspecified)
}

/// The kind of link error an error event names (design 8.11 "Link error"; ISO 22900-2:2022
/// D.4.1 lists the events).
fn link_error_kind(code: i32) -> LinkErrorKind {
    match PduErrorEvent::try_from(code) {
        Ok(PduErrorEvent::PduErrEvtFrameStruct) => LinkErrorKind::Frame,
        Ok(PduErrorEvent::PduErrEvtTxError) => LinkErrorKind::Transmit,
        Ok(PduErrorEvent::PduErrEvtTesterPresentError) => LinkErrorKind::TesterPresent,
        Ok(PduErrorEvent::PduErrEvtRscLocked) => LinkErrorKind::ResourceLocked,
        Ok(PduErrorEvent::PduErrEvtRxError) => LinkErrorKind::Receive,
        Ok(PduErrorEvent::PduErrEvtProtErr) => LinkErrorKind::Protocol,
        Ok(PduErrorEvent::PduErrEvtVciHardwareFault) => LinkErrorKind::VciHardwareFault,
        Ok(PduErrorEvent::PduErrEvtInitError) => LinkErrorKind::Initialisation,
        _ => LinkErrorKind::Other,
    }
}

/// Why the agent refused `request` for `error` (design 8.11, `not_sent`).
fn refusal_cause(request: &[u8], error: &HostError) -> RefusalCause {
    match error {
        // A service number no request can carry: the entry has no bytes for it.
        HostError::BadService(_) | HostError::UndeclaredService(_) => {
            RefusalCause::UndeclaredService
        }
        _ if request.is_empty() => RefusalCause::EmptyRequest,
        HostError::EmptyRequest => RefusalCause::EmptyRequest,
        HostError::NotAllowed(_)
        | HostError::UseFlashTransfer
        | HostError::WrongService { .. }
        | HostError::DownloadBySendReceive(_)
        | HostError::TransferInterrupted(_) => RefusalCause::Policy,
        HostError::NoTransferActive => RefusalCause::NoTransferActive,
        HostError::BlockTooLong { .. } => RefusalCause::BlockTooLong,
        HostError::NoBlockLength { .. } => RefusalCause::EarlierResponse,
        _ => RefusalCause::PreparationError,
    }
}

/// The name of `error`'s variant, which an exchange entry's `detail` carries in place of its
/// text (the text prints response bytes, which `redact` must see first).
fn host_error_name(error: &HostError) -> &'static str {
    match error {
        HostError::Rpc { .. } => "Rpc",
        HostError::WorkerUnresponsive(_) => "WorkerUnresponsive",
        HostError::LinkHeld => "LinkHeld",
        HostError::EventStreamEnded => "EventStreamEnded",
        HostError::NoResponse => "NoResponse",
        HostError::EventsLost => "EventsLost",
        HostError::VciLost { .. } => "VciLost",
        HostError::PrimitiveFailed(_) => "PrimitiveFailed",
        HostError::BadService(_) => "BadService",
        HostError::NotAllowed(_) => "NotAllowed",
        HostError::NoTransferActive => "NoTransferActive",
        HostError::TransferRefused { .. } => "TransferRefused",
        HostError::TransferEchoMismatch { .. } => "TransferEchoMismatch",
        HostError::BlockTooLong { .. } => "BlockTooLong",
        HostError::NoBlockLength { .. } => "NoBlockLength",
        HostError::UseFlashTransfer => "UseFlashTransfer",
        HostError::NoArtifact => "NoArtifact",
        HostError::ArtifactRange { .. } => "ArtifactRange",
        HostError::Unsupported(_) => "Unsupported",
        HostError::EmptyRequest => "EmptyRequest",
        HostError::UndeclaredService(_) => "UndeclaredService",
        HostError::WrongService { .. } => "WrongService",
        HostError::DeclaredLink(_) => "DeclaredLink",
        HostError::BadNegativeShape(_) => "BadNegativeShape",
        HostError::DownloadBySendReceive(_) => "DownloadBySendReceive",
        HostError::TransferInterrupted(_) => "TransferInterrupted",
        HostError::Setup(_) => "Setup",
        HostError::InputNotEstablished(_) => "InputNotEstablished",
    }
}

impl crate::oplog::LogSafe for HostError {
    /// The error's text without the response bytes some variants print (an NRC is kept), and
    /// with a failed RPC's status as its code: the status message is the worker's text.
    fn log_text(&self) -> String {
        match self {
            HostError::Rpc { rpc, status } => {
                format!("{rpc} failed: {}", grpc_code_name(status.code()))
            }
            HostError::TransferRefused { block, response } => match response.as_slice() {
                [0x7F, _, nrc] => {
                    format!("the ECU refused TransferData block {block} with NRC {nrc:#04x}")
                }
                _ => format!("the ECU refused TransferData block {block}"),
            },
            HostError::TransferEchoMismatch { block, counter, .. } => format!(
                "the ECU's TransferData response to block {block} (counter {counter:#04x}) is wrong"
            ),
            HostError::NoBlockLength { block, .. } => format!(
                "the ECU's RequestDownload response has no readable maxNumberOfBlockLength, so TransferData block {block} is not sent (ADR-336)"
            ),
            other => other.to_string(),
        }
    }

    fn log_external(&self) -> Option<crate::oplog::ExternalField> {
        match self {
            HostError::Rpc { status, .. } if !status.message().is_empty() => {
                Some(crate::oplog::external(status.message()))
            }
            _ => None,
        }
    }
}

/// The name of a gRPC status code, as `detail` carries it.
fn grpc_code_name(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "Ok",
        tonic::Code::Cancelled => "Cancelled",
        tonic::Code::Unknown => "Unknown",
        tonic::Code::InvalidArgument => "InvalidArgument",
        tonic::Code::DeadlineExceeded => "DeadlineExceeded",
        tonic::Code::NotFound => "NotFound",
        tonic::Code::AlreadyExists => "AlreadyExists",
        tonic::Code::PermissionDenied => "PermissionDenied",
        tonic::Code::ResourceExhausted => "ResourceExhausted",
        tonic::Code::FailedPrecondition => "FailedPrecondition",
        tonic::Code::Aborted => "Aborted",
        tonic::Code::OutOfRange => "OutOfRange",
        tonic::Code::Unimplemented => "Unimplemented",
        tonic::Code::Internal => "Internal",
        tonic::Code::Unavailable => "Unavailable",
        tonic::Code::DataLoss => "DataLoss",
        tonic::Code::Unauthenticated => "Unauthenticated",
    }
}

/// The VM position of the request a step sends, which a host's exchange entries carry (design
/// 8.11, `at`). The runner tells the host where each step begins and that it ended.
pub trait ProcedurePosition {
    /// The VM is about to run the instruction at `pc`, `steps` into the program.
    fn step_begins(&mut self, _program: &diag_ir::Program, _pc: u32, _steps: u64) {}
    /// The step ended: what the host sends now is not the procedure's.
    fn step_ends(&mut self) {}
    /// The runner or the restart begins reads of its own, outside a step (`Some`), or ends
    /// them (`None`): what the host sends meanwhile has that origin.
    fn own_reads(&mut self, _reads: Option<OwnRead>) {}
}

/// Reads the runner and the restart send on their own, outside the VM's steps, by the origin
/// the communication log gives them (design 8.11, `origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnRead {
    /// The identity reads: at a plan's entry, and every identity read of a restart (its gates
    /// included) or a checkpoint resume (8.2.5, 8.9.1).
    Identification,
    /// The write guard's reads of the ECU's state (8.9.1).
    Guard,
    /// The reads of the flash preconditions, wherever a restart checks them (8.9).
    Precondition,
    /// A restart's confirmation of the default session, and its other checks (8.2.5).
    RestartCheck,
    /// A restart's ECUReset (8.2.5).
    Teardown,
}

impl OwnRead {
    fn origin(self) -> Origin {
        match self {
            Self::Identification => Origin::Identification,
            Self::Guard => Origin::Guard,
            Self::Precondition => Origin::Precondition,
            Self::RestartCheck => Origin::RestartCheck,
            Self::Teardown => Origin::Teardown,
        }
    }
}

impl<C: WorkerApi> ProcedurePosition for WorkerHost<C> {
    fn step_begins(&mut self, program: &diag_ir::Program, pc: u32, steps: u64) {
        if self.comm_log.is_some() {
            self.at = Some(at_of(program, pc, steps));
        }
    }

    fn step_ends(&mut self) {
        self.at = None;
    }

    fn own_reads(&mut self, reads: Option<OwnRead>) {
        self.own = reads;
    }
}

/// The `at` of the instruction at `pc`: its source position, from the source map entry that
/// covers it (an entry covers its instruction and those after it up to the next, ADR-304, so
/// the map is in pc order; an unordered map from another producer gives a wrong position, not a
/// failure), and
/// the attributes of every section it is in, outermost first.
fn at_of(program: &diag_ir::Program, pc: u32, steps: u64) -> At {
    let covering = program.source_map.partition_point(|span| span.pc <= pc);
    let source = covering
        .checked_sub(1)
        .map(|at| SourcePos::from(&program.source_map[at]));
    let sections = program
        .sections
        .iter()
        .filter(|section| section.start_pc <= pc && pc < section.end_pc)
        .map(SectionAttrs::from)
        .collect();
    At {
        pc,
        steps,
        source,
        sections,
    }
}

/// The positive and the negative response to the request `progress` waits for.
fn expected_responses(progress: &Progress) -> Vec<ExpectedResponseData> {
    let expected = |response_type, mask: &[u8], pattern: Vec<u8>| ExpectedResponseData {
        response_type,
        acceptance_id: 0,
        mask_data: mask.to_vec(),
        pattern_data: pattern,
        unique_resp_ids: vec![],
    };
    let prefix = &progress.positive_prefix;
    let negative = progress.negative.pattern(progress.sid);
    vec![
        expected(0, &vec![0xFF; prefix.len()], prefix.clone()),
        expected(1, &vec![0xFF; negative.len()], negative),
    ]
}

/// How a positive response to `request` begins: the SID plus 0x40, then what the server echoes
/// of the request (ISO 14229-1:2026 clauses 9.4, 9.7, 10.2 and 11.3). A sub-function is echoed
/// without its suppress bit. A DID is matched only when one was requested, since the
/// standard does not fix the order of several in the response.
pub(crate) fn positive_prefix(request: &[u8]) -> Vec<u8> {
    match request {
        [0x22, hi, lo] => vec![0x62, *hi, *lo],
        [0x19, sub, ..] => vec![0x59, sub & 0x7F],
        [0x27, sub, ..] => vec![0x67, sub & 0x7F],
        [0x3E, sub, ..] => vec![0x7E, sub & 0x7F],
        [sid, ..] => vec![sid.wrapping_add(0x40)],
        [] => Vec::new(),
    }
}

/// What the events of one send-receive have shown so far.
#[derive(Debug)]
struct Progress {
    sid: u8,
    positive_prefix: Vec<u8>,
    negative: NegativeShape,
    response: Option<Vec<u8>>,
    error: Option<i32>,
    /// Every error event but the receive timeout and a lost VCI, in order, for the
    /// communication log.
    errors: Vec<ErrorSeen>,
    /// How many of `errors` were read when the final response was taken: the ones before it, in
    /// the order the link's items were read.
    errors_before_response: Option<usize>,
    /// The receive timeout the worker reported, which ends the exchange without a response.
    rx_timeout: Option<ErrorSeen>,
    /// The lost VCI the worker reported, which ends the exchange at once.
    vci_lost: Option<ErrorSeen>,
    /// The result and error items of no primitive or of an earlier one, in order, for the
    /// communication log. Status and information items are not kept.
    unbound: Vec<Unbound>,
    /// The response pendings bound to the primitive, in order, for the communication log.
    pendings: Vec<PendingRead>,
    /// The results bound to the primitive that do not answer the request, in order (design 8.11
    /// `rejected`, ADR-359).
    rejected: Vec<RejectedRead>,
    /// The last result of the primitive was a rejected one: no pending or answer came after it.
    last_rejected: bool,
    /// A start-of-message indication bound to the primitive was read: a response may be under
    /// way (design 8.11 `vci_did_not_end`).
    bound_start: bool,
    /// The time of a transmit indication bound to the primitive, the latest read: it confirms
    /// the send (design 8.11 `sent_evidence` `tx_indication`).
    bound_tx: Option<VciTime>,
    /// The worker reported lost events: `Some` with the agent's clocks when the host read it, if
    /// it has a log.
    events_lost: Option<Option<AgentNow>>,
    /// The VCI times the final response's item gave, for the communication log.
    response_times: VciTimes,
    /// The primitive's end (finished or cancelled) was read.
    ended: bool,
    /// Whether the items read are kept for the communication log's `primitive_ended` entry
    /// (a stray primitive's wait, ADR-386).
    track_reads: bool,
    /// The items the last notification added, which [`Progress::on_read`] stamps and moves to
    /// `reads`.
    new_items: Vec<EndItem>,
    /// Every item read for the primitive's end after its exchange's entry, in read order, each
    /// with when the host read it.
    reads: Vec<EndRead>,
    /// The final response read before the agent's deadline, which its exchange's entry does not
    /// carry: the first read of the end, with the agent's monotonic time it was read at.
    early_response: Option<(EndItem, u64)>,
}

impl Progress {
    /// The end the primitive's finished status would give, when what was read already decides
    /// it: a final response, or the VCI's receive timeout (with the error read beside it, as the
    /// finished status gives it).
    fn end_read(&self) -> Option<Result<Vec<u8>, HostError>> {
        match (&self.response, self.error) {
            (Some(response), _) => Some(Ok(response.clone())),
            (None, Some(code)) if self.rx_timeout.is_some() => {
                Some(Err(HostError::PrimitiveFailed(code)))
            }
            (None, None) if self.rx_timeout.is_some() => Some(Err(HostError::NoResponse)),
            _ => None,
        }
    }

    /// [`Progress::on_notification`] for a wait that keeps what it reads for the log's
    /// `primitive_ended` entry (ADR-386): each item the notification adds is read at `read`, the
    /// agent's clocks when the host read it. With no log (`read` `None`) nothing is kept.
    fn on_read(
        &mut self,
        data: Option<event_notification::EventData>,
        cop: ComPrimitiveHandle,
        read: Option<AgentNow>,
    ) -> Option<Result<Vec<u8>, HostError>> {
        let done = self.on_notification(data, cop);
        let items = std::mem::take(&mut self.new_items);
        if let Some(read) = read {
            self.reads.extend(items.into_iter().map(|mut item| {
                if let EndItem::Unattached(unattached) = &mut item {
                    unattached.read = Some(read);
                }
                EndRead { read, item }
            }));
        }
        done
    }

    /// Takes every item read for the end, in read order, as the end `result` has them: the
    /// final response read before the deadline first, with `epoch` the agent's. The last error
    /// read is the one the entry names; the last of a failed primitive's is that end itself,
    /// not a link error beside it. A cancelled primitive (`result` `None`) has no end to
    /// carry them, so its errors stay link errors.
    fn take_reads(
        &mut self,
        epoch: u32,
        result: Option<&Result<Vec<u8>, HostError>>,
    ) -> Vec<EndRead> {
        let mut reads = Vec::new();
        if let Some((item, read_us)) = self.early_response.take() {
            reads.push(EndRead {
                read: AgentNow {
                    epoch,
                    monotonic_us: read_us,
                    wall_unix_ms: 0,
                },
                item,
            });
        }
        reads.append(&mut self.reads);
        if let Some(result) = result
            && let Some(EndItem::Error(error)) = reads
                .iter_mut()
                .rev()
                .find_map(|read| matches!(read.item, EndItem::Error(_)).then_some(&mut read.item))
        {
            error.named = true;
            if matches!(result, Err(HostError::PrimitiveFailed(_))) {
                error.kind = None;
            }
        }
        reads
    }

    fn for_request(request: &[u8]) -> Result<Self, HostError> {
        let sid = *request.first().ok_or(HostError::BadService(0))?;
        Ok(Self {
            sid,
            positive_prefix: positive_prefix(request),
            negative: NegativeShape::uds(),
            response: None,
            error: None,
            errors: Vec::new(),
            errors_before_response: None,
            rx_timeout: None,
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            response_times: VciTimes::default(),
            ended: false,
            track_reads: false,
            new_items: Vec::new(),
            reads: Vec::new(),
            early_response: None,
        })
    }

    /// Waits for the answer `expected` describes (ADR-321).
    fn expecting(expected: Expectation) -> Self {
        Self {
            sid: expected.sid,
            positive_prefix: expected.positive,
            negative: expected.negative,
            response: None,
            error: None,
            errors: Vec::new(),
            errors_before_response: None,
            rx_timeout: None,
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            response_times: VciTimes::default(),
            ended: false,
            track_reads: false,
            new_items: Vec::new(),
            reads: Vec::new(),
            early_response: None,
        }
    }

    /// Whether `response` answers the request: a positive response that begins as expected,
    /// or a complete negative response for its SID. Stricter than the worker's match, which
    /// also accepts a response shorter than the pattern.
    fn answers(&self, response: &[u8]) -> bool {
        if response.first() == Some(&self.negative.marker) {
            self.negative.is_negative(self.sid, response)
        } else {
            response.starts_with(&self.positive_prefix)
        }
    }

    /// Takes one notification from the link's event stream; returns the outcome once the
    /// primitive `cop` has ended, or at once if the worker reports lost events.
    fn on_notification(
        &mut self,
        data: Option<event_notification::EventData>,
        cop: ComPrimitiveHandle,
    ) -> Option<Result<Vec<u8>, HostError>> {
        match data {
            Some(event_notification::EventData::Item(item)) => self.on_event(item, cop),
            // A dropped event may have been this primitive's response or end.
            Some(event_notification::EventData::Lost(_)) => Some(Err(HostError::EventsLost)),
            None => None,
        }
    }

    /// Takes one event; returns the outcome once the primitive `cop` has ended.
    fn on_event(
        &mut self,
        item: EventItem,
        cop: ComPrimitiveHandle,
    ) -> Option<Result<Vec<u8>, HostError>> {
        // A lost VCI ends the pending primitive at once (ADR-289). The worker reports it for the
        // whole link, with no primitive handle, before it cancels the primitives; one bound to
        // this primitive counts the same.
        if item.cop_handle.is_none_or(|handle| handle == cop)
            && let Some(event_item::Data::ErrorData(data)) = &item.data
            && data.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32
        {
            self.vci_lost = Some(ErrorSeen::of(data, vci_time(&item)));
            if self.track_reads {
                // The event that ends the primitive: no link error beside its end.
                self.new_items
                    .push(EndItem::Error(end_error(data, vci_time(&item), None)));
            }
            return Some(Err(HostError::VciLost {
                refused_after_loss: false,
            }));
        }
        // Events of earlier primitives, and of none, are not this request's; their results and
        // errors are kept for the communication log.
        if item.cop_handle != Some(cop) {
            if matches!(
                item.data,
                Some(event_item::Data::ResultData(_) | event_item::Data::ErrorData(_))
            ) {
                let unbound = Unbound {
                    item,
                    read: None,
                    owner: None,
                };
                if self.track_reads
                    && let Some(unattached) = unattached_item(&unbound)
                {
                    self.new_items.push(EndItem::Unattached(unattached));
                }
                self.unbound.push(unbound);
            }
            return None;
        }
        // A result of the primitive that does not answer the request, and is no indication or
        // copy of a transmitted message, is kept for the log as rejected (design 8.11 `rejected`,
        // ADR-359).
        if let Some(event_item::Data::ResultData(result)) = &item.data
            && !self.answers(&result.data_bytes)
            && !is_indication(result)
        {
            self.rejected.push(RejectedRead {
                identifier: header_identifier(result),
                bytes: result.data_bytes.clone(),
                vci_us: u64::from(item.timestamp),
                clock: clock_of(&item),
                reason: if result.data_bytes.first() == Some(&self.negative.marker) {
                    RejectedReason::PatternMismatch
                } else {
                    RejectedReason::WrongEcho
                },
                read: None,
            });
        }
        if let Some(event_item::Data::ResultData(result)) = &item.data
            && !is_indication(result)
        {
            self.last_rejected = !self.answers(&result.data_bytes);
        }
        if let Some(event_item::Data::ResultData(result)) = &item.data
            && is_start_indication(result)
        {
            self.bound_start = true;
        }
        if let Some(event_item::Data::ResultData(result)) = &item.data
            && is_tx_indication(result)
        {
            self.bound_tx = Some(vci_time(&item));
            if self.track_reads {
                self.new_items.push(EndItem::TxIndication(vci_time(&item)));
            }
        }
        // A response pending the worker passed on is kept for the log (design 8.11 `pending`),
        // with the item's time and clock as the worker gave them (ADR-353).
        if let Some(event_item::Data::ResultData(result)) = &item.data
            && self.answers(&result.data_bytes)
            && self.negative.is_pending(self.sid, &result.data_bytes)
        {
            self.pendings.push(PendingRead {
                vci_us: u64::from(item.timestamp),
                clock: clock_of(&item),
                read: None,
            });
            if self.track_reads {
                // Its agent time is when the host read it, which the log takes.
                self.new_items.push(EndItem::Pending(Pending {
                    vci_us: Some(u64::from(item.timestamp)),
                    clock: clock_of(&item),
                    epoch: None,
                    agent_us: 0,
                    since_previous_ms: None,
                }));
            }
        }
        let at = vci_time(&item);
        match item.data {
            // A response pending the worker passes on, and anything that does not answer this
            // request, is not the final response (ADR-235 item 3).
            Some(event_item::Data::ResultData(result))
                if self.answers(&result.data_bytes)
                    && !self.negative.is_pending(self.sid, &result.data_bytes) =>
            {
                // The response's own item is its end; it may carry its request's transmit done
                // and its own start, on the same clock (ISO 22900-2:2022 10.1.4.11.4).
                let on_its_clock = |us: u32| VciTime::new(u64::from(us), at.clock);
                self.response_times = VciTimes {
                    tx_done: result.tx_msg_done_timestamp.map(on_its_clock),
                    rx_start: result.start_msg_timestamp.map(on_its_clock),
                    rx_end: Some(at),
                    error: None,
                    deadline: None,
                    response_read_us: None,
                    rx_timeout: None,
                };
                if self.track_reads
                    && let Some(end) = self.response_times.rx_end
                {
                    self.new_items.push(EndItem::Response {
                        end,
                        tx_done: self.response_times.tx_done,
                        rx_start: self.response_times.rx_start,
                    });
                }
                self.response = Some(result.data_bytes);
                self.errors_before_response = Some(self.errors.len());
            }
            // A receive timeout means the ECU did not answer (or did not accept the rest of a
            // segmented request), which is `NoResponse`, not a failed transmission.
            Some(event_item::Data::ErrorData(data))
                if data.error_event == PduErrorEvent::PduErrEvtRxTimeout as i32 =>
            {
                self.rx_timeout = Some(ErrorSeen::of(&data, at));
                if self.track_reads {
                    self.new_items.push(EndItem::RxTimeout(at));
                }
            }
            Some(event_item::Data::ErrorData(data)) => {
                self.error = Some(data.error_event);
                self.errors.push(ErrorSeen::of(&data, at));
                if self.track_reads {
                    self.new_items.push(EndItem::Error(end_error(
                        &data,
                        at,
                        Some(link_error_kind(data.error_event)),
                    )));
                }
            }
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
                self.ended = true;
                return Some(match (self.response.take(), self.error) {
                    (Some(response), _) => Ok(response),
                    (None, Some(code)) => Err(HostError::PrimitiveFailed(code)),
                    (None, None) => Err(HostError::NoResponse),
                });
            }
            _ => {}
        }
        None
    }
}

/// A host that counts the TransferData blocks the ECU confirmed (ADR-250), which the
/// journaling runner records (ADR-252).
pub trait TransferProgress {
    /// See [`WorkerHost::transfer_block_index`].
    fn transfer_block_index(&self) -> Option<u64>;
}

impl<C: WorkerApi> crate::document::Declarations for WorkerHost<C> {
    fn declarations(&self) -> Option<crate::document::JobDocument> {
        self.document.clone()
    }
}

impl<C: WorkerApi> TransferProgress for WorkerHost<C> {
    fn transfer_block_index(&self) -> Option<u64> {
        WorkerHost::<C>::transfer_block_index(self)
    }
}

/// The supply voltage comes from the VCI; the worker interface has no defined source for the
/// other inputs.
impl<C: WorkerApi> crate::inputs::RuntimeInputs for WorkerHost<C> {
    fn read(&mut self, input: diag_ir::RuntimeInput) -> Result<crate::inputs::Reading, HostError> {
        use crate::inputs::Reading;
        use diag_ir::RuntimeInput;
        match input {
            RuntimeInput::SupplyVoltageMillivolts => crate::inputs::read_supply_voltage(
                &self.handle,
                &mut self.client,
                &self.link,
                &mut self.vbatt_id,
                self.timings.unary,
            ),
            RuntimeInput::ExternalSupplyConnected
            | RuntimeInput::IgnitionOn
            | RuntimeInput::EngineRunning
            | RuntimeInput::VehicleSpeedKmh => Ok(Reading::CannotBeEstablished),
        }
    }
}

impl<C: WorkerApi> DiagHost for WorkerHost<C> {
    type Error = HostError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
        let before = self.log_clock();
        let request = match service_request_bytes(service, payload) {
            Ok(request) => request,
            Err(error) => {
                // No request can carry the service, so the entry has no bytes.
                self.log_refused(&[], before, &error, None);
                return Err(error);
            }
        };
        // Every TransferData goes through the block counter of `flash_transfer`.
        if let Err(error) = refuse_transfer_data(&request) {
            self.log_refused(&request, before, &error, None);
            return Err(error);
        }
        let response = self.send_recv(request.clone());
        self.transfer
            .after_request(&request, response.as_deref().map_err(|_| ()));
        response
    }

    // The request is sent as it is (ADR-321). It must start with the declared service's SID,
    // which the policy checked before the program ran (ADR-322 item 1), and the flash-plan
    // rules `Program::validate` cannot apply to it apply here (item 2). The answer is matched
    // against the service's positive response and the variant's negative-response shape
    // (ADR-309 item 4).
    fn send_receive(&mut self, service: u32, request: &[u8]) -> Result<Vec<u8>, HostError> {
        let before = self.log_clock();
        let expected = match crate::send_receive::prepare(
            self.document.as_ref(),
            service,
            request,
            self.transfer.accepted.is_some(),
        ) {
            Ok(expected) => expected,
            Err(error) => {
                self.log_refused(request, before, &error, None);
                return Err(error);
            }
        };
        let response = self.send_expecting(request.to_vec(), expected);
        self.transfer
            .after_request(request, response.as_deref().map_err(|_| ()));
        response
    }

    // `diag.read_dtc` lowers to a send-receive (ADR-309 item 4), so no request is built for
    // the instruction (ADR-342); the policy refuses it before the link opens.
    fn read_dtc(&mut self, _mask: u8) -> Result<Vec<u8>, HostError> {
        Err(HostError::Unsupported("ReadDtc"))
    }

    // Goes through `send_recv`, so the link's permission decides (ADR-235 item 8, ADR-247).
    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, HostError> {
        self.send_recv(routine_control_bytes(routine, sub, payload))
    }

    // Refused: an agent without a server has no key source, and the policy already refuses
    // the instruction before the link opens (ADR-259). Never `Ok(None)`: that means "still
    // waiting" and the VM would wait forever (ADR-235 item 5).
    fn security_access(
        &mut self,
        _inquiry: u64,
        _level: u8,
        _seed: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError> {
        Err(HostError::Unsupported("SecurityAccess"))
    }

    // The IR operand `block` is ignored: it is constant per instruction, while the wire counter
    // rises and wraps, so the counter comes from the host's own running count (ADR-250).
    fn flash_transfer(&mut self, _block: u32, data: &[u8]) -> Result<(), HostError> {
        let before = self.log_clock();
        let (index, counter) = match self.transfer.next() {
            Ok(next) => next,
            Err(error) => {
                // No transfer gives the block a counter: the entry has the SID alone.
                self.log_refused(&[0x36], before, &error, None);
                return Err(error);
            }
        };
        let request = transfer_data_bytes(counter, data);
        if let Err(error) = self.transfer.check_length(index, data.len()) {
            // A block refused for want of a block length was refused for the RequestDownload's
            // response.
            let caused_by = match error {
                HostError::NoBlockLength { .. } => self.download_seq,
                _ => None,
            };
            self.log_refused(&request, before, &error, caused_by);
            return Err(error);
        }
        let outcome = self.send_recv(request);
        self.transfer.after_block(index, counter, outcome)
    }

    fn wait(&mut self, inquiry: u64, millis: u32) -> Result<bool, HostError> {
        wait_elapsed(&mut self.waits, inquiry, millis, Instant::now())
    }

    fn hmi_request(&mut self, _inquiry: u64, _form: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
        Err(HostError::Unsupported("HmiRequest"))
    }

    fn record_input(
        &mut self,
        _inquiry: u64,
        _template: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError> {
        Err(HostError::Unsupported("RecordInput"))
    }

    fn monitor_capture(&mut self, _back_millis: u32) -> Result<(), HostError> {
        Err(HostError::Unsupported("MonitorCapture"))
    }

    // The job's artifact, held in memory since it was hashed (ADR-336 item 12). `check_job` has
    // refused a segment outside it, so a range outside it is defended against, not expected.
    fn artifact_bytes(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, HostError> {
        let artifact = self.artifact.as_ref().ok_or(HostError::NoArtifact)?;
        artifact
            .slice(offset, len)
            .map(<[u8]>::to_vec)
            .ok_or(HostError::ArtifactRange { offset, len })
    }

    // Reads through the host's runtime inputs, the source the restart's precondition checks use
    // (`inputs`): a value is the integer reading, and an input the agent cannot establish or
    // decode, or one that reads as text, is [`HostError::InputNotEstablished`], like a failure to
    // reach the worker a host error the job can be resumed after (ADR-338, ADR-288).
    fn runtime_input(&mut self, input: diag_ir::RuntimeInput) -> Result<i64, HostError> {
        use crate::inputs::RuntimeInputs;
        integer_reading(input, self.read(input)?)
    }

    // The message goes to the communication log only, since it may hold vehicle data; the
    // operational log records that one came, with its level (ADR-348 item 6). A job whose
    // procedure logs always has a log (`Job::with_comm_log_dir`).
    fn log(&mut self, level: u8, message: &str) {
        tracing::info!(level, "the procedure logged a message");
        if let Some(log) = &self.comm_log {
            log.procedure_log(level, message);
        }
    }

    // An identity value the runner read goes to the communication log as it is read (design
    // 8.11 "Header"); it is vehicle data, so never to the operational log.
    fn note_identity(&mut self, kind: diag_ir::IdentityKind, read: diag_ir::IdentityRead<'_>) {
        if let Some(log) = &self.comm_log {
            log.identity(kind, read);
        }
    }
}

/// The integer a runtime-input reading gives `ReadInput`: anything else (no source, an
/// undecodable answer, text) is [`HostError::InputNotEstablished`] (ADR-338).
fn integer_reading(
    input: diag_ir::RuntimeInput,
    reading: crate::inputs::Reading,
) -> Result<i64, HostError> {
    reading.value().ok_or(HostError::InputNotEstablished(input))
}

/// Whether `millis` have passed since the first poll of `inquiry` at or before `now`. A finished
/// inquiry is forgotten, so the same instruction reached again starts a new wait.
fn wait_elapsed(
    waits: &mut HashMap<u64, Instant>,
    inquiry: u64,
    millis: u32,
    now: Instant,
) -> Result<bool, HostError> {
    let started = *waits.entry(inquiry).or_insert(now);
    let done = now.saturating_duration_since(started) >= Duration::from_millis(millis.into());
    if done {
        waits.remove(&inquiry);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- exchange entries (design 8.11 "Exchange entry") ----

    fn end_of(request: &[u8], result: Result<Vec<u8>, HostError>) -> (End, Detail) {
        sent_end(request, &result, &NegativeShape::uds(), request[0])
    }

    fn rpc(code: tonic::Code) -> HostError {
        rpc_of("StartComPrimitive", code)
    }

    fn rpc_of(rpc: &'static str, code: tonic::Code) -> HostError {
        HostError::Rpc {
            rpc,
            status: Box::new(tonic::Status::new(code, "x")),
        }
    }

    /// A start the worker refused with a reason was not sent, refused by the worker for its own
    /// reasons (ADR-350 item 2, design 8.11 `not_sent`), the reason in `detail`. A refusal
    /// without a reason, and a reason on a call after the start, keep the send unknown.
    #[test]
    fn a_refusal_that_names_its_reason_is_not_sent() {
        use vci_service_interface::{ErrorDetail, PduError, RefusalReason};
        let refused_on = |rpc: &'static str, reason: Option<RefusalReason>| {
            Err(HostError::Rpc {
                rpc,
                status: Box::new(vci_service_interface::status_with_error_detail(
                    tonic::Code::InvalidArgument,
                    "refused",
                    ErrorDetail {
                        pdu_error: PduError::PduErrInvalidParameters as i32,
                        error_event_data: None,
                        detail_text: None,
                        refusal: reason.map(|reason| reason as i32),
                    },
                )),
            })
        };
        let refused = |reason: Option<RefusalReason>| refused_on("StartComPrimitive", reason);
        let read = [0x22, 0xF1, 0x90];
        for reason in [
            RefusalReason::InvalidParameters,
            RefusalReason::QueueFull,
            RefusalReason::ProtocolCannotCarry,
        ] {
            let (end, detail) = end_of(&read, refused(Some(reason)));
            assert!(
                matches!(end, End::NotSent(RefusalCause::SystemOwnReasons)),
                "{reason:?}: {end:?}"
            );
            assert_eq!(detail.worker_detail.as_deref(), Some(reason.as_str_name()));
            assert_eq!(detail.grpc_status, Some("InvalidArgument"));
        }
        for result in [refused(None), refused(Some(RefusalReason::Unspecified))] {
            let end = end_of(&read, result).0;
            assert!(
                matches!(end, End::Unknown(UnknownCause::PrimitiveFailedToStart)),
                "{end:?}"
            );
        }
        let end = end_of(
            &read,
            refused_on("SubscribeEvent", Some(RefusalReason::QueueFull)),
        )
        .0;
        assert!(
            matches!(end, End::Unknown(UnknownCause::RpcFailure)),
            "{end:?}"
        );
    }

    #[test]
    fn a_sent_request_ends_as_the_host_saw_it() {
        let read = [0x22, 0xF1, 0x90];
        assert!(matches!(
            end_of(&read, Ok(vec![0x62, 0xF1, 0x90])).0,
            End::Positive(_)
        ));
        assert!(matches!(
            end_of(&read, Ok(vec![0x7F, 0x22, 0x31])).0,
            End::Negative(_)
        ));
        let (end, detail) = end_of(&read, Err(HostError::NoResponse));
        assert!(matches!(end, End::NoResponse));
        assert_eq!(detail.host_error, Some("NoResponse"));
        for refused_after_loss in [true, false] {
            let (end, detail) = end_of(&read, Err(HostError::VciLost { refused_after_loss }));
            assert!(
                matches!(end, End::VciLost { refused_after_loss: r } if r == refused_after_loss),
                "{end:?}"
            );
            assert_eq!(
                detail.error_event.as_deref(),
                Some("PDU_ERR_EVT_LOST_COMM_TO_VCI")
            );
        }
        let (end, detail) = end_of(&read, Err(rpc(tonic::Code::Unavailable)));
        assert!(
            matches!(end, End::Unknown(UnknownCause::PrimitiveFailedToStart)),
            "{end:?}"
        );
        assert_eq!(detail.grpc_status, Some("Unavailable"));
        let end = end_of(&read, Err(rpc_of("SubscribeEvent", tonic::Code::Internal))).0;
        assert!(
            matches!(end, End::Unknown(UnknownCause::RpcFailure)),
            "{end:?}"
        );
        let (end, detail) = end_of(
            &read,
            Err(HostError::PrimitiveFailed(
                PduErrorEvent::PduErrEvtTxError as i32,
            )),
        );
        assert!(
            matches!(end, End::LinkError(LinkErrorKind::Transmit)),
            "{end:?}"
        );
        assert_eq!(detail.error_event.as_deref(), Some("PDU_ERR_EVT_TX_ERROR"));
        assert!(matches!(
            end_of(&read, Err(HostError::EventsLost)).0,
            End::Unknown(UnknownCause::EventsLost)
        ));
        assert!(matches!(
            end_of(&read, Err(HostError::EventStreamEnded)).0,
            End::Unknown(UnknownCause::EventsEnded)
        ));
        assert!(matches!(
            end_of(
                &read,
                Err(HostError::WorkerUnresponsive("StartComPrimitive"))
            )
            .0,
            End::Unknown(UnknownCause::WorkerNotAnswering)
        ));
        assert!(matches!(
            end_of(&read, Err(HostError::Setup("no handle"))).0,
            End::Unknown(UnknownCause::PrimitiveFailedToStart)
        ));
    }

    #[test]
    fn a_positive_response_the_agent_cannot_use_is_unusable() {
        let block = [0x36, 0x01, 0xAA];
        assert!(matches!(
            end_of(&block, Ok(vec![0x76, 0x01])).0,
            End::Positive(_)
        ));
        for wrong in [vec![0x76, 0x02], vec![0x76]] {
            assert!(
                matches!(
                    end_of(&block, Ok(wrong.clone())).0,
                    End::PositiveUnusable(_)
                ),
                "{wrong:02X?}"
            );
        }
        let download = [0x34, 0x00, 0x44, 0, 0, 0, 0, 0, 0, 1, 0];
        assert!(matches!(
            end_of(&download, Ok(vec![0x74, 0x20, 0x01, 0x02])).0,
            End::Positive(_)
        ));
        assert!(matches!(
            end_of(&download, Ok(vec![0x74, 0x20, 0x01])).0,
            End::PositiveUnusable(_)
        ));
    }

    #[test]
    fn a_refusal_names_its_cause() {
        let cases: [(&[u8], HostError, RefusalCause); 9] = [
            (
                &[],
                HostError::BadService(0x1FF),
                RefusalCause::UndeclaredService,
            ),
            (
                &[0x22],
                HostError::UndeclaredService(7),
                RefusalCause::UndeclaredService,
            ),
            (&[], HostError::NotAllowed(0), RefusalCause::EmptyRequest),
            (&[0x2E], HostError::NotAllowed(0x2E), RefusalCause::Policy),
            (
                &[0x36, 0x01],
                HostError::UseFlashTransfer,
                RefusalCause::Policy,
            ),
            (
                &[0x36],
                HostError::NoTransferActive,
                RefusalCause::NoTransferActive,
            ),
            (
                &[0x36, 0x01],
                HostError::BlockTooLong {
                    block: 1,
                    length: 9,
                    max: 8,
                },
                RefusalCause::BlockTooLong,
            ),
            (
                &[0x36, 0x01],
                HostError::NoBlockLength {
                    block: 1,
                    response: vec![0x74],
                },
                RefusalCause::EarlierResponse,
            ),
            (
                &[0x22],
                HostError::BadNegativeShape("x"),
                RefusalCause::PreparationError,
            ),
        ];
        for (request, error, cause) in cases {
            assert_eq!(refusal_cause(request, &error), cause, "{error:?}");
        }
    }

    /// An error event without extra error information.
    fn seen(event: i32) -> ErrorSeen {
        ErrorSeen {
            event,
            extra: None,
            extra_name: None,
            at: None,
        }
    }

    #[test]
    fn an_error_event_counts_however_the_exchange_ended() {
        let tx = [seen(PduErrorEvent::PduErrEvtTxError as i32)];
        for result in [
            Ok(vec![0x62, 0xF1, 0x90]),
            Err(HostError::NoResponse),
            Err(HostError::EventsLost),
            Err(HostError::EventStreamEnded),
        ] {
            assert_eq!(also_seen(&result, &tx), tx, "{result:?}");
        }
        let failed = Err(HostError::PrimitiveFailed(tx[0].event));
        assert!(also_seen(&failed, &tx).is_empty());
        let fault = seen(PduErrorEvent::PduErrEvtVciHardwareFault as i32);
        assert_eq!(also_seen(&failed, &[fault.clone(), tx[0].clone()]), [fault]);
        assert!(also_seen(&Err(HostError::NoResponse), &[]).is_empty());
    }

    #[test]
    fn detail_names_the_event_that_ended_the_exchange_first() {
        let read = [0x22, 0xF1, 0x90];
        let tx = PduErrorEvent::PduErrEvtTxError as i32;
        let fault = PduErrorEvent::PduErrEvtVciHardwareFault as i32;
        let frame = PduErrorEvent::PduErrEvtFrameStruct as i32;
        let sent = |result, error_events: &[i32], rx_timeout: bool| Sent {
            result,
            error_events: error_events.iter().copied().map(seen).collect(),
            errors_before_response: None,
            rx_timeout: rx_timeout.then(|| seen(PduErrorEvent::PduErrEvtRxTimeout as i32)),
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            agent_deadline: false,
            response_times: VciTimes::default(),
            deadline_reading: None,
        };
        let detail_of = |sent: &Sent| {
            let (_, mut detail) = sent_end(&read, &sent.result, &NegativeShape::uds(), read[0]);
            let (link_errors, _) = events_seen(sent, &mut detail);
            assert_eq!(detail.extra_error_info, None);
            assert_eq!(detail.extra_error_info_name, None);
            (detail.error_event, link_errors)
        };
        let name = |name: &str| Some(name.to_owned());
        // A lost VCI after another error event: the loss is named, the other is a link error.
        assert_eq!(
            detail_of(&sent(
                Err(HostError::VciLost {
                    refused_after_loss: false
                }),
                &[tx],
                false
            )),
            (
                name("PDU_ERR_EVT_LOST_COMM_TO_VCI"),
                vec![LinkErrorKind::Transmit]
            )
        );
        // A receive timeout is named, and is no link error.
        assert_eq!(
            detail_of(&sent(Err(HostError::NoResponse), &[], true)),
            (name("PDU_ERR_EVT_RX_TIMEOUT"), vec![])
        );
        // An error event before the timeout is the one named.
        assert_eq!(
            detail_of(&sent(Err(HostError::NoResponse), &[tx], true)),
            (name("PDU_ERR_EVT_TX_ERROR"), vec![LinkErrorKind::Transmit])
        );
        // Every fault seen is kept, so the classification can take the most severe.
        assert_eq!(
            detail_of(&sent(Ok(vec![0x62, 0xF1, 0x90]), &[fault, frame], false)),
            (
                name("PDU_ERR_EVT_FRAME_STRUCT"),
                vec![LinkErrorKind::VciHardwareFault, LinkErrorKind::Frame]
            )
        );
        // A primitive that failed ends on its last event; the earlier one is a link error
        // beside it.
        assert_eq!(
            detail_of(&sent(
                Err(HostError::PrimitiveFailed(frame)),
                &[fault, frame],
                false
            )),
            (
                name("PDU_ERR_EVT_FRAME_STRUCT"),
                vec![LinkErrorKind::VciHardwareFault]
            )
        );
        // A code the interface does not list (a vendor's) is named by its number.
        let vendor = 0x7FFF_0001;
        assert_eq!(
            detail_of(&sent(
                Err(HostError::PrimitiveFailed(vendor)),
                &[vendor],
                false
            )),
            (name("error event 0x7fff0001"), vec![])
        );
        assert_eq!(link_error_kind(vendor), LinkErrorKind::Other);
    }

    #[test]
    fn detail_gives_the_extra_error_information_of_the_event_it_names() {
        let read = [0x22, 0xF1, 0x90];
        let event = |event: PduErrorEvent, extra, name: Option<&str>| ErrorSeen {
            event: event as i32,
            extra,
            extra_name: name.map(str::to_owned),
            at: None,
        };
        let tx = |extra, name| event(PduErrorEvent::PduErrEvtTxError, extra, name);
        let timeout = |extra, name| event(PduErrorEvent::PduErrEvtRxTimeout, extra, name);
        let lost = |extra| event(PduErrorEvent::PduErrEvtLostCommToVci, extra, None);
        let base = |result| Sent {
            result,
            error_events: Vec::new(),
            errors_before_response: None,
            rx_timeout: None,
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            agent_deadline: false,
            response_times: VciTimes::default(),
            deadline_reading: None,
        };
        let given = |sent: Sent| {
            let (_, mut detail) = sent_end(&read, &sent.result, &NegativeShape::uds(), read[0]);
            events_seen(&sent, &mut detail);
            (
                detail.error_event,
                detail.extra_error_info,
                detail.extra_error_info_name,
            )
        };
        let name = |name: &str| Some(name.to_owned());
        // The code and the name come as the worker gave them: both, the code alone (a D-PDU
        // code its MDF does not name), or the name alone (a J2534 worker's case).
        for (extra, extra_name) in [
            (Some(0x101), Some("XTRA_RX_TIMEOUT")),
            (Some(0x101), None),
            (None, Some("ISO15765_CF_TIMEOUT")),
        ] {
            let timed_out = Sent {
                rx_timeout: Some(timeout(extra, extra_name)),
                ..base(Err(HostError::NoResponse))
            };
            assert_eq!(
                given(timed_out),
                (
                    name("PDU_ERR_EVT_RX_TIMEOUT"),
                    extra,
                    extra_name.map(str::to_owned)
                )
            );
        }
        // The information is that of the event named: here the transmit error before the
        // timeout, which has none.
        let both = Sent {
            error_events: vec![tx(None, None)],
            rx_timeout: Some(timeout(Some(0x101), Some("XTRA_RX_TIMEOUT"))),
            ..base(Err(HostError::NoResponse))
        };
        assert_eq!(given(both), (name("PDU_ERR_EVT_TX_ERROR"), None, None));
        // A failed primitive names the event it ended on, not an earlier one.
        let failed = Sent {
            error_events: vec![tx(Some(0x101), Some("EARLIER")), tx(Some(0x7F), None)],
            ..base(Err(HostError::PrimitiveFailed(
                PduErrorEvent::PduErrEvtTxError as i32,
            )))
        };
        assert_eq!(
            given(failed),
            (name("PDU_ERR_EVT_TX_ERROR"), Some(0x7F), None)
        );
        // A lost VCI gives its own, also after another event.
        let gone = Sent {
            error_events: vec![tx(Some(0x101), None)],
            vci_lost: Some(lost(Some(0xFFFF_FFFF))),
            ..base(Err(HostError::VciLost {
                refused_after_loss: false,
            }))
        };
        assert_eq!(
            given(gone),
            (
                name("PDU_ERR_EVT_LOST_COMM_TO_VCI"),
                Some(0xFFFF_FFFF),
                None
            )
        );
        // A lost VCI the start refused carries no event of the stream, so no information.
        assert_eq!(
            given(base(Err(HostError::VciLost {
                refused_after_loss: false
            }))),
            (name("PDU_ERR_EVT_LOST_COMM_TO_VCI"), None, None)
        );
    }

    #[test]
    fn the_suppress_bit_counts_on_a_service_with_a_sub_function() {
        for suppressed in [
            [0x3E, 0x80],
            [0x10, 0x81],
            [0x87, 0x83],
            [0x29, 0x80],
            [0x31, 0x81],
            [0x19, 0x82],
        ] {
            assert!(
                suppresses_positive_response(&suppressed),
                "{suppressed:02X?}"
            );
        }
        for not in [&[0x3E, 0x00][..], &[0x22, 0x80, 0x01], &[0x3E], &[]] {
            assert!(!suppresses_positive_response(not), "{not:02X?}");
        }
    }

    #[test]
    fn a_step_s_position_names_its_source_and_sections() {
        let span = |pc, line| diag_ir::SourceSpan {
            pc,
            file: 0,
            line,
            column: 5,
        };
        let section = |start_pc, end_pc| diag_ir::Section {
            start_pc,
            end_pc,
            interruptible: diag_ir::Interruptible::No,
            idempotency: diag_ir::Idempotency::Safe,
            expected_millis: 180_000,
        };
        let program = diag_ir::Program {
            schema_version: diag_ir::IR_SCHEMA_VERSION,
            code: Vec::new(),
            constants: Vec::new(),
            sections: vec![section(2, 4), section(6, 8)],
            source_map: vec![span(0, 10), span(3, 11)],
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Vec::new(),
        };
        let at = at_of(&program, 2, 9);
        assert_eq!((at.pc, at.steps), (2, 9));
        assert_eq!(at.source.map(|source| source.line), Some(10));
        assert_eq!(at.sections.len(), 1);
        assert_eq!(at.sections[0].expected_millis, 180_000);
        assert_eq!(
            at_of(&program, 3, 0).source.map(|source| source.line),
            Some(11)
        );
        assert!(at_of(&program, 4, 0).sections.is_empty());
        let unmapped = diag_ir::Program {
            source_map: Vec::new(),
            ..program
        };
        assert!(at_of(&unmapped, 2, 0).source.is_none());
    }

    /// Only an integer reading is a value for `ReadInput`; a reading the agent cannot establish
    /// or decode, or one that is text, is the repeatable host error (ADR-338).
    #[test]
    fn only_an_integer_reading_reads_as_an_input() {
        use crate::inputs::Reading;
        let input = diag_ir::RuntimeInput::VehicleSpeedKmh;
        assert!(matches!(integer_reading(input, Reading::Value(87)), Ok(87)));
        for reading in [
            Reading::CannotBeEstablished,
            Reading::CannotBeDecoded,
            Reading::Text("87".to_owned()),
        ] {
            assert!(
                matches!(
                    integer_reading(input, reading.clone()),
                    Err(HostError::InputNotEstablished(i)) if i == input
                ),
                "{reading:?}"
            );
        }
    }

    #[test]
    fn requests_are_encoded_with_the_sid_first() {
        assert_eq!(
            service_request_bytes(0x22, &[0xF1, 0x90]).unwrap(),
            [0x22, 0xF1, 0x90]
        );
        assert_eq!(service_request_bytes(0x3E, &[]).unwrap(), [0x3E]);
        assert!(matches!(
            service_request_bytes(0x100, &[]),
            Err(HostError::BadService(0x100))
        ));
        assert_eq!(
            routine_control_bytes(0xFF00, 0x01, &[0xAA]),
            [0x31, 0x01, 0xFF, 0x00, 0xAA]
        );
    }

    #[test]
    fn waits_end_after_their_time_and_restart_for_a_new_inquiry() {
        let mut waits = HashMap::new();
        let t0 = Instant::now();
        assert!(!wait_elapsed(&mut waits, 7, 100, t0).unwrap());
        assert!(!wait_elapsed(&mut waits, 7, 100, t0 + Duration::from_millis(99)).unwrap());
        assert!(wait_elapsed(&mut waits, 7, 100, t0 + Duration::from_millis(100)).unwrap());
        assert!(waits.is_empty());
        // A zero wait ends at once.
        assert!(wait_elapsed(&mut waits, 8, 0, t0).unwrap());
    }

    #[test]
    fn send_recv_ceiling_covers_p2_and_the_0x78_chain() {
        let config = LinkConfig::iso15765(0x7E0, 0x7E8);
        assert_eq!(
            Timings::for_link(&config).send_recv,
            Duration::from_millis(1_000 + 25_000 + 5_000 + 2_000)
        );
    }

    #[test]
    fn link_configs_with_unusable_values_are_refused() {
        let good = LinkConfig::iso15765(0x7E0, 0x7E8);
        good.validate().unwrap();
        LinkConfig {
            p2_max_ms: crate::link::MAX_TIMING_MS,
            tx_id: 0x7FF,
            ..good.clone()
        }
        .validate()
        .unwrap();
        for bad in [
            LinkConfig {
                p2_max_ms: 0,
                ..good.clone()
            },
            LinkConfig {
                p2_star_ms: 0,
                ..good.clone()
            },
            LinkConfig {
                rc78_completion_ms: 0,
                ..good.clone()
            },
            LinkConfig {
                rc78_completion_ms: crate::link::MAX_TIMING_MS + 1,
                ..good.clone()
            },
            // Raw CAN carries no UDS, so the read-only policy would mean nothing there.
            LinkConfig {
                protocol_short_name: "CAN".to_owned(),
                ..good.clone()
            },
            // 29-bit IDs need the ID format set on the link, which it does not do yet.
            LinkConfig {
                rx_id: 0x800,
                ..good.clone()
            },
            LinkConfig {
                tx_id: 0x18DA_10F1,
                ..good.clone()
            },
        ] {
            assert!(
                matches!(bad.validate(), Err(HostError::Setup(_))),
                "{bad:?}"
            );
        }
    }

    /// A counter with a transfer begun by a positive RequestDownload.
    fn begun() -> TransferCounter {
        let mut counter = TransferCounter::default();
        counter.after_request(&[0x34, 0x00, 0x44], Ok(&[0x74, 0x20, 0x01, 0x02]));
        counter
    }

    fn confirm(counter: &mut TransferCounter, index: u64) {
        let (next, wire) = counter.next().unwrap();
        assert_eq!(next, index);
        counter
            .after_block(next, wire, Ok(vec![0x76, wire]))
            .unwrap();
    }

    #[test]
    fn the_wire_counter_starts_at_one_and_wraps_after_ff_to_zero() {
        let counter = TransferCounter::default();
        assert!(matches!(counter.next(), Err(HostError::NoTransferActive)));
        let mut counter = begun();
        assert_eq!(counter.accepted, Some(0));
        assert_eq!(counter.next().unwrap(), (1, 0x01));
        for index in 1..=255 {
            confirm(&mut counter, index);
        }
        assert_eq!(counter.next().unwrap(), (256, 0x00));
        confirm(&mut counter, 256);
        assert_eq!(counter.next().unwrap(), (257, 0x01));
        assert_eq!(wire_counter(u64::from(u32::MAX) + 1), 0x00);
        assert_eq!(wire_counter(254), 0xFE);
        assert_eq!(wire_counter(255), 0xFF);
    }

    #[test]
    fn a_download_request_begins_a_transfer_only_when_answered_positively() {
        let download = [0x34, 0x00, 0x44];
        let mut counter = TransferCounter::default();
        counter.after_request(&download, Ok(&[0x7F, 0x34, 0x22]));
        assert_eq!(counter.accepted, None);
        counter.after_request(&download, Ok(&[0x74, 0x20, 0x01, 0x02]));
        assert_eq!(counter.accepted, Some(0));
        confirm(&mut counter, 1);
        confirm(&mut counter, 2);
        // A second download mid-transfer starts over.
        counter.after_request(&download, Ok(&[0x74, 0x20]));
        assert_eq!(counter.accepted, Some(0));
        assert_eq!(counter.next().unwrap(), (1, 0x01));
        // A failed one leaves no transfer, as does no response.
        confirm(&mut counter, 1);
        counter.after_request(&download, Ok(&[0x7F, 0x34, 0x22]));
        assert_eq!(counter.accepted, None);
        counter.after_request(&download, Ok(&[0x74, 0x20]));
        counter.after_request(&download, Err(()));
        assert_eq!(counter.accepted, None);
        // A response of another service does not begin one.
        counter.after_request(&download, Ok(&[0x77]));
        assert_eq!(counter.accepted, None);
    }

    #[test]
    fn these_requests_end_the_tracked_transfer_whatever_the_response() {
        // RequestUpload never begins one either.
        let mut counter = TransferCounter::default();
        counter.after_request(&[0x35, 0x00, 0x44], Ok(&[0x75, 0x20, 0x01]));
        assert_eq!(counter.accepted, None);
        for request in [
            &[0x35, 0x00, 0x44][..],
            &[0x37],
            &[0x38, 0x01],
            &[0x10, 0x01],
            &[0x11, 0x01],
        ] {
            for outcome in [Ok(&[0x7F, request[0], 0x22][..]), Ok(&[0x00][..]), Err(())] {
                let mut counter = begun();
                counter.after_request(request, outcome);
                assert_eq!(counter.accepted, None, "{request:02X?}");
                assert!(matches!(counter.next(), Err(HostError::NoTransferActive)));
            }
        }
        // Other services leave it.
        let mut counter = begun();
        counter.after_request(&[0x22, 0xF1, 0x90], Ok(&[0x62, 0xF1, 0x90]));
        counter.after_request(&[0x31, 0x01, 0xFF, 0x00], Err(()));
        assert_eq!(counter.accepted, Some(0));
    }

    #[test]
    fn any_failed_block_ends_the_transfer() {
        for failure in [
            Err(HostError::NoResponse),
            Err(HostError::EventsLost),
            Err(HostError::WorkerUnresponsive("StartComPrimitive")),
            Ok(vec![0x7F, 0x36, 0x73]),
            Ok(vec![0x76, 0x03]),
        ] {
            let mut counter = begun();
            confirm(&mut counter, 1);
            assert!(counter.after_block(2, 0x02, failure).is_err());
            assert_eq!(counter.accepted, None);
            assert!(matches!(counter.next(), Err(HostError::NoTransferActive)));
        }
    }

    #[test]
    fn the_download_response_sets_the_block_length_limit() {
        assert_eq!(max_block_length(&[0x74, 0x20, 0x01, 0x02]), Some(0x0102));
        assert_eq!(max_block_length(&[0x74, 0x10, 0x82]), Some(0x82));
        assert_eq!(
            max_block_length(&[0x74, 0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]),
            Some(u64::MAX - 1)
        );
        // Wider than 64 bits: no block reaches it, also with leading zeros kept exact.
        assert_eq!(
            max_block_length(&[0x74, 0x90, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(u64::MAX)
        );
        assert_eq!(
            max_block_length(&[0x74, 0x90, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x02]),
            Some(0x0102)
        );
        // No readable limit: no length byte, a length of 0, a reserved nibble that is not 0,
        // too few bytes, or more than the length names.
        for response in [
            &[0x74][..],
            &[0x74, 0x00],
            &[0x74, 0x00, 0x01],
            &[0x74, 0x2F, 0x01, 0x02],
            &[0x74, 0x20, 0x01],
            &[0x74, 0x40],
            &[0x74, 0x20, 0x01, 0x02, 0xAA],
        ] {
            assert_eq!(max_block_length(response), None, "{response:02X?}");
        }
    }

    #[test]
    fn a_block_longer_than_the_ecus_limit_is_refused_and_the_transfer_kept() {
        // A limit of 0x0102 bytes leaves 0x0100 data bytes after the SID and the counter.
        let mut counter = begun();
        assert_eq!(counter.max_block_length, Some(Ok(0x0102)));
        counter.check_length(1, 0x0100).unwrap();
        assert!(matches!(
            counter.check_length(1, 0x0101),
            Err(HostError::BlockTooLong {
                block: 1,
                length: 0x0103,
                max: 0x0102
            })
        ));
        assert_eq!(counter.accepted, Some(0));
        confirm(&mut counter, 1);
        // A limit below the SID and counter refuses every block, even an empty one.
        counter.after_request(&[0x34, 0x00, 0x44], Ok(&[0x74, 0x10, 0x01]));
        assert!(matches!(
            counter.check_length(1, 0),
            Err(HostError::BlockTooLong {
                length: 2,
                max: 1,
                ..
            })
        ));
        // A response with no readable limit begins the transfer, but no block is sent.
        counter.after_request(&[0x34, 0x00, 0x44], Ok(&[0x74, 0x00]));
        assert_eq!(counter.accepted, Some(0));
        assert!(matches!(
            counter.check_length(1, 1),
            Err(HostError::NoBlockLength { block: 1, response }) if response == [0x74, 0x00]
        ));
        // A new download replaces the limit; a refused one clears it with the transfer.
        counter.after_request(&[0x34, 0x00, 0x44], Ok(&[0x74, 0x10, 0x10]));
        assert_eq!(counter.max_block_length, Some(Ok(0x10)));
        counter.after_request(&[0x34, 0x00, 0x44], Ok(&[0x7F, 0x34, 0x22]));
        assert_eq!(counter.max_block_length, None);
    }

    #[test]
    fn a_service_request_cannot_carry_transfer_data() {
        assert!(matches!(
            refuse_transfer_data(&[0x36, 0x01, 0xAA]),
            Err(HostError::UseFlashTransfer)
        ));
        refuse_transfer_data(&[0x34, 0x00]).unwrap();
        refuse_transfer_data(&[0x37]).unwrap();
    }

    #[test]
    fn transfer_data_bytes_carry_the_counter_first() {
        assert_eq!(transfer_data_bytes(0x00, &[1, 2]), [0x36, 0x00, 1, 2]);
    }

    #[test]
    fn the_transfer_response_must_echo_the_counter() {
        check_transfer_response(1, 0x01, vec![0x76, 0x01]).unwrap();
        check_transfer_response(1, 0x01, vec![0x76, 0x01, 0xAA]).unwrap();
        assert!(matches!(
            check_transfer_response(2, 0x02, vec![0x76, 0x01]),
            Err(HostError::TransferEchoMismatch {
                block: 2,
                counter: 0x02,
                ..
            })
        ));
        // A positive response with no counter at all echoes nothing.
        assert!(matches!(
            check_transfer_response(1, 0x01, vec![0x76]),
            Err(HostError::TransferEchoMismatch { block: 1, .. })
        ));
        // A negative response, wrong block sequence counter (0x73).
        let refused = check_transfer_response(3, 0x03, vec![0x7F, 0x36, 0x73]);
        assert!(
            matches!(&refused, Err(HostError::TransferRefused { block: 3, response }) if response == &[0x7F, 0x36, 0x73]),
            "{refused:?}"
        );
        assert!(matches!(
            check_transfer_response(1, 0x01, vec![]),
            Err(HostError::TransferRefused { .. })
        ));
    }

    const COP: ComPrimitiveHandle = ComPrimitiveHandle {
        module_handle: 1,
        cll_handle: 1,
        cop_handle: 7,
    };

    fn event(cop: Option<ComPrimitiveHandle>, data: event_item::Data) -> EventItem {
        EventItem {
            cop_handle: cop,
            data: Some(data),
            ..EventItem::default()
        }
    }

    fn result(bytes: &[u8]) -> event_item::Data {
        event_item::Data::ResultData(vci_service_interface::ResultData {
            data_bytes: bytes.to_vec(),
            ..Default::default()
        })
    }

    fn status(status: PduComPrimitiveStatus) -> event_item::Data {
        event_item::Data::CopStatus(status as i32)
    }

    /// Feeds `events` and returns the outcome of the first that ends the primitive.
    fn outcome(events: Vec<EventItem>) -> Option<Result<Vec<u8>, HostError>> {
        outcome_for(&[0x22, 0xF1, 0x90], events)
    }

    fn outcome_for(request: &[u8], events: Vec<EventItem>) -> Option<Result<Vec<u8>, HostError>> {
        let mut progress = Progress::for_request(request).unwrap();
        events
            .into_iter()
            .find_map(|item| progress.on_event(item, COP))
    }

    #[test]
    fn the_last_response_of_the_primitive_is_its_result() {
        let other = ComPrimitiveHandle {
            cop_handle: 6,
            ..COP
        };
        let got = outcome(vec![
            // An earlier primitive's late events, and events of no primitive, do not end it.
            event(Some(other), result(&[0x50, 0x01])),
            event(Some(other), status(PduComPrimitiveStatus::PduCopstFinished)),
            event(None, result(&[0x7F, 0x10, 0x11])),
            event(None, status(PduComPrimitiveStatus::PduCopstFinished)),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstExecuting)),
            event(Some(COP), result(&[0x7F, 0x22, 0x78])),
            event(Some(COP), result(&[0x62, 0xF1, 0x90])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]);
        assert_eq!(got.unwrap().unwrap(), [0x62, 0xF1, 0x90]);
    }

    /// Design 8.11 `vci_did_not_end`: the agent's own deadline ending the wait is no response
    /// within the VCI's limit; a receive timeout the worker reported is.
    #[test]
    fn the_agent_s_deadline_is_vci_did_not_end() {
        assert!(matches!(
            deadline_end(End::NoResponse, true),
            End::VciDidNotEnd
        ));
        assert!(matches!(
            deadline_end(End::NoResponse, false),
            End::NoResponse
        ));
        assert!(matches!(
            deadline_end(End::Positive(vec![0x62]), true),
            End::Positive(_)
        ));
    }

    /// ADR-355: only a link the worker said forwards every pending is `forwarded`.
    #[test]
    fn only_a_link_said_to_forward_every_pending_is_forwarded() {
        assert_eq!(pending_visibility(Some(true)), PendingVisibility::Forwarded);
        assert_eq!(
            pending_visibility(Some(false)),
            PendingVisibility::AbsorbedByVci
        );
        assert_eq!(pending_visibility(None), PendingVisibility::AbsorbedByVci);
    }

    /// Design 8.11 `pending`: each response pending bound to the primitive is kept for the log,
    /// with its time and clock as the worker gave them; another primitive's is not. The log's
    /// form takes the agent's time when the host read it and leaves out one read with no log.
    #[test]
    fn the_pendings_bound_to_the_primitive_are_kept_for_the_log() {
        let other = ComPrimitiveHandle {
            cop_handle: 6,
            ..COP
        };
        let pending = |cop, timestamp| EventItem {
            timestamp,
            timestamp_clock: TimestampClock::Device as i32,
            ..event(cop, result(&[0x7F, 0x22, 0x78]))
        };
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let got = [
            pending(Some(other), 5),
            pending(Some(COP), 10),
            pending(Some(COP), 20),
            event(Some(COP), result(&[0x62, 0xF1, 0x90])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]
        .into_iter()
        .find_map(|item| progress.on_event(item, COP));
        assert_eq!(got.unwrap().unwrap(), [0x62, 0xF1, 0x90]);
        let times: Vec<_> = progress
            .pendings
            .iter()
            .map(|pending| (pending.vci_us, pending.clock))
            .collect();
        assert_eq!(
            times,
            [(10, Some(Clock::Device)), (20, Some(Clock::Device))]
        );

        progress.pendings[0].read = Some(AgentNow {
            epoch: 0,
            monotonic_us: 100,
            wall_unix_ms: 0,
        });
        let logged = pendings_of(&progress.pendings);
        assert_eq!(logged.len(), 1, "the one read with no log is left out");
        assert_eq!(logged[0].vci_us, Some(10));
        assert_eq!(logged[0].clock, Some(Clock::Device));
        assert_eq!(logged[0].agent_us, 100);
    }

    /// A request the host did not start because an earlier primitive still holds the link is
    /// `not_sent`, refused by the agent (design 8.11 `sent`).
    #[test]
    fn a_request_behind_a_held_link_is_not_sent() {
        let read = [0x22, 0xF1, 0x90];
        let (end, detail) = sent_end(
            &read,
            &Err(HostError::LinkHeld),
            &NegativeShape::uds(),
            read[0],
        );
        assert!(
            matches!(end, End::NotSent(RefusalCause::LinkHeld)),
            "{end:?}"
        );
        assert_eq!(detail.host_error, Some("LinkHeld"));
    }

    /// ADR-359: a primitive that ended with no answer after a rejected result of positive form
    /// ends as that response, `positive_unusable`; a negative one, or an end the agent's
    /// deadline or the VCI's receive timeout gave, keeps its end.
    #[test]
    fn a_primitive_ended_on_a_rejected_positive_result_is_positive_unusable() {
        let rejected = |reason| RejectedRead {
            identifier: None,
            bytes: vec![0x62, 0xF1],
            vci_us: 0,
            clock: None,
            reason,
            read: None,
        };
        let sent = |reason, agent_deadline| Sent {
            rejected: vec![rejected(reason)],
            last_rejected: true,
            agent_deadline,
            ..Sent::not_started(Err(HostError::NoResponse))
        };
        assert!(matches!(
            unanswered_end(End::NoResponse, &sent(RejectedReason::WrongEcho, false)),
            End::PositiveUnusable(bytes) if bytes == [0x62, 0xF1]
        ));
        assert!(matches!(
            unanswered_end(
                End::NoResponse,
                &sent(RejectedReason::PatternMismatch, false)
            ),
            End::NoResponse
        ));
        assert!(matches!(
            unanswered_end(End::VciDidNotEnd, &sent(RejectedReason::WrongEcho, true)),
            End::VciDidNotEnd
        ));
        // A pending or an answer after the rejected result: it was not the primitive's end.
        let followed = Sent {
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            ..sent(RejectedReason::WrongEcho, false)
        };
        assert!(matches!(
            unanswered_end(End::NoResponse, &followed),
            End::NoResponse
        ));
        let none = Sent::not_started(Err(HostError::NoResponse));
        assert!(matches!(
            unanswered_end(End::NoResponse, &none),
            End::NoResponse
        ));
    }

    /// ADR-359: a result of the primitive that does not answer the request is rejected, with its
    /// reason; an indication, a result of no primitive and the answer are not.
    #[test]
    fn a_result_of_the_primitive_that_does_not_answer_is_rejected() {
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let tx_indication = event_item::Data::ResultData(vci_service_interface::ResultData {
            rx_flag: vec![0, 0, 0, 0x08],
            data_bytes: vec![0x22, 0xF1, 0x90],
            ..Default::default()
        });
        for item in [
            event(Some(COP), result(&[0x62, 0xF1])),
            event(Some(COP), result(&[0x7F, 0x31, 0x22])),
            event(Some(COP), tx_indication),
            event(None, result(&[0x62, 0xF1, 0x91])),
            event(Some(COP), result(&[0x62, 0xF1, 0x90, 0x41])),
        ] {
            assert!(progress.on_event(item, COP).is_none());
        }
        // The cut positive response and another service's refusal; not the indication, a result
        // of no primitive or the answer.
        let rejected: Vec<_> = progress
            .rejected
            .iter()
            .map(|rejected| (rejected.bytes.clone(), rejected.reason))
            .collect();
        assert_eq!(
            rejected,
            [
                (vec![0x62, 0xF1], RejectedReason::WrongEcho),
                (vec![0x7F, 0x31, 0x22], RejectedReason::PatternMismatch),
            ]
        );
        assert_eq!(progress.response, Some(vec![0x62, 0xF1, 0x90, 0x41]));
        assert!(!progress.last_rejected, "the answer came last");
        // The transmit indication bound to the primitive is no start of a response; a
        // start-of-message indication bound to it is (design 8.11 `vci_did_not_end`).
        assert!(!progress.bound_start);
        // It confirms the send, with its time (design 8.11 `sent_evidence`).
        assert!(progress.bound_tx.is_some());
        let mut started = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let start = event_item::Data::ResultData(vci_service_interface::ResultData {
            rx_flag: vec![0, 0, 0, 0x02],
            ..Default::default()
        });
        assert!(started.on_event(event(Some(COP), start), COP).is_none());
        assert!(started.bound_start);
        assert!(started.rejected.is_empty(), "an indication is not rejected");
        let mut pending_after = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        pending_after.on_event(event(Some(COP), result(&[0x62, 0xF1])), COP);
        assert!(pending_after.last_rejected);
        pending_after.on_event(event(Some(COP), result(&[0x7F, 0x22, 0x78])), COP);
        assert!(
            !pending_after.last_rejected,
            "a pending came after the rejected result"
        );
        let logged = rejected_of(
            &[RejectedRead {
                identifier: None,
                bytes: vec![0x62],
                vci_us: 5,
                clock: Some(Clock::Device),
                reason: RejectedReason::WrongEcho,
                read: Some(AgentNow {
                    epoch: 0,
                    monotonic_us: 9,
                    wall_unix_ms: 0,
                }),
            }],
            0x7E8,
        );
        assert_eq!(logged[0].rx_id, 0x7E8);
        assert_eq!(logged[0].agent_us, 9);
    }

    /// Design 8.11 "Settlement": a primitive cancelled before the link's next request takes its
    /// own items, lost events and a lost VCI of no primitive, not an indication of no primitive,
    /// which the exchange in flight may own. Its finished status ends it with what was read,
    /// and its cancelled status with the end read before it, else as the agent's cancel.
    #[test]
    fn a_primitive_cancelled_before_the_next_request_ends_on_its_own_items() {
        let carrying = |carried_out| {
            let progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
            let stray = Stray {
                cop: COP,
                deadline: tokio::time::Instant::now(),
                cancel_failed: false,
                request: vec![0x22, 0xF1, 0x90],
                expectation: crate::send_receive::Expectation {
                    sid: progress.sid,
                    positive: progress.positive_prefix.clone(),
                    negative: progress.negative.clone(),
                },
                seq: Some(1),
                response: None,
                response_times: VciTimes::default(),
                error: None,
                errors: Vec::new(),
            };
            Settling {
                progress: stray.progress(),
                stray,
                carried_out,
                end: None,
                items: Vec::new(),
            }
        };
        let settling = || carrying(true);
        let item = |cop, data| Some(event_notification::EventData::Item(event(cop, data)));
        let other = ComPrimitiveHandle {
            cop_handle: COP.cop_handle + 1,
            ..COP
        };
        let start = event_item::Data::ResultData(vci_service_interface::ResultData {
            rx_flag: vec![0, 0, 0, 0x02],
            ..Default::default()
        });
        let lost_vci = event_item::Data::ErrorData(ErrorData {
            error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
            ..ErrorData::default()
        });
        let taker = settling();
        assert!(taker.takes(&item(Some(COP), result(&[0x62, 0xF1, 0x90]))));
        assert!(!taker.takes(&item(Some(other), result(&[0x62, 0xF1, 0x90]))));
        assert!(!taker.takes(&item(None, start)));
        assert!(taker.takes(&item(None, lost_vci.clone())));
        assert!(shared_end(&item(None, lost_vci)));
        let lost = Some(event_notification::EventData::Lost(Default::default()));
        assert!(taker.takes(&lost) && shared_end(&lost));
        assert!(!shared_end(&item(Some(COP), result(&[0x62, 0xF1, 0x90]))));

        let finished = status(PduComPrimitiveStatus::PduCopstFinished);
        let cancelled = status(PduComPrimitiveStatus::PduCopstCancelled);
        // A response, then the finished or the cancelled status: the response ends it.
        for end in [finished.clone(), cancelled.clone()] {
            let mut responded = settling();
            responded.read(item(Some(COP), result(&[0x62, 0xF1, 0x90, 0x41])), None);
            assert!(responded.end.is_none(), "a response alone does not end it");
            responded.read(item(Some(COP), end), None);
            assert!(
                matches!(&responded.end, Some(SettlingEnd::Ended(Ok(bytes))) if bytes == &[0x62, 0xF1, 0x90, 0x41]),
            );
            assert!(
                !responded.takes(&item(Some(COP), result(&[0x62]))),
                "it has ended"
            );
        }
        // The cancelled status alone is the agent's cancel; the finished status alone, no
        // response.
        let mut cancel = settling();
        cancel.read(item(Some(COP), cancelled.clone()), None);
        assert!(matches!(cancel.end, Some(SettlingEnd::Cancelled)));
        // A cancelled status the agent's cancel did not give (the handle had ended) is no
        // cancel of the agent's: it ends the primitive as its exchange's wait would have.
        let mut not_ours = carrying(false);
        not_ours.read(item(Some(COP), cancelled), None);
        assert!(matches!(
            not_ours.end,
            Some(SettlingEnd::Ended(Err(HostError::NoResponse)))
        ));
        // Its own results and errors are kept, for the exchange in flight when no end of it
        // is written; its status and a lost VCI of no primitive are not.
        let mut kept = settling();
        kept.read(item(Some(COP), result(&[0x62, 0xF1, 0x91])), None);
        kept.read(
            item(
                Some(COP),
                event_item::Data::ErrorData(ErrorData {
                    error_event: PduErrorEvent::PduErrEvtRxError as i32,
                    ..ErrorData::default()
                }),
            ),
            None,
        );
        kept.read(
            item(
                None,
                event_item::Data::ErrorData(ErrorData {
                    error_event: PduErrorEvent::PduErrEvtLostCommToVci as i32,
                    ..ErrorData::default()
                }),
            ),
            None,
        );
        assert_eq!(kept.items.len(), 2);
        let mut silent = settling();
        silent.read(item(Some(COP), finished), None);
        assert!(matches!(
            silent.end,
            Some(SettlingEnd::Ended(Err(HostError::NoResponse)))
        ));
        let mut gone = settling();
        gone.read(lost, None);
        assert!(matches!(
            gone.end,
            Some(SettlingEnd::Ended(Err(HostError::EventsLost)))
        ));
    }

    /// ADR-358: at job end a stray primitive has ended on its own finished or cancelled status,
    /// or on a lost VCI of no primitive or of it.
    #[test]
    fn a_stray_primitive_ends_on_its_own_end_or_a_lost_vci() {
        let other = ComPrimitiveHandle {
            cop_handle: COP.cop_handle + 1,
            ..COP
        };
        let error = |code: PduErrorEvent| {
            event_item::Data::ErrorData(ErrorData {
                error_event: code as i32,
                ..ErrorData::default()
            })
        };
        for end in [
            PduComPrimitiveStatus::PduCopstFinished,
            PduComPrimitiveStatus::PduCopstCancelled,
        ] {
            assert!(ends_primitive(&event(Some(COP), status(end)), COP));
            assert!(!ends_primitive(&event(Some(other), status(end)), COP));
        }
        assert!(!ends_primitive(
            &event(Some(COP), status(PduComPrimitiveStatus::PduCopstExecuting)),
            COP
        ));
        assert!(!ends_primitive(&event(Some(COP), result(&[0x62])), COP));
        let lost = error(PduErrorEvent::PduErrEvtLostCommToVci);
        assert!(ends_primitive(&event(None, lost.clone()), COP));
        assert!(ends_primitive(&event(Some(COP), lost.clone()), COP));
        assert!(!ends_primitive(&event(Some(other), lost), COP));
        assert!(!ends_primitive(
            &event(Some(COP), error(PduErrorEvent::PduErrEvtRxTimeout)),
            COP
        ));
    }

    /// Design 8.11 "Settlement": only a primitive whose end was read counts as ended, so one
    /// the host stopped waiting for is cancelled before the next request.
    #[test]
    fn a_primitive_counts_as_ended_only_when_its_end_was_read() {
        let mut finished = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        assert!(
            finished
                .on_event(event(Some(COP), result(&[0x62, 0xF1, 0x90])), COP)
                .is_none()
        );
        assert!(!finished.ended, "a response alone does not end it");
        assert!(
            finished
                .on_event(
                    event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
                    COP
                )
                .is_some()
        );
        assert!(finished.ended);
        let mut cancelled = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        cancelled.on_event(
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
            COP,
        );
        assert!(cancelled.ended);
        // Lost events end the wait, not the primitive.
        let mut lost = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let done = lost.on_notification(
            Some(event_notification::EventData::Lost(Default::default())),
            COP,
        );
        assert!(matches!(done, Some(Err(HostError::EventsLost))), "{done:?}");
        assert!(!lost.ended);
    }

    /// Design 8.11 `times`: the final response's item gives the response's end, and the
    /// transmit done and start it carries, on its clock; the error time is that of the error
    /// event the entry names.
    #[test]
    fn the_response_s_item_and_the_named_error_give_their_vci_times() {
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let response = EventItem {
            timestamp: 900,
            timestamp_clock: TimestampClock::Device as i32,
            ..event(
                Some(COP),
                event_item::Data::ResultData(vci_service_interface::ResultData {
                    data_bytes: vec![0x62, 0xF1, 0x90],
                    tx_msg_done_timestamp: Some(100),
                    start_msg_timestamp: Some(600),
                    ..Default::default()
                }),
            )
        };
        assert!(progress.on_event(response, COP).is_none());
        let on = |us, clock: Option<Clock>| Some(VciTime::new(us, clock));
        assert_eq!(
            progress.response_times,
            VciTimes {
                tx_done: on(100, Some(Clock::Device)),
                rx_start: on(600, Some(Clock::Device)),
                rx_end: on(900, Some(Clock::Device)),
                error: None,
                deadline: None,
                response_read_us: None,
                rx_timeout: None,
            }
        );
        // A receive timeout, the event a silent exchange names, has its own item's time and
        // clock; an item that names no clock gives a time with none.
        let mut silent = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let timeout = EventItem {
            timestamp: 1_500,
            timestamp_clock: TimestampClock::Worker as i32,
            ..event(
                Some(COP),
                event_item::Data::ErrorData(ErrorData {
                    error_event: PduErrorEvent::PduErrEvtRxTimeout as i32,
                    extra_error_info: None,
                    extra_error_info_name: None,
                }),
            )
        };
        let fault = EventItem {
            timestamp: 1_200,
            ..event(
                Some(COP),
                event_item::Data::ErrorData(ErrorData {
                    error_event: PduErrorEvent::PduErrEvtFrameStruct as i32,
                    extra_error_info: None,
                    extra_error_info_name: None,
                }),
            )
        };
        assert!(silent.on_event(timeout, COP).is_none());
        let sent = |progress: &mut Progress, result| Sent {
            result,
            error_events: std::mem::take(&mut progress.errors),
            errors_before_response: None,
            rx_timeout: progress.rx_timeout.take(),
            vci_lost: None,
            unbound: Vec::new(),
            pendings: Vec::new(),
            rejected: Vec::new(),
            last_rejected: false,
            bound_start: false,
            bound_tx: None,
            events_lost: None,
            agent_deadline: false,
            response_times: progress.response_times,
            deadline_reading: None,
        };
        let read = [0x22, 0xF1, 0x90];
        let named = |sent: &Sent| {
            let (_, mut detail) = sent_end(&read, &sent.result, &NegativeShape::uds(), read[0]);
            events_seen(sent, &mut detail).1
        };
        let timed_out = sent(&mut silent, Err(HostError::NoResponse));
        assert_eq!(named(&timed_out), on(1_500, Some(Clock::Worker)));
        // A failed primitive names its link error, with that item's time.
        let mut faulted = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        assert!(faulted.on_event(fault, COP).is_none());
        let failed = sent(
            &mut faulted,
            Err(HostError::PrimitiveFailed(
                PduErrorEvent::PduErrEvtFrameStruct as i32,
            )),
        );
        assert_eq!(named(&failed), on(1_200, None));
    }

    /// Design 8.11 "Matched answer": the order the items were read in says which of the
    /// exchange's link errors came before its final response.
    #[test]
    fn the_read_order_says_which_errors_came_before_the_final_response() {
        let fault = || {
            event(
                Some(COP),
                event_item::Data::ErrorData(ErrorData {
                    error_event: PduErrorEvent::PduErrEvtFrameStruct as i32,
                    extra_error_info: None,
                    extra_error_info_name: None,
                }),
            )
        };
        let answer = || event(Some(COP), result(&[0x62, 0xF1, 0x90]));
        let finished = || event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished));
        let sent_of = |events: Vec<EventItem>| {
            let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
            let result = events
                .into_iter()
                .find_map(|item| progress.on_event(item, COP))
                .unwrap();
            Sent {
                result,
                error_events: std::mem::take(&mut progress.errors),
                errors_before_response: progress.errors_before_response.take(),
                rx_timeout: None,
                vci_lost: None,
                unbound: Vec::new(),
                pendings: Vec::new(),
                rejected: Vec::new(),
                last_rejected: false,
                bound_start: false,
                bound_tx: None,
                events_lost: None,
                agent_deadline: false,
                response_times: VciTimes::default(),
                deadline_reading: None,
            }
        };
        let before = sent_of(vec![fault(), answer(), finished()]);
        assert_eq!(before.error_events.len(), 1);
        assert!(errors_before_answer(&before));
        let after = sent_of(vec![answer(), fault(), finished()]);
        assert_eq!(after.error_events.len(), 1);
        assert!(!errors_before_answer(&after));
        // One error on each side of the answer is not all before it.
        let both = sent_of(vec![fault(), answer(), fault(), finished()]);
        assert!(!errors_before_answer(&both));
        // With no answer there is none to count.
        let none = sent_of(vec![fault(), finished()]);
        assert!(none.result.is_err() && !errors_before_answer(&none));
    }

    /// Design 8.11 "Link settings for the log": the result and error items of no primitive or
    /// of an earlier one are kept, in order, for the communication log; status items and the
    /// primitive's own items are not.
    #[test]
    fn items_of_no_primitive_or_an_earlier_one_are_kept_for_the_log() {
        let other = ComPrimitiveHandle {
            cop_handle: 6,
            ..COP
        };
        let tx_done = event_item::Data::ResultData(vci_service_interface::ResultData {
            rx_flag: vec![0, 0, 0, 0x09],
            ..Default::default()
        });
        let error = |code: PduErrorEvent| {
            event_item::Data::ErrorData(ErrorData {
                error_event: code as i32,
                extra_error_info: Some(3),
                extra_error_info_name: None,
            })
        };
        let kept = [
            event(None, tx_done),
            event(Some(other), result(&[0x50, 0x01])),
            event(None, error(PduErrorEvent::PduErrEvtFrameStruct)),
            event(Some(other), error(PduErrorEvent::PduErrEvtRxTimeout)),
        ];
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let mut events = vec![
            kept[0].clone(),
            event(Some(other), status(PduComPrimitiveStatus::PduCopstFinished)),
            kept[1].clone(),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstExecuting)),
            kept[2].clone(),
            event(Some(COP), result(&[0x62, 0xF1, 0x90])),
            kept[3].clone(),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ];
        let got = events
            .drain(..)
            .find_map(|item| progress.on_event(item, COP));
        assert_eq!(got.unwrap().unwrap(), [0x62, 0xF1, 0x90]);
        let unbound: Vec<_> = progress.unbound.iter().map(|u| u.item.clone()).collect();
        assert_eq!(unbound, kept);
        assert!(progress.errors.is_empty() && progress.rx_timeout.is_none());

        // A lost VCI of no primitive ends the exchange and is its outcome, not an unbound item.
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        let lost = progress.on_event(
            event(None, error(PduErrorEvent::PduErrEvtLostCommToVci)),
            COP,
        );
        assert!(matches!(
            lost,
            Some(Err(HostError::VciLost {
                refused_after_loss: false
            }))
        ));
        assert!(progress.unbound.is_empty());
    }

    /// Design 8.11 "Link error": the log's form of an unbound error event names the kind of
    /// link error it is, except for a receive timeout, an outcome, and a lost VCI.
    #[test]
    fn an_unbound_error_names_its_link_error_kind_unless_it_is_none() {
        let error = |code: PduErrorEvent| Unbound {
            item: event(
                None,
                event_item::Data::ErrorData(ErrorData {
                    error_event: code as i32,
                    ..ErrorData::default()
                }),
            ),
            read: None,
            owner: None,
        };
        let kinds: Vec<_> = unattached_items(&[
            error(PduErrorEvent::PduErrEvtRxError),
            error(PduErrorEvent::PduErrEvtVciHardwareFault),
            error(PduErrorEvent::PduErrEvtRxTimeout),
            error(PduErrorEvent::PduErrEvtLostCommToVci),
        ])
        .into_iter()
        .map(|item| match item.data {
            UnattachedData::Error { link_error, .. } => link_error,
            UnattachedData::Result { .. } => panic!("an error"),
        })
        .collect();
        assert_eq!(
            kinds,
            [
                Some(LinkErrorKind::Receive),
                Some(LinkErrorKind::VciHardwareFault),
                None,
                None
            ]
        );
    }

    /// The log's form of the unbound items takes a result's CAN identifier from its four header
    /// bytes, and none from a header of another length.
    #[test]
    fn an_unbound_result_names_the_identifier_its_header_carries() {
        let with_header = |header: &[u8]| Unbound {
            item: event(
                None,
                event_item::Data::ResultData(vci_service_interface::ResultData {
                    rx_flag: vec![0, 0, 0, 0x09],
                    extra_info: Some(vci_service_interface::ExtraInfo {
                        header_bytes: header.to_vec(),
                        footer_bytes: Vec::new(),
                    }),
                    ..Default::default()
                }),
            ),
            read: None,
            owner: None,
        };
        let items = unattached_items(&[
            with_header(&[0, 0, 7, 0xE0]),
            with_header(&[0, 0, 7, 0xE0, 0x12]),
            Unbound {
                item: event(None, status(PduComPrimitiveStatus::PduCopstFinished)),
                read: None,
                owner: None,
            },
        ]);
        let identifiers: Vec<_> = items
            .iter()
            .map(|item| match &item.data {
                UnattachedData::Result { identifier, .. } => *identifier,
                UnattachedData::Error { .. } => panic!("a result"),
            })
            .collect();
        // The status item is not an item for the log.
        assert_eq!(identifiers, [Some(0x7E0), None]);
    }

    /// Design 8.11 "On one clock" (ADR-353): an unbound item names the clock its worker gave it,
    /// and none when the worker gives none or a value this agent does not know.
    #[test]
    fn an_unbound_item_names_the_clock_its_worker_gave_it() {
        let with_clock = |timestamp_clock| Unbound {
            item: EventItem {
                timestamp_clock,
                ..event(None, result(&[0x62]))
            },
            read: None,
            owner: None,
        };
        let clocks: Vec<_> = unattached_items(&[
            with_clock(TimestampClock::Device as i32),
            with_clock(TimestampClock::Worker as i32),
            with_clock(TimestampClock::Unspecified as i32),
            with_clock(99),
        ])
        .iter()
        .map(|item| item.clock)
        .collect();
        assert_eq!(
            clocks,
            [Some(Clock::Device), Some(Clock::Worker), None, None]
        );
    }

    /// A send-receive's answer is matched against its declared service and the declared
    /// negative-response shape (ADR-321), not the UDS rules of `positive_prefix`.
    #[test]
    fn a_send_receive_waits_for_its_declared_answer() {
        let document = crate::send_receive::tests::document_with(
            vec![Some(crate::send_receive::tests::uds_shape())],
            None,
        );
        let expected = crate::send_receive::expectation(
            &document.variant(),
            crate::send_receive::tests::READ_DTC,
        )
        .unwrap();
        let progress = Progress::expecting(expected.clone());
        assert_eq!(
            expected_responses(&progress)
                .iter()
                .map(|e| (e.response_type, e.mask_data.clone(), e.pattern_data.clone()))
                .collect::<Vec<_>>(),
            [
                (0, vec![0xFF, 0xFF], vec![0x59, 0x02]),
                (1, vec![0xFF, 0xFF], vec![0x7F, 0x19]),
            ]
        );
        let run = |events: Vec<EventItem>| {
            let mut progress = Progress::expecting(expected.clone());
            events
                .into_iter()
                .find_map(|item| progress.on_event(item, COP))
        };
        // Another sub-function's answer and a pending answer are not the result.
        let got = run(vec![
            event(Some(COP), result(&[0x59, 0x01, 0xFF, 0x01, 0x00, 0x02])),
            event(Some(COP), result(&[0x7F, 0x19, 0x78])),
            event(Some(COP), result(&[0x59, 0x02, 0xFF])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]);
        assert_eq!(got.unwrap().unwrap(), [0x59, 0x02, 0xFF]);
        let got = run(vec![
            event(Some(COP), result(&[0x7F, 0x22, 0x31])),
            event(Some(COP), result(&[0x7F, 0x19, 0x31])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]);
        assert_eq!(got.unwrap().unwrap(), [0x7F, 0x19, 0x31]);
    }

    #[test]
    fn only_answers_to_the_request_are_its_result() {
        // A late answer to another service, a negative response to one, and a truncated
        // negative response do not answer ReadDataByIdentifier.
        let got = outcome(vec![
            event(Some(COP), result(&[0x50, 0x03])),
            event(Some(COP), result(&[0x7F, 0x10, 0x22])),
            event(Some(COP), result(&[0x7F, 0x22])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]);
        assert!(matches!(got, Some(Err(HostError::NoResponse))), "{got:?}");
    }

    #[test]
    fn a_positive_response_must_echo_the_request() {
        // A positive response for another DID does not end a VIN read.
        let got = outcome(vec![
            event(Some(COP), result(&[0x62, 0xF1, 0x86, 0x01])),
            event(Some(COP), result(&[0x62, 0xF1, 0x90, 0x4E])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
        ]);
        assert_eq!(got.unwrap().unwrap(), [0x62, 0xF1, 0x90, 0x4E]);

        let answers = |request: &[u8], response: &[u8]| {
            Progress::for_request(request).unwrap().answers(response)
        };
        assert!(answers(&[0x22, 0xF1, 0x90], &[0x62, 0xF1, 0x90]));
        assert!(!answers(&[0x22, 0xF1, 0x90], &[0x62, 0xF1]));
        assert!(answers(&[0x22, 0xF1, 0x90], &[0x7F, 0x22, 0x31]));
        // Several DIDs: any positive response to the service.
        assert!(answers(
            &[0x22, 0xF1, 0x90, 0xF1, 0x86],
            &[0x62, 0xF1, 0x86, 0x01]
        ));
        assert!(answers(&[0x19, 0x02, 0x08], &[0x59, 0x02, 0xFF]));
        assert!(!answers(&[0x19, 0x02, 0x08], &[0x59, 0x03, 0xFF]));
        assert!(answers(&[0x3E, 0x00], &[0x7E, 0x00]));
        assert!(answers(&[0x3E, 0x80], &[0x7E, 0x00]));
        // A seed or key response echoes the security level it answers.
        assert!(answers(&[0x27, 0x01], &[0x67, 0x01, 0xAA]));
        assert!(!answers(&[0x27, 0x01], &[0x67, 0x03, 0xAA]));
        assert!(answers(&[0x27, 0x02, 0x12], &[0x67, 0x02]));
        assert!(answers(&[0xFF], &[0x3F]));
    }

    #[test]
    fn the_worker_is_asked_for_answers_to_the_request_only() {
        let expected = expected_responses(&Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap());
        let patterns: Vec<_> = expected
            .iter()
            .map(|e| (e.mask_data.as_slice(), e.pattern_data.as_slice()))
            .collect();
        assert_eq!(
            patterns,
            [
                (&[0xFF, 0xFF, 0xFF][..], &[0x62, 0xF1, 0x90][..]),
                (&[0xFF, 0xFF][..], &[0x7F, 0x22][..])
            ]
        );
    }

    #[test]
    fn lost_events_end_the_primitive_at_once() {
        let mut progress = Progress::for_request(&[0x22, 0xF1, 0x90]).unwrap();
        assert!(
            progress
                .on_notification(
                    Some(event_notification::EventData::Item(event(
                        Some(COP),
                        result(&[0x62])
                    ))),
                    COP
                )
                .is_none()
        );
        assert!(matches!(
            progress.on_notification(
                Some(event_notification::EventData::Lost(
                    vci_service_interface::LostEventItemNotification {}
                )),
                COP
            ),
            Some(Err(HostError::EventsLost))
        ));
    }

    #[test]
    fn a_negative_response_is_a_result() {
        let got = outcome(vec![
            event(Some(COP), result(&[0x7F, 0x22, 0x31])),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
        ]);
        assert_eq!(got.unwrap().unwrap(), [0x7F, 0x22, 0x31]);
    }

    #[test]
    fn a_primitive_without_a_final_response_fails() {
        // Only a response pending arrived before the primitive ended.
        assert!(matches!(
            outcome(vec![
                event(Some(COP), result(&[0x7F, 0x22, 0x78])),
                event(Some(COP), result(&[0x7F, 0x22, 0x78, 0x00])),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
            ]),
            Some(Err(HostError::NoResponse))
        ));
        // A transmit error bound to the primitive is a failed primitive.
        let tx_error = PduErrorEvent::PduErrEvtTxError as i32;
        let failed = outcome(vec![
            event(
                Some(COP),
                event_item::Data::ErrorData(ErrorData {
                    error_event: tx_error,
                    extra_error_info: None,
                    extra_error_info_name: None,
                }),
            ),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
        ]);
        let Some(Err(error @ HostError::PrimitiveFailed(code))) = failed else {
            panic!("{failed:?}");
        };
        assert_eq!(code, tx_error);
        assert!(error.to_string().contains("TX_ERROR"), "{error}");
        // The worker reports a lost VCI with no primitive handle, then cancels the primitive:
        // the lost VCI ends the primitive at once, before the cancellation (ADR-289).
        let lost = PduErrorEvent::PduErrEvtLostCommToVci as i32;
        assert!(matches!(
            outcome(vec![event(
                None,
                event_item::Data::ErrorData(ErrorData {
                    error_event: lost,
                    extra_error_info: None,
                    extra_error_info_name: None,
                })
            )]),
            Some(Err(HostError::VciLost {
                refused_after_loss: false
            }))
        ));
        assert!(matches!(
            outcome(vec![
                event(
                    None,
                    event_item::Data::ErrorData(ErrorData {
                        error_event: lost,
                        extra_error_info: None,
                        extra_error_info_name: None,
                    })
                ),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
            ]),
            Some(Err(HostError::VciLost {
                refused_after_loss: false
            }))
        ));
        // Bound to the primitive it counts the same, also after a response pending.
        assert!(matches!(
            outcome(vec![
                event(Some(COP), result(&[0x7F, 0x22, 0x78])),
                event(
                    Some(COP),
                    event_item::Data::ErrorData(ErrorData {
                        error_event: lost,
                        extra_error_info: None,
                        extra_error_info_name: None,
                    })
                ),
            ]),
            Some(Err(HostError::VciLost {
                refused_after_loss: false
            }))
        ));
        // One bound to an earlier primitive is not this request's: the primitive goes on.
        assert!(
            outcome(vec![event(
                Some(ComPrimitiveHandle {
                    cop_handle: COP.cop_handle + 1,
                    ..COP
                }),
                event_item::Data::ErrorData(ErrorData {
                    error_event: lost,
                    extra_error_info: None,
                    extra_error_info_name: None,
                })
            )])
            .is_none()
        );
        // A receive timeout is no response, not a failed primitive.
        let timeout = PduErrorEvent::PduErrEvtRxTimeout as i32;
        assert!(matches!(
            outcome(vec![
                event(
                    Some(COP),
                    event_item::Data::ErrorData(ErrorData {
                        error_event: timeout,
                        extra_error_info: None,
                        extra_error_info_name: None,
                    })
                ),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
            ]),
            Some(Err(HostError::NoResponse))
        ));
        // Nothing ends the primitive yet.
        assert!(outcome(vec![event(Some(COP), result(&[0x62]))]).is_none());
    }

    /// A primitive refused on an offline link whose last error is the lost VCI is `VciLost`
    /// (ADR-294); the same refusal with another event, or none, and any other status stay as
    /// they are.
    #[test]
    fn an_offline_link_after_a_lost_vci_is_vci_lost() {
        use vci_service_interface::{ErrorDetail, ErrorEventData, PduError};
        let refused = |code, event: Option<PduErrorEvent>| HostError::Rpc {
            rpc: "StartComPrimitive",
            status: Box::new(vci_service_interface::status_with_error_detail(
                code,
                "logical link must be connected before starting a primitive",
                ErrorDetail {
                    pdu_error: PduError::PduErrCllNotConnected as i32,
                    error_event_data: event.map(|event| ErrorEventData {
                        error_event: event as i32,
                        cop_handle: None,
                        timestamp: 1,
                        extra_error_info: 0,
                    }),
                    detail_text: None,
                    refusal: None,
                },
            )),
        };
        assert!(matches!(
            lost_vci_between_primitives(refused(
                tonic::Code::FailedPrecondition,
                Some(PduErrorEvent::PduErrEvtLostCommToVci)
            )),
            HostError::VciLost {
                refused_after_loss: true
            }
        ));
        for error in [
            refused(tonic::Code::FailedPrecondition, None),
            refused(
                tonic::Code::FailedPrecondition,
                Some(PduErrorEvent::PduErrEvtTxError),
            ),
            refused(
                tonic::Code::Internal,
                Some(PduErrorEvent::PduErrEvtLostCommToVci),
            ),
            HostError::Rpc {
                rpc: "StartComPrimitive",
                status: Box::new(tonic::Status::failed_precondition("no detail")),
            },
            HostError::NoResponse,
        ] {
            let kept = format!("{error:?}");
            assert_eq!(format!("{:?}", lost_vci_between_primitives(error)), kept);
        }
    }

    /// One error of each variant group, for the `send_state` tests.
    fn refused_start(reason: Option<vci_service_interface::RefusalReason>) -> HostError {
        use vci_service_interface::{ErrorDetail, PduError};
        HostError::Rpc {
            rpc: "StartComPrimitive",
            status: Box::new(vci_service_interface::status_with_error_detail(
                tonic::Code::InvalidArgument,
                "refused",
                ErrorDetail {
                    pdu_error: PduError::PduErrInvalidParameters as i32,
                    error_event_data: None,
                    detail_text: None,
                    refusal: reason.map(|reason| reason as i32),
                },
            )),
        }
    }

    /// Every agent-side refusal, with the request it would be logged with.
    fn agent_refusals() -> Vec<HostError> {
        vec![
            HostError::LinkHeld,
            HostError::BadService(0x1FF),
            HostError::NotAllowed(0x2E),
            HostError::EmptyRequest,
            HostError::UndeclaredService(7),
            HostError::WrongService {
                service: 1,
                declared: 0x22,
                sent: 0x2E,
            },
            HostError::BadNegativeShape("x"),
            HostError::DownloadBySendReceive(0x34),
            HostError::UseFlashTransfer,
            HostError::TransferInterrupted(0x11),
            HostError::NoTransferActive,
            HostError::BlockTooLong {
                block: 1,
                length: 9,
                max: 8,
            },
            HostError::NoBlockLength {
                block: 1,
                response: vec![0x74],
            },
            HostError::NoArtifact,
            HostError::ArtifactRange { offset: 1, len: 2 },
            HostError::Unsupported("ReadDtc"),
            HostError::DeclaredLink("x".to_owned()),
            HostError::InputNotEstablished(diag_ir::RuntimeInput::IgnitionOn),
        ]
    }

    /// Errors that leave the send open.
    fn open_sends() -> Vec<HostError> {
        use vci_service_interface::RefusalReason;
        vec![
            HostError::WorkerUnresponsive("StartComPrimitive"),
            HostError::WorkerUnresponsive("CancelComPrimitive"),
            rpc(tonic::Code::Unavailable),
            rpc(tonic::Code::Cancelled),
            rpc(tonic::Code::Unknown),
            rpc_of("SubscribeEvent", tonic::Code::Unavailable),
            refused_start(None),
            refused_start(Some(RefusalReason::Unspecified)),
            // A reason on a call after the start proves nothing.
            HostError::Rpc {
                rpc: "SubscribeEvent",
                status: match refused_start(Some(RefusalReason::QueueFull)) {
                    HostError::Rpc { status, .. } => status,
                    _ => unreachable!(),
                },
            },
            HostError::Setup("StartComPrimitive returned no primitive handle"),
            HostError::VciLost {
                refused_after_loss: false,
            },
            HostError::NoResponse,
            HostError::EventsLost,
            HostError::EventStreamEnded,
            HostError::PrimitiveFailed(PduErrorEvent::PduErrEvtTxError as i32),
            HostError::TransferRefused {
                block: 1,
                response: vec![0x7F, 0x36, 0x73],
            },
            HostError::TransferEchoMismatch {
                block: 1,
                counter: 1,
                response: vec![0x76, 0x02],
            },
        ]
    }

    #[test]
    fn send_state_is_not_sent_only_on_a_refusal_before_the_start() {
        use vci_service_interface::RefusalReason;
        for error in agent_refusals() {
            assert_eq!(error.send_state(), SendState::NotSent, "{error:?}");
        }
        for reason in [
            RefusalReason::InvalidParameters,
            RefusalReason::QueueFull,
            RefusalReason::ProtocolCannotCarry,
        ] {
            assert_eq!(
                refused_start(Some(reason)).send_state(),
                SendState::NotSent,
                "{reason:?}"
            );
        }
        assert_eq!(
            HostError::VciLost {
                refused_after_loss: true
            }
            .send_state(),
            SendState::NotSent
        );
        for error in open_sends() {
            assert_eq!(error.send_state(), SendState::MayHaveReached, "{error:?}");
        }
    }

    /// A start that timed out is neither a refusal nor a success (rpc-api-guide "Reading a
    /// Worker's Answer"): the request may have reached the ECU.
    #[test]
    fn a_timed_out_start_may_have_reached_the_ecu() {
        let error = HostError::WorkerUnresponsive("StartComPrimitive");
        assert_eq!(error.send_state(), SendState::MayHaveReached);
        let (end, _) = end_of(&[0x22, 0xF1, 0x90], Err(error));
        assert!(matches!(
            end,
            End::Unknown(UnknownCause::WorkerNotAnswering)
        ));
    }

    /// The error `unary` gives for a start the worker never answered is `MayHaveReached`.
    #[test]
    fn a_start_that_times_out_in_unary_may_have_reached_the_ecu() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let error = runtime
            .block_on(unary(
                Duration::from_millis(10),
                "StartComPrimitive",
                std::future::pending::<Result<tonic::Response<()>, tonic::Status>>(),
            ))
            .unwrap_err();
        assert!(matches!(
            error,
            HostError::WorkerUnresponsive("StartComPrimitive")
        ));
        assert_eq!(error.send_state(), SendState::MayHaveReached);
    }

    /// The log's `sent` is `not_sent` exactly when `send_state` says `NotSent`.
    #[test]
    fn the_log_agrees_with_send_state() {
        use crate::comm_log::exchange::{Sent as Logged, sent_of_end};
        let read = [0x22, 0xF1, 0x90];
        // An agent-side refusal is logged by `log_refused`; the others by `sent_end`.
        let refused = agent_refusals().into_iter().map(|error| {
            let end = End::NotSent(refusal_cause(&read, &error));
            (error, end)
        });
        let sent = open_sends()
            .into_iter()
            .chain([
                refused_start(Some(vci_service_interface::RefusalReason::QueueFull)),
                HostError::VciLost {
                    refused_after_loss: true,
                },
            ])
            .map(|error| {
                let result: Result<Vec<u8>, HostError> = Err(error);
                let end = sent_end(&read, &result, &NegativeShape::uds(), read[0]).0;
                let Err(error) = result else { unreachable!() };
                (error, end)
            });
        for (error, end) in refused.chain(sent) {
            assert_eq!(
                sent_of_end(&read, end.clone()) == Logged::NotSent,
                error.send_state() == SendState::NotSent,
                "{error:?}: {end:?}"
            );
        }
    }
}
