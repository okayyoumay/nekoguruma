//! `hw_t0_two_endpoint_loopback` (design 13.5, ADR-275): on a T0 loopback of two ends, each end
//! sends single frames that the other end must receive exactly, in both directions, and the
//! check records which library, ABI and `unsigned long` width the worker ran with.
//!
//! The ends are two channels of one module (one handle, two links), two modules (two handles,
//! one link each), or a VCI module and a SocketCAN interface (ADR-343); the check takes any of
//! them as [`Ends`]. Every frame goes through a guard, the VCI's or the SocketCAN end's, so the
//! frames are the T0 ones the guard allows: the sending end's own header, and the payload.
//!
//! A frame is a single ISO 15765-2 (2024) frame (clause 9.2: a payload of 1 to 7 bytes, one less
//! under extended or mixed addressing, which takes a byte of the frame for the address). The
//! receiver gets the whole frame (the sender's identifier, the address byte, the payload),
//! compared byte for byte.

use std::path::PathBuf;
use std::time::Duration;

use vci_service_interface::ComLogicalLinkHandle;
use worker_host::abi::{Abi, detect_file};

use crate::guard::socketcan::SocketCanError;
use crate::guard::{GuardedHandle, GuardedSocketCan, HandleError};
use crate::open::OpenedDevice;
use crate::profile::{Addressing, Class, Profile, Setup};
use crate::report::{Masker, Outcome};

/// How long each frame gets to arrive.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// The two ends of the loopback, in the order of the profile's endpoints.
pub enum Ends<'a> {
    /// Two channels of one module: one handle with a link per endpoint.
    OneModule(&'a mut GuardedHandle),
    /// Two modules: the handle of the first endpoint's VCI, then the second's.
    TwoModules(&'a mut GuardedHandle, &'a mut GuardedHandle),
    /// A VCI module and a SocketCAN interface, whichever endpoint each is in the profile.
    VciAndSocketCan(&'a mut GuardedHandle, &'a mut GuardedSocketCan),
}

/// One end of the loopback, as the check drives it.
enum End<'h> {
    Vci(&'h mut GuardedHandle, ComLogicalLinkHandle),
    SocketCan(&'h mut GuardedSocketCan),
}

/// What a device was opened with, for the check's result and its ABI check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFacts {
    pub library: PathBuf,
    pub abi: Abi,
    /// The `unsigned long` width the worker was started with.
    pub long_size: u8,
    /// The `LongSize` the library's registration definition declares, if it does (design 7.1.1).
    pub declared_long_size: Option<u8>,
}

impl DeviceFacts {
    /// The facts of an opened device; `declared_long_size` is what its registration definition
    /// says, which the caller looks up the way the launch did.
    pub fn of(device: &OpenedDevice, declared_long_size: Option<u8>) -> Self {
        Self {
            library: device.library().to_path_buf(),
            abi: device.abi(),
            long_size: device.long_size(),
            declared_long_size,
        }
    }
}

/// Why the check did not pass.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// The profile or the handles are not what the check needs; nothing was sent.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Handle(#[from] HandleError),
    /// The ABI or the width the worker ran with is not the library's (design 7.1.2).
    #[error("{0}")]
    Abi(String),
    /// A frame did not arrive, or arrived different.
    #[error("{0}")]
    Frame(String),
    /// The sending end could not transmit a frame (a transmit error, a cancel, no confirmation).
    #[error("{0}")]
    Send(String),
    /// The receiving end's monitor ended, so no frame can arrive.
    #[error("{0}")]
    Monitor(String),
    #[error(transparent)]
    SocketCan(#[from] SocketCanError),
}

impl CheckError {
    /// The outcome the report records.
    pub fn outcome(&self) -> Outcome {
        match self {
            CheckError::Refused(_) | CheckError::SocketCan(SocketCanError::Refused(_)) => {
                Outcome::Refused
            }
            _ => Outcome::Failed,
        }
    }
}

/// What this verifies, and no more: the ABI read from the library file's header is the ABI of the
/// worker the launch chose (the launch resolved the worker from that same header, so a mismatch
/// shows a launch that went wrong), and the width the launch passed to the worker is the
/// registration definition's `LongSize`, else the ABI's default (design 7.1.2). The worker does
/// not report its width or ABI back, so neither is read from the running worker; the frame round
/// trip is what exercises the `unsigned long` fields in practice.
pub fn check_abi(facts: &DeviceFacts) -> Result<(), CheckError> {
    let detected = detect_file(&facts.library).map_err(|error| {
        CheckError::Abi(format!(
            "cannot read the ABI of {}: {error}",
            facts.library.display()
        ))
    })?;
    if detected != facts.abi {
        return Err(CheckError::Abi(format!(
            "{} is built for {}, but the worker is {}",
            facts.library.display(),
            detected.name(),
            facts.abi.name()
        )));
    }
    let expected = facts
        .declared_long_size
        .unwrap_or_else(|| facts.abi.default_long_size());
    if facts.long_size != expected {
        return Err(CheckError::Abi(format!(
            "the worker ran with an unsigned long of {} bytes, but {} declares {expected} for {}",
            facts.long_size,
            if facts.declared_long_size.is_some() {
                "the registration definition"
            } else {
                "the ABI's default"
            },
            facts.abi.name()
        )));
    }
    Ok(())
}

/// The payload of a frame of `len` bytes sent from end `from` (0 or 1): bytes that tell the
/// directions and positions apart.
fn payload(from: usize, len: usize) -> Vec<u8> {
    let base = if from == 0 { 0xA0 } else { 0xB0 };
    (0..len)
        .map(|i| base + u8::try_from(i).unwrap_or(0))
        .collect()
}

/// The longest single-frame payload the profile's addressing allows.
fn max_payload(profile: &Profile) -> usize {
    let Setup::Loopback(ends) = &profile.setup else {
        return 0;
    };
    let addressed = ends
        .iter()
        .any(|end| end.link.addressing != Addressing::Normal);
    if addressed { 6 } else { 7 }
}

/// What serves end `end`: a handle and its link, or the SocketCAN end. `socketcan` is the
/// profile's SocketCAN endpoint, if it has one.
fn end_of<'h>(
    ends: &'h mut Ends<'_>,
    end: usize,
    socketcan: Option<usize>,
) -> Result<End<'h>, CheckError> {
    let (handle, index): (&mut GuardedHandle, usize) = match ends {
        Ends::OneModule(handle) => (&mut **handle, end),
        Ends::TwoModules(first, second) => {
            if end == 0 {
                (&mut **first, 0)
            } else {
                (&mut **second, 0)
            }
        }
        Ends::VciAndSocketCan(_, can) if socketcan == Some(end) => {
            return Ok(End::SocketCan(can));
        }
        Ends::VciAndSocketCan(handle, _) => (&mut **handle, 0),
    };
    let cll = *handle.links().get(index).ok_or_else(|| {
        CheckError::Refused(format!("the handle of endpoint {end} has no link {index}"))
    })?;
    Ok(End::Vci(handle, cll))
}

impl End<'_> {
    /// Starts listening; a SocketCAN end always does.
    async fn listen(&mut self) -> Result<(), CheckError> {
        match self {
            End::Vci(handle, cll) => {
                handle.raw_listen(*cll).await?;
                Ok(())
            }
            End::SocketCan(_) => Ok(()),
        }
    }

    /// Discards what arrived so far; `false` when the bus did not go quiet.
    async fn drain(&mut self) -> Result<bool, CheckError> {
        match self {
            End::Vci(handle, cll) => match handle.raw_drain(*cll).await {
                Ok(()) => Ok(true),
                Err(HandleError::NotQuiet) => Ok(false),
                Err(error) => Err(error.into()),
            },
            End::SocketCan(can) => match tokio::task::block_in_place(|| can.drain()) {
                Ok(()) => Ok(true),
                Err(SocketCanError::NotQuiet) => Ok(false),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// The frame the end sends for `payload`.
    fn frame(&self, payload: &[u8]) -> Result<Vec<u8>, CheckError> {
        match self {
            End::Vci(handle, cll) => Ok(handle.raw_frame(*cll, payload)?),
            End::SocketCan(can) => Ok(can.frame(payload)?),
        }
    }

    /// Sends one frame with `payload`; a failure to transmit is `Err(Ok(why))`.
    async fn send(&mut self, payload: &[u8]) -> Result<(), Result<String, CheckError>> {
        match self {
            End::Vci(handle, cll) => match handle.raw_send(*cll, payload).await {
                Ok(_) => Ok(()),
                Err(HandleError::SendFailed(why)) => Err(Ok(why)),
                Err(other) => Err(Err(other.into())),
            },
            End::SocketCan(can) => match tokio::task::block_in_place(|| can.send(payload)) {
                Ok(()) => Ok(()),
                Err(SocketCanError::Io { source, .. }) => Err(Ok(source.to_string())),
                Err(other) => Err(Err(other.into())),
            },
        }
    }

    /// Waits for `frame`; what went wrong is a [`Missed`] or an error.
    async fn receive(&mut self, frame: &[u8], timeout: Duration) -> Result<(), Missed> {
        match self {
            End::Vci(handle, cll) => match handle.raw_receive(*cll, frame, timeout).await {
                Ok(_) => Ok(()),
                Err(HandleError::MonitorEnded) => Err(Missed::MonitorEnded),
                Err(HandleError::RawBufferFull { cap }) => Err(Missed::Flooded(cap)),
                Err(HandleError::NoResponse) => Err(Missed::NoFrame),
                Err(error) => Err(Missed::Error(error.into())),
            },
            End::SocketCan(can) => {
                match tokio::task::block_in_place(|| can.receive(frame, timeout)) {
                    Ok(()) => Ok(()),
                    Err(SocketCanError::NoFrame) => Err(Missed::NoFrame),
                    Err(error) => Err(Missed::Error(error.into())),
                }
            }
        }
    }
}

/// Why an awaited frame did not arrive.
enum Missed {
    MonitorEnded,
    Flooded(usize),
    NoFrame,
    Error(CheckError),
}

/// Runs the check: both ends listen, then each sends frames of 1 to 7 bytes that the other must
/// receive exactly. `devices` are the facts of the opened devices (one per handle), whose ABI and
/// width are checked first. Returns the result message: what ran, with the device's library, ABI
/// and width, masked as the report masks.
pub async fn run(
    profile: &Profile,
    mut ends: Ends<'_>,
    devices: &[DeviceFacts],
    masker: &Masker,
    frame_timeout: Duration,
) -> Result<String, CheckError> {
    let Setup::Loopback(profile_ends) = &profile.setup else {
        return Err(CheckError::Refused(
            "the check needs a T0 profile: a loopback of two endpoints".to_owned(),
        ));
    };
    if profile.class != Class::T0 {
        return Err(CheckError::Refused("the check runs on T0 only".to_owned()));
    }
    let socketcan = profile_ends.iter().position(|end| end.vci.is_none());
    match (&ends, socketcan) {
        (Ends::VciAndSocketCan(..), Some(_))
        | (Ends::OneModule(_) | Ends::TwoModules(..), None) => {}
        (Ends::VciAndSocketCan(..), None) => {
            return Err(CheckError::Refused(
                "a SocketCAN end was given, but the profile names two VCI ends".to_owned(),
            ));
        }
        (_, Some(at)) => {
            return Err(CheckError::Refused(format!(
                "endpoint {at} is a SocketCAN end ({}), but no SocketCAN end was given",
                profile_ends[at].socketcan.as_deref().unwrap_or("?")
            )));
        }
    }
    let expected_devices = match ends {
        Ends::OneModule(_) | Ends::VciAndSocketCan(..) => 1,
        Ends::TwoModules(..) => 2,
    };
    if devices.len() != expected_devices {
        return Err(CheckError::Refused(format!(
            "{} device(s) given for {expected_devices} handle(s)",
            devices.len()
        )));
    }
    for facts in devices {
        check_abi(facts)?;
    }
    // Each handle's link serves the endpoint it stands for, in the profile's order.
    for end in 0..2 {
        let End::Vci(handle, cll) = end_of(&mut ends, end, socketcan)? else {
            continue;
        };
        let bound = handle.bound_links();
        let at = bound.get(&cll.cll_handle).map(|link| link.endpoint);
        if at != Some(end) {
            return Err(CheckError::Refused(format!(
                "the link given for endpoint {end} serves endpoint {at:?}"
            )));
        }
    }

    // Both ends listen before anything is sent.
    for end in 0..2 {
        end_of(&mut ends, end, socketcan)?.listen().await?;
    }
    let lengths = 1..=max_payload(profile);
    for len in lengths.clone() {
        for from in 0..2 {
            let to = 1 - from;
            let bytes = payload(from, len);
            // What the receiver holds already cannot answer this send: a copy of the frame
            // received before it was sent (bus traffic, an earlier run) proves nothing.
            if !end_of(&mut ends, to, socketcan)?.drain().await? {
                return Err(CheckError::Frame(format!(
                    "end {to} kept receiving frames before the {len}-byte send; the bus is not a \
                     quiet loopback"
                )));
            }
            let mut sender = end_of(&mut ends, from, socketcan)?;
            let frame = sender.frame(&bytes)?;
            match sender.send(&bytes).await {
                Ok(()) => {}
                Err(Ok(why)) => {
                    return Err(CheckError::Send(format!(
                        "end {from} could not send the {len}-byte frame {frame:02X?}: {why}"
                    )));
                }
                Err(Err(error)) => return Err(error),
            }
            let got = end_of(&mut ends, to, socketcan)?
                .receive(&frame, frame_timeout)
                .await;
            match got {
                Ok(()) => {}
                Err(Missed::MonitorEnded) => {
                    return Err(CheckError::Monitor(format!(
                        "end {to}'s monitor ended before the {len}-byte frame {frame:02X?} arrived"
                    )));
                }
                Err(Missed::Flooded(cap)) => {
                    return Err(CheckError::Frame(format!(
                        "end {to} received more than {cap} unrelated frames; the bus is not a quiet loopback"
                    )));
                }
                Err(Missed::NoFrame) => {
                    return Err(CheckError::Frame(format!(
                        "end {to} did not receive the {len}-byte frame {frame:02X?} end {from} sent within {frame_timeout:?}"
                    )));
                }
                Err(Missed::Error(error)) => return Err(error),
            }
        }
    }
    let device_text: Vec<String> = devices
        .iter()
        .map(|facts| {
            format!(
                "library {}, ABI {} (its header matches the worker), unsigned long {} bytes \
                 (the launch's setting, from the definition or the ABI default)",
                facts.library.display(),
                facts.abi.name(),
                facts.long_size
            )
        })
        .collect();
    let counted = lengths.count();
    let socketcan_text = match &ends {
        Ends::VciAndSocketCan(_, can) => format!("; SocketCAN interface {}", can.interface()),
        _ => String::new(),
    };
    Ok(masker.text(&format!(
        "{counted} frame lengths in both directions matched exactly; {}{socketcan_text}",
        device_text.join("; ")
    )))
}

#[cfg(test)]
#[path = "t0_loopback_tests.rs"]
mod tests;
