//! [`DiagHost`] on top of the worker gRPC client (ADR-235).
//!
//! The VM is synchronous and the worker client asynchronous. The runner builds this host on the
//! job's blocking thread, and each primitive blocks on its gRPC calls through the runtime's
//! [`Handle`] (item 1).

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use diag_ir::DiagHost;
use tokio::runtime::Handle;
use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, ComPrimitiveHandle, EventItem, ExpectedResponseData,
    PduComPrimitiveStatus, PduErrorEvent, StartComPrimitiveRequest, event_item, event_notification,
};
use worker_host::client::WorkerClient;

use crate::link::{Link, LinkConfig};

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
    #[error("{0} is not supported by this agent yet")]
    Unsupported(&'static str),
    #[error("link setup: {0}")]
    Setup(&'static str),
}

fn error_event_name(code: i32) -> String {
    PduErrorEvent::try_from(code)
        .map(|event| event.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("error event {code:#x}"))
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

/// ReadDTCInformation (0x19), sub-function reportDTCByStatusMask (0x02); ISO 14229-1:2026
/// clause 11.3.
pub fn read_dtc_bytes(mask: u8) -> Vec<u8> {
    vec![0x19, 0x02, mask]
}

/// RoutineControl (0x31): sub-function, routine identifier, option record; ISO 14229-1:2026
/// clause 13.2.
pub fn routine_control_bytes(routine: u16, sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut request = vec![0x31, sub];
    request.extend_from_slice(&routine.to_be_bytes());
    request.extend_from_slice(payload);
    request
}

/// The VM's host for one job on one link.
pub struct WorkerHost {
    handle: Handle,
    client: WorkerClient,
    link: Link,
    timings: Timings,
    /// When each `Wait` inquiry started.
    waits: HashMap<u64, Instant>,
}

impl WorkerHost {
    pub fn new(handle: Handle, client: WorkerClient, link: Link, timings: Timings) -> Self {
        Self {
            handle,
            client,
            link,
            timings,
            waits: HashMap::new(),
        }
    }

    /// Gives back the client and the link, to close it.
    pub fn into_parts(self) -> (WorkerClient, Link) {
        (self.client, self.link)
    }

    /// Sends `request` on the link and returns the whole final response (positive, or a
    /// negative `7F` response), or [`HostError::NoResponse`] if the ECU did not answer.
    ///
    /// Every request leaves through here, so the policy is checked here (ADR-235 item 8).
    fn send_recv(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        crate::policy::check_request(&request, self.link.permission)?;
        let handle = self.handle.clone();
        handle.block_on(self.send_recv_async(request))
    }

    async fn send_recv_async(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let mut progress = Progress::for_request(&request)?;
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
        .await?
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
    vec![
        expected(0, &vec![0xFF; prefix.len()], prefix.clone()),
        expected(1, &[0xFF, 0xFF], vec![0x7F, progress.sid]),
    ]
}

/// How a positive response to `request` begins: the SID plus 0x40, then what the server echoes
/// of the request (ISO 14229-1:2026 clauses 9.7, 10.2 and 11.3). A sub-function is echoed
/// without its suppress bit. A DID is matched only when one was requested, since the
/// standard does not fix the order of several in the response.
fn positive_prefix(request: &[u8]) -> Vec<u8> {
    match request {
        [0x22, hi, lo] => vec![0x62, *hi, *lo],
        [0x19, sub, ..] => vec![0x59, sub & 0x7F],
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
    response: Option<Vec<u8>>,
    error: Option<i32>,
}

impl Progress {
    fn for_request(request: &[u8]) -> Result<Self, HostError> {
        let sid = *request.first().ok_or(HostError::BadService(0))?;
        Ok(Self {
            sid,
            positive_prefix: positive_prefix(request),
            response: None,
            error: None,
        })
    }

    /// Whether `response` answers the request: a positive response that begins as expected,
    /// or a complete negative response for its SID. Stricter than the worker's match, which
    /// also accepts a response shorter than the pattern.
    fn answers(&self, response: &[u8]) -> bool {
        match response {
            [0x7F, sid, _, ..] => *sid == self.sid,
            _ => response.starts_with(&self.positive_prefix),
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
        // Events of earlier primitives, and of none, are not this request's.
        if item.cop_handle != Some(cop) {
            return None;
        }
        match item.data {
            // A response pending the worker passes on, and anything that does not answer this
            // request, is not the final response (ADR-235 item 3).
            Some(event_item::Data::ResultData(result))
                if self.answers(&result.data_bytes) && !is_response_pending(&result.data_bytes) =>
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

/// A negative response with code 0x78 (requestCorrectlyReceived-ResponsePending, ISO 14229-1:2026
/// annex A.1).
fn is_response_pending(response: &[u8]) -> bool {
    matches!(response, [0x7F, _, 0x78, ..])
}

impl DiagHost for WorkerHost {
    type Error = HostError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
        let request = service_request_bytes(service, payload)?;
        self.send_recv(request)
    }

    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, HostError> {
        self.send_recv(read_dtc_bytes(mask))
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

    // Never `Ok(None)`: that means "still waiting" and the VM would wait forever (ADR-235
    // item 5).
    fn security_access(
        &mut self,
        _inquiry: u64,
        _level: u8,
        _seed: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError> {
        Err(HostError::Unsupported("SecurityAccess"))
    }

    fn flash_transfer(&mut self, _block: u32, _data: &[u8]) -> Result<(), HostError> {
        Err(HostError::Unsupported("FlashTransfer"))
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

    fn log(&mut self, level: u8, message: &str) {
        tracing::info!(level, "{message}");
    }
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
        assert_eq!(read_dtc_bytes(0x08), [0x19, 0x02, 0x08]);
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
        // for now that is no response.
        let lost = PduErrorEvent::PduErrEvtLostCommToVci as i32;
        assert!(matches!(
            outcome(vec![
                event(None, event_item::Data::ErrorData(lost)),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
            ]),
            Some(Err(HostError::NoResponse))
        ));
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
}
