//! [`DiagHost`] on top of the worker gRPC client (ADR-235).
//!
//! The VM is synchronous and the worker client asynchronous. The runner moves the VM and this
//! host onto a blocking thread, and each primitive blocks on its gRPC calls through the
//! runtime's [`Handle`] (item 1).

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
    /// The primitive ended without a response after the worker reported this error event
    /// (for example a transmit error or a lost VCI), so the request may not have been sent.
    #[error("the primitive failed: {}", error_event_name(*.0))]
    PrimitiveFailed(i32),
    #[error("service {0:#x} is not a UDS service ID")]
    BadService(u16),
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

/// ReadDTCInformation, reportDTCByStatusMask (ISO 14229-1 service 0x19, sub-function 0x02).
pub fn read_dtc_bytes(mask: u8) -> Vec<u8> {
    vec![0x19, 0x02, mask]
}

/// RoutineControl (ISO 14229-1 service 0x31): sub-function, routine identifier, option record.
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
    fn send_recv(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let handle = self.handle.clone();
        handle.block_on(self.send_recv_async(request))
    }

    async fn send_recv_async(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
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
                // Any response from the ECU ends the primitive.
                expected_response_array: vec![ExpectedResponseData {
                    acceptance_id: 0,
                    response_type: 0,
                    mask_data: vec![0x00],
                    pattern_data: vec![0x00],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        };
        let cop = unary(
            self.timings.unary,
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
            let mut progress = Progress::default();
            loop {
                let notification = events
                    .message()
                    .await
                    .map_err(|status| HostError::Rpc {
                        rpc: "SubscribeEvent",
                        status: Box::new(status),
                    })?
                    .ok_or(HostError::EventStreamEnded)?;
                let Some(event_notification::EventData::Item(item)) = notification.event_data
                else {
                    continue;
                };
                if let Some(done) = progress.on_event(item, cop) {
                    return done;
                }
            }
        };
        tokio::time::timeout(self.timings.send_recv, wait)
            .await
            .map_err(|_| HostError::NoResponse)?
    }
}

/// What the events of one send-receive have shown so far.
#[derive(Debug, Default)]
struct Progress {
    response: Option<Vec<u8>>,
    error: Option<i32>,
}

impl Progress {
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
            // A response pending the worker passes on is not the final response (ADR-235
            // item 3).
            Some(event_item::Data::ResultData(result))
                if !is_response_pending(&result.data_bytes) =>
            {
                self.response = Some(result.data_bytes);
            }
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

/// A negative response with code 0x78 (requestCorrectlyReceived-ResponsePending, ISO 14229-1
/// annex A).
fn is_response_pending(response: &[u8]) -> bool {
    matches!(response, [0x7F, _, 0x78])
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
            tx_id: 0x1FFF_FFFF,
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
            LinkConfig {
                rx_id: 0x2000_0000,
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
        let mut progress = Progress::default();
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
                event(Some(COP), result(&[0x7F, 0x31, 0x78])),
                event(Some(COP), status(PduComPrimitiveStatus::PduCopstFinished)),
            ]),
            Some(Err(HostError::NoResponse))
        ));
        let lost = PduErrorEvent::PduErrEvtLostCommToVci as i32;
        let failed = outcome(vec![
            event(Some(COP), event_item::Data::ErrorData(lost)),
            event(Some(COP), status(PduComPrimitiveStatus::PduCopstCancelled)),
        ]);
        let Some(Err(error @ HostError::PrimitiveFailed(code))) = failed else {
            panic!("{failed:?}");
        };
        assert_eq!(code, lost);
        assert!(error.to_string().contains("LOST_COMM_TO_VCI"), "{error}");
        // Nothing ends the primitive yet.
        assert!(outcome(vec![event(Some(COP), result(&[0x62]))]).is_none());
    }
}
