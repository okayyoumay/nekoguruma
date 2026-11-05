//! The agent's record for a job's keep-alive on its own link (design 8.2.5, ADR-418, ADR-420).
//!
//! The worker sends the tester-present; the agent decides when, from what it knows of the link
//! and of the ECU's session. That knowledge is four values, each with one writer
//! ([`KeepAlive`]): the session the ECU is in, the link's communication state, whether the
//! worker's tester-present handling is believed on, and whether a switch of it failed. The
//! functions here decide; the host ([`crate::host`]) sends what they decide.

use crate::comm_log::AgentNow;
use crate::host::HostError;
use vci_service_interface::{
    ComPrimitiveHandle, EventNotification, PduErrorEvent, event_item, event_notification,
};

/// The sub-function of the default session.
pub(crate) const DEFAULT_SESSION: u8 = 0x01;
const SID_SESSION_CONTROL: u8 = 0x10;
const SID_ECU_RESET: u8 = 0x11;
const SID_POSITIVE_SESSION_CONTROL: u8 = 0x50;
/// The bit of a request's sub-function that suppresses the positive response.
const SUPPRESS_POSITIVE_RESPONSE: u8 = 0x80;

/// The state of the link as the tester-present sees it (ISO 22900-2:2022 8.2.6.3.2: the cyclic
/// tester-present runs only in COMM_STARTED, which a successful `CoptStartcomm` reaches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// ONLINE: the link opened and no StartComm has started communication.
    Online,
    /// COMM_STARTED.
    Started,
    /// A StartComm reached the worker but its request got no positive answer: the link's status
    /// is asked (ADR-420 item 3).
    Unknown,
}

/// What the agent switched on the worker's tester-present, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Switch {
    On,
    Off,
}

/// The agent's record of one link's keep-alive. Reset when a link opens, and when the host opens
/// it again (ADR-393).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeepAlive {
    /// The link keeps a session alive: its keep-alive is on and the library accepted the
    /// ComParams. Written when the link opens.
    enabled: bool,
    /// The session the ECU is in, by the sub-function of the session control that entered it.
    /// Written by [`KeepAlive::record`] only (ADR-417 item 3, ADR-420 item 4).
    session: u8,
    /// Written by [`KeepAlive::record`] (a start) and [`KeepAlive::resolved`] (its status).
    link: LinkState,
    /// Whether the worker's tester-present handling is believed on: it is at open when the
    /// keep-alive is on. Written by [`KeepAlive::switched`] after a switch succeeded.
    handling: bool,
    /// A switch-on failed: the next request of the host is refused (ADR-420 item 6).
    failed: bool,
    /// The session is lost: the worker's tester-present reported an error while the session
    /// record held a non-default session (ADR-420 item 8). Written by [`KeepAlive::observe`]
    /// only; a link that opens again starts over.
    lost: bool,
    /// The VCI was lost and the link not yet opened again: a tester-present error read after
    /// it is logged only, since the loss ends the run or reopens the link first (ADR-393).
    vci_lost: bool,
    /// The tester-present errors read whose `keep_alive` entry is not written yet.
    seen: Vec<TesterPresentSeen>,
}

/// A tester-present error event ([`KeepAlive::observe`]) read from the link's event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TesterPresentSeen {
    /// A lost-events notice read outside an exchange while a session was kept (ADR-420 item 8):
    /// the tester-present error may have been among the events lost.
    pub(crate) events_lost: bool,
    /// The extra error information as the worker gave it (ADR-351): its code and its name.
    pub(crate) extra_error_info: Option<u32>,
    pub(crate) extra_error_info_name: Option<String>,
    /// The agent's clocks when the host read it, with a log open.
    pub(crate) read: Option<AgentNow>,
    /// It made the session lost; an error read with the record in the default session, or
    /// after a loss, did not.
    pub(crate) latched: bool,
}

/// What [`KeepAlive::record`] saw, for [`KeepAlive::wanted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recorded {
    /// The session before the exchange.
    session_before: u8,
    /// The link became COMM_STARTED by this exchange's StartComm.
    started_now: bool,
}

impl Recorded {
    /// The link became COMM_STARTED by this exchange, by its StartComm or by the link's status.
    pub(crate) fn started_now(self) -> bool {
        self.started_now
    }
}

impl KeepAlive {
    /// A link that has just opened, with the keep-alive `enabled`.
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            session: DEFAULT_SESSION,
            link: LinkState::Online,
            handling: enabled,
            failed: false,
            lost: false,
            vci_lost: false,
            seen: Vec::new(),
        }
    }

    /// The one observer of the link's event stream (ADR-420 item 8): every reader of the stream
    /// passes each notification here before it hands it on. A tester-present error event, told
    /// apart by its code and not by whether it names a primitive (the J2534 worker's first
    /// tester-present of a switch-on runs inside the `CoptUpdateparam` primitive), is noted for
    /// the log; it also makes the session lost when the session record holds a non-default
    /// session and no loss is latched yet. A lost VCI is noted so that errors read after it
    /// latch nothing. Returns whether the notification was a tester-present error.
    ///
    /// `own` is the primitive the reader waits for, `in_exchange` whether the reader is the wait
    /// of an exchange. A lost-events notice read outside an exchange, while the record holds a
    /// non-default session and the handling is on, makes the session lost too: the error may
    /// have been among the events lost (the maintainer's decision, 2026-11-05). In an exchange
    /// it ends the exchange, as without a session.
    pub(crate) fn observe(
        &mut self,
        notification: &EventNotification,
        read: Option<AgentNow>,
        own: Option<ComPrimitiveHandle>,
        in_exchange: bool,
    ) -> bool {
        if matches!(
            notification.event_data,
            Some(event_notification::EventData::Lost(_))
        ) {
            if !in_exchange
                && self.session != DEFAULT_SESSION
                && self.handling
                && !self.lost
                && !self.vci_lost
            {
                self.lost = true;
                self.seen.push(TesterPresentSeen {
                    events_lost: true,
                    extra_error_info: None,
                    extra_error_info_name: None,
                    read,
                    latched: true,
                });
            }
            return false;
        }
        let Some(event_notification::EventData::Item(item)) = &notification.event_data else {
            return false;
        };
        let Some(event_item::Data::ErrorData(error)) = &item.data else {
            return false;
        };
        if error.error_event == PduErrorEvent::PduErrEvtLostCommToVci as i32 {
            // Only a loss that ends the link: one of no primitive, or of the one waited for
            // (ADR-294); an earlier primitive's is that primitive's.
            if item.cop_handle.is_none() || item.cop_handle == own {
                self.vci_lost = true;
            }
            return false;
        }
        if error.error_event != PduErrorEvent::PduErrEvtTesterPresentError as i32 {
            return false;
        }
        let latched = self.session != DEFAULT_SESSION && !self.lost && !self.vci_lost;
        self.lost |= latched;
        self.seen.push(TesterPresentSeen {
            events_lost: false,
            extra_error_info: error.extra_error_info,
            extra_error_info_name: error.extra_error_info_name.clone(),
            read,
            latched,
        });
        true
    }

    /// Whether the session is lost (a tester-present error latched it).
    pub(crate) fn session_lost(&self) -> bool {
        self.lost
    }

    /// Takes the tester-present errors whose entries are not written yet, in read order.
    pub(crate) fn take_seen(&mut self) -> Vec<TesterPresentSeen> {
        std::mem::take(&mut self.seen)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn session(&self) -> u8 {
        self.session
    }

    pub(crate) fn link(&self) -> LinkState {
        self.link
    }

    pub(crate) fn handling(&self) -> bool {
        self.handling
    }

    /// Whether `request` goes as the request of a `CoptStartcomm`: the keep-alive is on, the
    /// link is still ONLINE, and the request is a DiagnosticSessionControl into a session other
    /// than the default (ADR-420 item 3). Nothing else is: a StartComm on a started link is
    /// refused by the J2534 worker, and an earlier request has no session to keep.
    pub(crate) fn starts_comm(&self, request: &[u8]) -> bool {
        self.enabled
            && self.link == LinkState::Online
            && request.first() == Some(&SID_SESSION_CONTROL)
            && request
                .get(1)
                .is_some_and(|sub| sub & !SUPPRESS_POSITIVE_RESPONSE != DEFAULT_SESSION)
    }

    /// The session `request` leaves the ECU in when it ended as `result`, or `None` when the
    /// record does not change (ADR-417 item 3, ADR-408): a positive answer to a session control
    /// (or the silence of one whose positive response is suppressed) enters the session its
    /// sub-function names, an accepted ECUReset that resets the ECU (sub-function 01 to 03) returns to the default
    /// session, and anything
    /// else, a negative answer or a failure, leaves it as it was.
    pub(crate) fn session_after(request: &[u8], result: &Result<Vec<u8>, HostError>) -> Option<u8> {
        let accepted = match result {
            Ok(response) if response.is_empty() => true,
            Ok(response) => response.first() == Some(&positive_of(request.first().copied()?)),
            Err(_) => false,
        };
        if !accepted {
            return None;
        }
        match request {
            [SID_SESSION_CONTROL, sub, ..] => Some(sub & !SUPPRESS_POSITIVE_RESPONSE),
            // Only the sub-functions that reset the ECU (hard, key off/on, soft: 01 to 03) end
            // its session; the two rapid-power-shutdown ones (04, 05) do not (ISO 14229-1:2026
            // 9.3), and a tester-present kept running in the default session is harmless.
            [SID_ECU_RESET, sub, ..] if (1..=3).contains(&(sub & !SUPPRESS_POSITIVE_RESPONSE)) => {
                Some(DEFAULT_SESSION)
            }
            _ => None,
        }
    }

    /// Takes in how an exchange of `request` ended as `result`, which went as a StartComm when
    /// `start_comm`: the link's state and the session record (their only writer). A
    /// StartComm that ended positively, or in the silence of a suppressed positive response,
    /// started communication; any other end that reached the worker leaves the state unknown;
    /// a refusal before the start leaves it as it was.
    pub(crate) fn record(
        &mut self,
        request: &[u8],
        start_comm: bool,
        result: &Result<Vec<u8>, HostError>,
    ) -> Recorded {
        let recorded = Recorded {
            session_before: self.session,
            started_now: false,
        };
        let mut recorded = recorded;
        if start_comm {
            let not_sent = matches!(result, Err(error) if error.send_state() == crate::host::SendState::NotSent);
            if !not_sent {
                let positive = matches!(
                    result,
                    Ok(response) if response.is_empty() || response.first() == Some(&SID_POSITIVE_SESSION_CONTROL)
                );
                if positive {
                    self.link = LinkState::Started;
                    recorded.started_now = true;
                } else {
                    self.link = LinkState::Unknown;
                }
            }
        }
        if let Some(session) = Self::session_after(request, result) {
            self.session = session;
        }
        recorded
    }

    /// Whether the link's state is unknown and its status must be asked.
    pub(crate) fn needs_status(&self) -> bool {
        self.link == LinkState::Unknown
    }

    /// Takes in the status the worker gave the link, when `started` it is COMM_STARTED. Returns
    /// whether that made the link started, which `recorded` then notes.
    pub(crate) fn resolved(&mut self, started: bool, recorded: &mut Recorded) {
        self.link = if started {
            LinkState::Started
        } else {
            LinkState::Online
        };
        recorded.started_now |= started;
    }

    /// The switch the agent makes of the worker's tester-present after the exchange `recorded`
    /// (ADR-420 item 5), or `None`. On a started link only: entering a non-default session
    /// with the handling off switches it on, leaving one with the handling on switches it off,
    /// and a link that has just started with the ECU in the default session switches the
    /// handling off, since the StartComm left it on.
    pub(crate) fn wanted(&self, recorded: Recorded) -> Option<Switch> {
        if self.link != LinkState::Started {
            return None;
        }
        let was_default = recorded.session_before == DEFAULT_SESSION;
        let is_default = self.session == DEFAULT_SESSION;
        if was_default && !is_default && !self.handling {
            Some(Switch::On)
        } else if self.handling && is_default && (!was_default || recorded.started_now) {
            Some(Switch::Off)
        } else {
            None
        }
    }

    /// A switch to `switch` succeeded (its only writer after open).
    pub(crate) fn switched(&mut self, switch: Switch) {
        self.handling = switch == Switch::On;
    }

    /// The ECU is in a non-default session on a link that is ONLINE, so no tester-present can
    /// run: the status came late and said the link never started (ADR-420 item 5). The session is
    /// not kept, which is the failure of a switch-on.
    pub(crate) fn unkept(&self) -> bool {
        self.enabled && self.link == LinkState::Online && self.session != DEFAULT_SESSION
    }

    /// A switch-on failed: the next request is refused.
    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    /// Whether a switch-on failed.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }
}

/// The positive response's SID of the service `sid`: the service plus 0x40.
fn positive_of(sid: u8) -> u8 {
    sid.wrapping_add(0x40)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positive(bytes: &[u8]) -> Result<Vec<u8>, HostError> {
        Ok(bytes.to_vec())
    }

    #[test]
    fn only_a_non_default_session_control_on_an_online_link_starts_communication() {
        let on = KeepAlive::new(true);
        for (request, starts) in [
            (&[0x10, 0x02][..], true),
            (&[0x10, 0x03], true),
            // Suppressing the positive response does not change the session asked for.
            (&[0x10, 0x82], true),
            (&[0x10, 0x01], false),
            (&[0x10, 0x81], false),
            (&[0x10], false),
            (&[0x22, 0xF1, 0x90], false),
            (&[0x11, 0x01], false),
            (&[0x3E, 0x00], false),
            (&[], false),
        ] {
            assert_eq!(on.starts_comm(request), starts, "{request:02X?}");
        }
        // Keep-alive off: never.
        assert!(!KeepAlive::new(false).starts_comm(&[0x10, 0x02]));
        // Started, or unknown: the J2534 worker refuses a second StartComm.
        for link in [LinkState::Started, LinkState::Unknown] {
            let mut keep = KeepAlive::new(true);
            keep.link = link;
            assert!(!keep.starts_comm(&[0x10, 0x02]), "{link:?}");
        }
    }

    /// ADR-417 item 3 and ADR-408: what each request leaves in the session record.
    #[test]
    fn the_session_record_follows_the_answers_that_change_the_session() {
        let refused = Err(HostError::NoResponse);
        type Row<'a> = (&'a [u8], Result<Vec<u8>, HostError>, Option<u8>);
        let rows: [Row; 21] = [
            (
                &[0x10, 0x02],
                positive(&[0x50, 0x02, 0, 0x32, 1, 0xF4]),
                Some(0x02),
            ),
            (&[0x10, 0x03], positive(&[0x50, 0x03]), Some(0x03)),
            // The silence of a suppressed positive response is the session entered.
            (&[0x10, 0x82], positive(&[]), Some(0x02)),
            (&[0x10, 0x01], positive(&[0x50, 0x01]), Some(0x01)),
            (&[0x10, 0x81], positive(&[]), Some(0x01)),
            (&[0x10, 0x02], positive(&[0x7F, 0x10, 0x22]), None),
            (&[0x10, 0x02], refused, None),
            (&[0x10, 0x02], Err(HostError::LinkHeld), None),
            (&[0x11, 0x01], positive(&[0x51, 0x01]), Some(0x01)),
            (&[0x11, 0x81], positive(&[]), Some(0x01)),
            (&[0x11, 0x01], positive(&[0x7F, 0x11, 0x33]), None),
            // Rapid power shutdown does not reset the ECU (ISO 14229-1:2026 9.3).
            (&[0x11, 0x04], positive(&[0x51, 0x04, 0x10]), None),
            (&[0x11, 0x05], positive(&[0x51, 0x05]), None),
            (&[0x11, 0x84], positive(&[]), None),
            (&[0x11, 0x03], positive(&[0x51, 0x03]), Some(0x01)),
            (&[0x11, 0x02], positive(&[0x51, 0x02]), Some(0x01)),
            (&[0x11, 0x01], Err(HostError::NoResponse), None),
            // Another service's positive answer changes nothing.
            (&[0x22, 0xF1, 0x90], positive(&[0x62, 0xF1, 0x90, 1]), None),
            (&[0x22, 0xF1, 0x90], positive(&[]), None),
            // A positive answer of another service to a session control is not the session.
            (&[0x10, 0x02], positive(&[0x62, 0x02]), None),
            (&[0x3E, 0x00], positive(&[0x7E, 0x00]), None),
        ];
        for (request, result, expected) in rows {
            assert_eq!(
                KeepAlive::session_after(request, &result),
                expected,
                "{request:02X?} {result:?}"
            );
        }
    }

    #[test]
    fn a_started_link_in_a_non_default_session_keeps_its_handling_on() {
        let mut keep = KeepAlive::new(true);
        let result = positive(&[0x50, 0x02]);
        let recorded = keep.record(&[0x10, 0x02], true, &result);
        assert_eq!((keep.link(), keep.session()), (LinkState::Started, 0x02));
        // The StartComm left the handling on: no switch.
        assert_eq!(keep.wanted(recorded), None);
        assert!(keep.handling());
    }

    #[test]
    fn a_start_that_got_no_positive_answer_leaves_the_link_unknown_until_its_status_says() {
        for result in [
            positive(&[0x7F, 0x10, 0x22]),
            Err(HostError::NoResponse),
            Err(HostError::EventsLost),
        ] {
            let mut keep = KeepAlive::new(true);
            let mut recorded = keep.record(&[0x10, 0x02], true, &result);
            assert!(keep.needs_status(), "{result:?}");
            assert_eq!(keep.session(), DEFAULT_SESSION);
            assert_eq!(keep.wanted(recorded), None);
            // COMM_STARTED: the handling the StartComm left on is switched off.
            keep.resolved(true, &mut recorded);
            assert_eq!(keep.link(), LinkState::Started);
            assert_eq!(keep.wanted(recorded), Some(Switch::Off));
            keep.switched(Switch::Off);
            assert!(!keep.handling());
            // ONLINE: nothing was started, and nothing is switched.
            let mut keep = KeepAlive::new(true);
            let mut recorded = keep.record(&[0x10, 0x02], true, &result);
            keep.resolved(false, &mut recorded);
            assert_eq!(keep.link(), LinkState::Online);
            assert_eq!(keep.wanted(recorded), None);
            assert!(keep.starts_comm(&[0x10, 0x02]));
        }
    }

    #[test]
    fn a_start_refused_before_it_began_leaves_the_link_online() {
        let mut keep = KeepAlive::new(true);
        keep.record(&[0x10, 0x02], true, &Err(HostError::LinkHeld));
        assert_eq!(keep.link(), LinkState::Online);
        assert!(!keep.needs_status());
    }

    #[test]
    fn the_handling_follows_the_session_on_a_started_link() {
        let mut keep = KeepAlive::new(true);
        keep.link = LinkState::Started;
        keep.handling = false;
        // Entering a non-default session with the handling off: on.
        let recorded = keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
        assert_eq!(keep.wanted(recorded), Some(Switch::On));
        keep.switched(Switch::On);
        // Another non-default session with the handling on: nothing.
        let recorded = keep.record(&[0x10, 0x03], false, &positive(&[0x50, 0x03]));
        assert_eq!((keep.session(), keep.wanted(recorded)), (0x03, None));
        // Back to the default session: off.
        let recorded = keep.record(&[0x10, 0x01], false, &positive(&[0x50, 0x01]));
        assert_eq!(keep.wanted(recorded), Some(Switch::Off));
        keep.switched(Switch::Off);
        // The default session again: nothing.
        let recorded = keep.record(&[0x10, 0x01], false, &positive(&[0x50, 0x01]));
        assert_eq!(keep.wanted(recorded), None);
        // An accepted ECUReset from a non-default session: off; a refused one: nothing.
        keep.session = 0x02;
        keep.handling = true;
        let recorded = keep.record(&[0x11, 0x01], false, &positive(&[0x7F, 0x11, 0x33]));
        assert_eq!((keep.session(), keep.wanted(recorded)), (0x02, None));
        let recorded = keep.record(&[0x11, 0x01], false, &positive(&[0x51, 0x01]));
        assert_eq!(keep.wanted(recorded), Some(Switch::Off));
    }

    #[test]
    fn nothing_switches_on_a_link_that_is_not_started() {
        let mut keep = KeepAlive::new(true);
        // The restart's teardown on a fresh link, and a refused or a read-only job's session
        // control: the record stays the default session and the link ONLINE.
        let recorded = keep.record(&[0x11, 0x01], false, &positive(&[0x51, 0x01]));
        assert_eq!((keep.link(), keep.session()), (LinkState::Online, 1));
        assert_eq!(keep.wanted(recorded), None);
        let recorded = keep.record(&[0x10, 0x01], false, &positive(&[0x50, 0x01]));
        assert_eq!(keep.wanted(recorded), None);
        // Keep-alive off: no state to act on.
        let keep = KeepAlive::new(false);
        assert!(!keep.enabled() && !keep.handling());
    }

    /// ADR-420 item 5: a status that comes late and says ONLINE, with the ECU already in a
    /// non-default session, leaves a session no tester-present runs for.
    #[test]
    fn a_late_online_status_with_a_non_default_session_leaves_the_session_unkept() {
        let mut keep = KeepAlive::new(true);
        // The start got no answer and its status could not be read; the next session control
        // went as a send-receive and was answered.
        let _ = keep.record(&[0x10, 0x02], true, &Err(HostError::NoResponse));
        let mut recorded = keep.record(&[0x10, 0x03], false, &positive(&[0x50, 0x03]));
        assert!(!keep.unkept());
        keep.resolved(false, &mut recorded);
        assert!(keep.unkept());
        assert_eq!(keep.wanted(recorded), None);
        // Started instead: the tester-present the start left on keeps it, and the exchange that
        // resolved it says so.
        let mut keep = KeepAlive::new(true);
        let _ = keep.record(&[0x10, 0x02], true, &Err(HostError::NoResponse));
        let mut recorded = keep.record(&[0x10, 0x03], false, &positive(&[0x50, 0x03]));
        keep.resolved(true, &mut recorded);
        assert!(!keep.unkept() && recorded.started_now());
        assert_eq!(keep.wanted(recorded), None);
        // Default session, or the keep-alive off: nothing is unkept.
        let mut keep = KeepAlive::new(true);
        let mut recorded = keep.record(&[0x10, 0x02], true, &Err(HostError::NoResponse));
        keep.resolved(false, &mut recorded);
        assert!(!keep.unkept());
        assert!(!KeepAlive::new(false).unkept());
    }

    #[test]
    fn a_failed_switch_is_remembered() {
        let mut keep = KeepAlive::new(true);
        assert!(!keep.failed());
        keep.fail();
        assert!(keep.failed());
        // Opening the link again starts over.
        assert!(!KeepAlive::new(true).failed());
    }

    fn error_item(event: PduErrorEvent, handle: Option<u32>) -> EventNotification {
        EventNotification {
            event_data: Some(event_notification::EventData::Item(
                vci_service_interface::EventItem {
                    cop_handle: handle.map(|cop_handle| {
                        vci_service_interface::ComPrimitiveHandle {
                            module_handle: 1,
                            cll_handle: 1,
                            cop_handle,
                        }
                    }),
                    data: Some(event_item::Data::ErrorData(
                        vci_service_interface::ErrorData {
                            error_event: event as i32,
                            extra_error_info: Some(7),
                            extra_error_info_name: Some("no_answer".to_owned()),
                        },
                    )),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    const TESTER_PRESENT_ERROR: PduErrorEvent = PduErrorEvent::PduErrEvtTesterPresentError;

    /// ADR-420 item 8: a tester-present error makes the session lost only while the record holds
    /// a non-default session; with the record in the default session it is noted and nothing is
    /// latched.
    #[test]
    fn a_tester_present_error_latches_the_loss_only_in_a_non_default_session() {
        let mut keep = KeepAlive::new(true);
        assert!(keep.observe(&error_item(TESTER_PRESENT_ERROR, None), None, None, false));
        assert!(!keep.session_lost());
        let seen = keep.take_seen();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].latched);
        assert_eq!(seen[0].extra_error_info, Some(7));
        assert_eq!(seen[0].extra_error_info_name.as_deref(), Some("no_answer"));
        assert!(keep.take_seen().is_empty());

        keep.record(&[0x10, 0x03], false, &positive(&[0x50, 0x03]));
        assert!(keep.observe(&error_item(TESTER_PRESENT_ERROR, None), None, None, false));
        assert!(keep.session_lost());
        // A second error is noted, not latched again.
        assert!(keep.observe(&error_item(TESTER_PRESENT_ERROR, None), None, None, false));
        let latched: Vec<bool> = keep.take_seen().iter().map(|seen| seen.latched).collect();
        assert_eq!(latched, [true, false]);
        // A link that opens again starts over.
        let keep = KeepAlive::new(true);
        assert!(!keep.session_lost());
    }

    /// The first tester-present of a switch-on runs inside the update primitive of the J2534
    /// worker, so its error may carry that primitive's handle: it is told apart by its code.
    #[test]
    fn the_event_code_decides_not_the_primitive_handle() {
        let mut keep = KeepAlive::new(true);
        keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
        assert!(keep.observe(
            &error_item(TESTER_PRESENT_ERROR, Some(9)),
            None,
            None,
            false
        ));
        assert!(keep.session_lost());
        // Other errors, with or without a handle, are not it.
        let mut keep = KeepAlive::new(true);
        keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
        for handle in [None, Some(9)] {
            assert!(!keep.observe(
                &error_item(PduErrorEvent::PduErrEvtRxError, handle),
                None,
                None,
                false
            ));
        }
        let status = EventNotification {
            event_data: Some(event_notification::EventData::Item(
                vci_service_interface::EventItem::default(),
            )),
            ..Default::default()
        };
        assert!(!keep.observe(&status, None, None, false));
        assert!(!keep.observe(&EventNotification::default(), None, None, false));
        assert!(!keep.session_lost() && keep.take_seen().is_empty());
    }

    /// A tester-present error read after the VCI was lost is noted and latches nothing: the loss
    /// ends the run or opens the link again first (ADR-393).
    #[test]
    fn an_error_read_after_a_lost_vci_latches_nothing() {
        let mut keep = KeepAlive::new(true);
        keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
        assert!(!keep.observe(
            &error_item(PduErrorEvent::PduErrEvtLostCommToVci, None),
            None,
            None,
            false
        ));
        assert!(keep.observe(&error_item(TESTER_PRESENT_ERROR, None), None, None, false));
        assert!(!keep.session_lost());
        assert!(!keep.take_seen()[0].latched);
    }

    /// A lost VCI that names an earlier primitive is that primitive's, not the link's, so errors
    /// after it still latch; one of the primitive waited for, or of none, is the link's.
    #[test]
    fn only_a_lost_vci_that_ends_the_link_blocks_the_latch() {
        let own = Some(ComPrimitiveHandle {
            module_handle: 1,
            cll_handle: 1,
            cop_handle: 9,
        });
        for (handle, blocks) in [(Some(3), false), (Some(9), true), (None, true)] {
            let mut keep = KeepAlive::new(true);
            keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
            keep.observe(
                &error_item(PduErrorEvent::PduErrEvtLostCommToVci, handle),
                None,
                own,
                true,
            );
            keep.observe(&error_item(TESTER_PRESENT_ERROR, None), None, own, true);
            assert_eq!(keep.session_lost(), !blocks, "{handle:?}");
        }
    }

    /// A lost-events notice outside an exchange, with a session kept by a running handling, may
    /// have hidden the tester-present error: the session is lost. Not in an exchange (it ends
    /// that exchange), not in the default session, not with the handling off.
    #[test]
    fn a_lost_events_notice_outside_an_exchange_latches_in_a_kept_session() {
        let lost = EventNotification {
            event_data: Some(event_notification::EventData::Lost(Default::default())),
            ..Default::default()
        };
        let kept = || {
            let mut keep = KeepAlive::new(true);
            keep.record(&[0x10, 0x02], false, &positive(&[0x50, 0x02]));
            keep
        };
        let mut keep = kept();
        keep.observe(&lost, None, None, true);
        assert!(!keep.session_lost() && keep.take_seen().is_empty());
        keep.observe(&lost, None, None, false);
        assert!(keep.session_lost());
        let seen = keep.take_seen();
        assert!(seen[0].events_lost && seen[0].latched);
        let mut keep = KeepAlive::new(true);
        keep.observe(&lost, None, None, false);
        assert!(!keep.session_lost());
        let mut keep = kept();
        keep.switched(Switch::Off);
        keep.observe(&lost, None, None, false);
        assert!(!keep.session_lost());
    }
}
