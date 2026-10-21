use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::*;
use crate::profile::parse;

/// A T0 loopback of a SocketCAN end (endpoint 0) and a VCI end, both with `protocol`; `first`
/// and `second` are appended to each end's link.
fn profile_with(protocol: &str, first: &str, second: &str) -> Profile {
    parse(&format!(
        r#"
class = "T0"

[[endpoint]]
socketcan = "can0"
[endpoint.link]
protocol = "{protocol}"
baud_rate = 500000
pins = [6, 14]
request_id = 0x123
response_id = 0x124
{first}

[[endpoint]]
[endpoint.vci]
api = "j2534-0404"
library = "Vendor VCI"
module = "VCI-1"
label = "bench"
pname = "Vendor VCI 1"
firmware_version = "1.2.3"
library_version = "4.5"
[endpoint.link]
protocol = "{protocol}"
baud_rate = 500000
pins = [6, 14]
request_id = 0x124
response_id = 0x123
{second}
"#
    ))
    .unwrap()
}

fn profile() -> Profile {
    profile_with(
        "CAN",
        "id_bits = 11\naddressing = \"normal\"",
        "id_bits = 11\naddressing = \"normal\"",
    )
}

/// A bus that records what is sent and serves what a test queues.
#[derive(Clone, Default)]
struct FakeBus {
    sent: Arc<Mutex<Vec<CanFrame>>>,
    inbound: Arc<Mutex<VecDeque<CanFrame>>>,
    /// Frames served without end, as a busy bus does.
    busy: bool,
}

impl CanBus for FakeBus {
    fn send(&mut self, frame: &CanFrame) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(frame.clone());
        Ok(())
    }

    fn recv(&mut self, _: Duration) -> std::io::Result<Option<CanFrame>> {
        if self.busy {
            return Ok(Some(CanFrame {
                id: 0x555,
                extended: false,
                data: vec![1],
            }));
        }
        Ok(self.inbound.lock().unwrap().pop_front())
    }
}

fn end(profile: &Profile, bus: &FakeBus) -> GuardedSocketCan {
    GuardedSocketCan::with_bus(profile, 0, Box::new(bus.clone())).unwrap()
}

#[test]
fn the_end_sends_only_its_own_single_frames() {
    let bus = FakeBus::default();
    let mut can = end(&profile(), &bus);
    assert_eq!(can.interface(), "can0");
    can.send(&[0xA0, 0xA1]).unwrap();
    assert_eq!(
        *bus.sent.lock().unwrap(),
        [CanFrame {
            id: 0x123,
            extended: false,
            data: vec![0xA0, 0xA1],
        }]
    );
    assert_eq!(
        can.frame(&[0xA0, 0xA1]).unwrap(),
        [0x00, 0x00, 0x01, 0x23, 0xA0, 0xA1]
    );
    // No empty frame, and no payload longer than a single frame takes.
    for payload in [&[][..], &[0u8; 8][..]] {
        assert!(
            matches!(can.send(payload), Err(SocketCanError::Refused(_))),
            "{payload:?}"
        );
    }
    can.send(&[0; 7]).unwrap();
    assert_eq!(bus.sent.lock().unwrap().len(), 2);
}

/// A 29-bit end, with mixed addressing and padding: the frame carries the address byte, is padded
/// on the bus, and its identifier is a 29-bit one.
#[test]
fn the_frame_follows_the_end_s_link() {
    let bus = FakeBus::default();
    let profile = profile_with(
        "ISO15765",
        "id_bits = 29\naddressing = \"mixed\"\naddress = 0x12\npadding = 0xCC",
        "id_bits = 29\naddressing = \"mixed\"\naddress = 0x12\npadding = 0xCC",
    );
    let mut can = end(&profile, &bus);
    assert_eq!(can.frame(&[0xA0]).unwrap(), [0, 0, 0x01, 0x23, 0x12, 0xA0]);
    can.send(&[0xA0]).unwrap();
    assert_eq!(
        *bus.sent.lock().unwrap(),
        [CanFrame {
            id: 0x123,
            extended: true,
            data: vec![0x12, 0xA0, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC],
        }]
    );
    // An addressed frame leaves room for 6 payload bytes.
    assert!(matches!(can.send(&[0; 7]), Err(SocketCanError::Refused(_))));
}

#[test]
fn the_end_receives_the_other_end_s_frame_exactly() {
    let bus = FakeBus::default();
    let mut can = end(&profile(), &bus);
    let expected = [0x00, 0x00, 0x01, 0x24, 0xB0, 0xB1];
    {
        let mut inbound = bus.inbound.lock().unwrap();
        // Unrelated traffic first, then a near miss, then the frame.
        inbound.push_back(CanFrame {
            id: 0x555,
            extended: false,
            data: vec![0xB0, 0xB1],
        });
        inbound.push_back(CanFrame {
            id: 0x124,
            extended: false,
            data: vec![0xB0],
        });
        inbound.push_back(CanFrame {
            id: 0x124,
            extended: false,
            data: vec![0xB0, 0xB1],
        });
    }
    can.receive(&expected, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        can.receive(&expected, Duration::from_millis(10)),
        Err(SocketCanError::NoFrame)
    ));
}

/// A padding peer's frame arrives padded to a whole frame, and is matched as such.
#[test]
fn a_padding_peer_s_frame_is_matched_padded() {
    let bus = FakeBus::default();
    let profile = profile_with(
        "ISO15765",
        "id_bits = 11\naddressing = \"normal\"",
        "id_bits = 11\naddressing = \"normal\"\npadding = 0xAA",
    );
    let mut can = end(&profile, &bus);
    bus.inbound.lock().unwrap().push_back(CanFrame {
        id: 0x124,
        extended: false,
        data: vec![0xB0],
    });
    assert!(matches!(
        can.receive(&[0, 0, 0x01, 0x24, 0xB0], Duration::from_millis(10)),
        Err(SocketCanError::NoFrame)
    ));
    bus.inbound.lock().unwrap().push_back(CanFrame {
        id: 0x124,
        extended: false,
        data: vec![0xB0, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA],
    });
    can.receive(&[0, 0, 0x01, 0x24, 0xB0], Duration::from_secs(1))
        .unwrap();
}

#[test]
fn a_drain_empties_a_quiet_bus_and_gives_up_on_a_busy_one() {
    let bus = FakeBus::default();
    bus.inbound.lock().unwrap().push_back(CanFrame {
        id: 0x124,
        extended: false,
        data: vec![1],
    });
    let mut can = end(&profile(), &bus);
    can.drain().unwrap();
    assert!(bus.inbound.lock().unwrap().is_empty());
    let busy = FakeBus {
        busy: true,
        ..FakeBus::default()
    };
    let mut can = end(&profile(), &busy);
    assert!(matches!(can.drain(), Err(SocketCanError::NotQuiet)));
}

/// Only a T0 profile's SocketCAN endpoint gets a guard.
#[test]
fn only_a_t0_socketcan_endpoint_gets_a_guard() {
    let bus = FakeBus::default();
    let profile = profile();
    for endpoint in [1, 2] {
        assert!(
            matches!(
                GuardedSocketCan::with_bus(&profile, endpoint, Box::new(bus.clone())),
                Err(SocketCanError::Refused(_))
            ),
            "{endpoint}"
        );
    }
    let mut t1 = profile.clone();
    t1.class = Class::T1;
    assert!(matches!(
        GuardedSocketCan::with_bus(&t1, 0, Box::new(bus)),
        Err(SocketCanError::Refused(_))
    ));
}

/// Opening fails on an interface that does not exist on Linux, and is refused everywhere else,
/// before anything is sent.
#[test]
fn an_unknown_interface_does_not_open() {
    let mut profile = profile();
    let Setup::Loopback(ends) = &mut profile.setup else {
        unreachable!()
    };
    ends[0].socketcan = Some("ngr-no-such-can".to_owned());
    let opened = GuardedSocketCan::open(&profile, 0);
    #[cfg(target_os = "linux")]
    assert!(
        matches!(opened, Err(SocketCanError::Io { .. })),
        "{opened:?}"
    );
    #[cfg(not(target_os = "linux"))]
    assert!(
        matches!(opened, Err(SocketCanError::Refused(_))),
        "{opened:?}"
    );
}
