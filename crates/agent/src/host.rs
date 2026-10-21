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
    ComOperationType, ComPrimitiveCtrlData, ComPrimitiveHandle, EventItem, ExpectedResponseData,
    PduComPrimitiveStatus, PduErrorEvent, StartComPrimitiveRequest, event_item, event_notification,
};

use crate::link::{Link, LinkConfig};
use crate::send_receive::{Expectation, NegativeShape};

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("{rpc} failed: {status}")]
    Rpc {
        rpc: &'static str,
        status: Box<tonic::Status>,
    },
    #[error("the worker did not answer {0} in time")]
    WorkerUnresponsive(&'static str),
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
    #[error("the VCI was lost")]
    VciLost,
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
            HostError::VciLost
        }
        _ => error,
    }
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
        }
    }

    /// Gives the host the job's document (`crate::document`).
    pub fn set_document(&mut self, document: Option<crate::document::JobDocument>) {
        self.document = document;
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
        crate::policy::check_request(&request, self.link.application_layer, self.link.permission)?;
        let progress = Progress::for_request(&request)?;
        let handle = self.handle.clone();
        handle.block_on(self.send_recv_async(request, progress))
    }

    /// [`send_recv`](Self::send_recv) for a generic send-receive, whose answer is matched
    /// against `expected` from the job's document instead of UDS's rules (ADR-321).
    fn send_expecting(
        &mut self,
        request: Vec<u8>,
        expected: Expectation,
    ) -> Result<Vec<u8>, HostError> {
        crate::policy::check_request(&request, self.link.application_layer, self.link.permission)?;
        let progress = Progress::expecting(expected);
        let handle = self.handle.clone();
        handle.block_on(self.send_recv_async(request, progress))
    }

    async fn send_recv_async(
        &mut self,
        request: Vec<u8>,
        mut progress: Progress,
    ) -> Result<Vec<u8>, HostError> {
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
        // One deadline from the request to the end of the primitive.
        let now = tokio::time::Instant::now();
        let deadline = now + self.timings.send_recv;
        // Starting the primitive counts against the same budget.
        let cop = unary(
            self.timings.unary.min(self.timings.send_recv),
            "StartComPrimitive",
            self.client.start_com_primitive(start),
        )
        .await
        .map_err(lost_vci_between_primitives)?
        .cop_handle
        // Without a handle, events of no primitive would match.
        .ok_or(HostError::Setup(
            "StartComPrimitive returned no primitive handle",
        ))?;

        let events = &mut self.link.events;
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
                if let Some(done) = progress.on_notification(notification.event_data, cop) {
                    return done;
                }
            }
        };
        tokio::time::timeout_at(deadline, wait)
            .await
            .map_err(|_| HostError::NoResponse)?
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
fn positive_prefix(request: &[u8]) -> Vec<u8> {
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
}

impl Progress {
    fn for_request(request: &[u8]) -> Result<Self, HostError> {
        let sid = *request.first().ok_or(HostError::BadService(0))?;
        Ok(Self {
            sid,
            positive_prefix: positive_prefix(request),
            negative: NegativeShape::uds(),
            response: None,
            error: None,
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
            && matches!(
                item.data,
                Some(event_item::Data::ErrorData(code))
                    if code == PduErrorEvent::PduErrEvtLostCommToVci as i32
            )
        {
            return Some(Err(HostError::VciLost));
        }
        // Events of earlier primitives, and of none, are not this request's.
        if item.cop_handle != Some(cop) {
            return None;
        }
        match item.data {
            // A response pending the worker passes on, and anything that does not answer this
            // request, is not the final response (ADR-235 item 3).
            Some(event_item::Data::ResultData(result))
                if self.answers(&result.data_bytes)
                    && !self.negative.is_pending(self.sid, &result.data_bytes) =>
            {
                self.response = Some(result.data_bytes);
            }
            // A receive timeout means the ECU did not answer (or did not accept the rest of a
            // segmented request), which is `NoResponse`, not a failed transmission.
            Some(event_item::Data::ErrorData(code))
                if code == PduErrorEvent::PduErrEvtRxTimeout as i32 => {}
            Some(event_item::Data::ErrorData(code)) => self.error = Some(code),
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
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
        let request = service_request_bytes(service, payload)?;
        // Every TransferData goes through the block counter of `flash_transfer`.
        refuse_transfer_data(&request)?;
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
        let expected = crate::send_receive::prepare(
            self.document.as_ref(),
            service,
            request,
            self.transfer.accepted.is_some(),
        )?;
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
        let (index, counter) = self.transfer.next()?;
        self.transfer.check_length(index, data.len())?;
        let outcome = self.send_recv(transfer_data_bytes(counter, data));
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

    fn log(&mut self, level: u8, message: &str) {
        tracing::info!(level, "{message}");
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
            // An earlier primitive's late events, and events of no primitive, are skipped.
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
            event(Some(COP), event_item::Data::ErrorData(tx_error)),
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
            outcome(vec![event(None, event_item::Data::ErrorData(lost))]),
            Some(Err(HostError::VciLost))
        ));
        assert!(matches!(
            outcome(vec![
                event(None, event_item::Data::ErrorData(lost)),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
            ]),
            Some(Err(HostError::VciLost))
        ));
        // Bound to the primitive it counts the same, also after a response pending.
        assert!(matches!(
            outcome(vec![
                event(Some(COP), result(&[0x7F, 0x22, 0x78])),
                event(Some(COP), event_item::Data::ErrorData(lost)),
            ]),
            Some(Err(HostError::VciLost))
        ));
        // One bound to an earlier primitive is not this request's: the primitive goes on.
        assert!(
            outcome(vec![event(
                Some(ComPrimitiveHandle {
                    cop_handle: COP.cop_handle + 1,
                    ..COP
                }),
                event_item::Data::ErrorData(lost)
            )])
            .is_none()
        );
        // A receive timeout is no response, not a failed primitive.
        let timeout = PduErrorEvent::PduErrEvtRxTimeout as i32;
        assert!(matches!(
            outcome(vec![
                event(Some(COP), event_item::Data::ErrorData(timeout)),
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
                },
            )),
        };
        assert!(matches!(
            lost_vci_between_primitives(refused(
                tonic::Code::FailedPrecondition,
                Some(PduErrorEvent::PduErrEvtLostCommToVci)
            )),
            HostError::VciLost
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
}
