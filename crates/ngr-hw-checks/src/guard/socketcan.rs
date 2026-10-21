//! The guarded SocketCAN end of a T0 loopback (design 13.5 "Guarded VCI access", ADR-343).
//!
//! A T0 profile can wire a VCI channel to a CAN interface the host drives through Linux SocketCAN
//! ("the VCI and a second CAN interface" in design 13.5's class table). That end is not on a
//! worker, so [`decide`](super::decide) never sees its traffic. [`GuardedSocketCan`] is its guard:
//! the harness opens it for the profile's SocketCAN endpoint, and a check can only send what T0
//! allows from that end: a frame with the end's own identifier and a payload of 1 to 7 bytes. It
//! offers no call that takes a whole frame or another identifier, so the guard holds by
//! construction.
//!
//! The end carries raw CAN frames only. On an ISO 15765 loopback the VCI end's channel frames each
//! payload with the transport's protocol control information (ISO 15765-2 (2024) clause 9.6), which
//! this end does not build or parse, so a SocketCAN end on such a profile is refused. A raw CAN
//! end takes no padding and normal addressing only (the profile refuses the others), so its frame
//! is the identifier and the payload.
//!
//! A frame is written as the J2534 worker's RawMode data is (design 13.5): the identifier in four
//! bytes, most significant first, then the CAN data. A VCI end and a SocketCAN end therefore build
//! and compare the same bytes.
//!
//! The bus is behind [`CanBus`], so the guard and the check run against a fake in CI; the Linux
//! raw socket is the only real bus. On other operating systems [`GuardedSocketCan::open`] refuses.

use std::time::{Duration, Instant};

use crate::profile::{Addressing, Class, Framing, Link, Profile, Setup, framing};

/// One classical CAN frame: its identifier, whether that is a 29-bit one, and up to 8 data bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    pub id: u32,
    pub extended: bool,
    pub data: Vec<u8>,
}

impl CanFrame {
    /// The frame as the J2534 worker's RawMode data: the identifier in four bytes, most
    /// significant first, then the data.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.id.to_be_bytes().to_vec();
        bytes.extend_from_slice(&self.data);
        bytes
    }
}

/// A CAN interface the guard sends on and receives from.
pub trait CanBus: Send {
    fn send(&mut self, frame: &CanFrame) -> std::io::Result<()>;
    /// The next frame another node sent, or `None` when none arrives within `timeout`.
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<CanFrame>>;
}

/// Why the SocketCAN end did not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum SocketCanError {
    /// The profile, the endpoint or the payload is not one the guard allows; nothing was sent.
    #[error("{0}")]
    Refused(String),
    #[error("SocketCAN interface {interface}: {source}")]
    Io {
        interface: String,
        source: std::io::Error,
    },
    /// The expected frame did not arrive in time.
    #[error("no matching frame arrived in time")]
    NoFrame,
    /// The bus kept carrying frames while the end waited for a quiet interval.
    #[error("the bus did not go quiet")]
    NotQuiet,
}

/// The guarded SocketCAN end of a T0 loopback.
pub struct GuardedSocketCan {
    interface: String,
    link: Link,
    bus: Box<dyn CanBus>,
}

impl std::fmt::Debug for GuardedSocketCan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedSocketCan")
            .field("interface", &self.interface)
            .finish_non_exhaustive()
    }
}

impl GuardedSocketCan {
    /// Opens the raw CAN socket of endpoint `endpoint` of a T0 profile. The interface must be up
    /// at the profile's baud rate already: the harness changes no interface settings. Refused on
    /// an operating system other than Linux, which has no SocketCAN.
    pub fn open(profile: &Profile, endpoint: usize) -> Result<Self, SocketCanError> {
        let (interface, _) = Self::endpoint(profile, endpoint)?;
        #[cfg(target_os = "linux")]
        {
            let bus = raw::RawCanSocket::open(&interface)
                .map_err(|source| SocketCanError::Io { interface, source })?;
            Self::with_bus(profile, endpoint, Box::new(bus))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(SocketCanError::Refused(format!(
                "endpoint {endpoint} is the SocketCAN interface {interface}, and SocketCAN exists \
                 on Linux only"
            )))
        }
    }

    /// The guard over `bus` for endpoint `endpoint`, as [`open`](Self::open) builds it: for a test
    /// that gives the end a fake bus.
    pub fn with_bus(
        profile: &Profile,
        endpoint: usize,
        bus: Box<dyn CanBus>,
    ) -> Result<Self, SocketCanError> {
        let (interface, link) = Self::endpoint(profile, endpoint)?;
        Ok(Self {
            interface,
            link,
            bus,
        })
    }

    /// The interface and link of a T0 profile's raw CAN SocketCAN endpoint.
    fn endpoint(profile: &Profile, endpoint: usize) -> Result<(String, Link), SocketCanError> {
        if profile.class != Class::T0 {
            return Err(SocketCanError::Refused(
                "a SocketCAN end is a T0 tool: the profile is not T0".to_owned(),
            ));
        }
        let Setup::Loopback(ends) = &profile.setup else {
            return Err(SocketCanError::Refused(
                "the profile is not a T0 loopback".to_owned(),
            ));
        };
        let end = ends.get(endpoint).ok_or_else(|| {
            SocketCanError::Refused(format!("the profile has no endpoint {endpoint}"))
        })?;
        let interface = end.socketcan.clone().ok_or_else(|| {
            SocketCanError::Refused(format!("endpoint {endpoint} is not a SocketCAN end"))
        })?;
        let link = &end.link;
        if framing(&link.protocol) != Some(Framing::RawCan) {
            return Err(SocketCanError::Refused(format!(
                "endpoint {endpoint} is the SocketCAN interface {interface} on {:?}: a SocketCAN \
                 end carries raw CAN frames only, and an ISO 15765 end's frames carry the \
                 transport's protocol control byte, which it does not build",
                link.protocol
            )));
        }
        // The profile refuses both on a raw CAN end; checked again, since the frame has no room
        // for them.
        if link.padding.is_some() || link.addressing != Addressing::Normal {
            return Err(SocketCanError::Refused(
                "a raw CAN end takes no padding and normal addressing only".to_owned(),
            ));
        }
        Ok((interface, link.clone()))
    }

    /// The interface name, such as `can0`.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The profile link the end sends with.
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The frame this end sends for `payload`, as [`CanFrame::bytes`] writes it: the end's own
    /// identifier, then the payload. A VCI end's `GuardedHandle::raw_frame` gives the same bytes
    /// for the same raw CAN link.
    pub fn frame(&self, payload: &[u8]) -> Result<Vec<u8>, SocketCanError> {
        Ok(self.own_frame(payload)?.bytes())
    }

    /// Sends one frame with `payload`.
    pub fn send(&mut self, payload: &[u8]) -> Result<(), SocketCanError> {
        let frame = self.own_frame(payload)?;
        let result = self.bus.send(&frame);
        result.map_err(|source| self.io(source))
    }

    /// Discards what arrived so far, until a 50 ms window passes with no frame;
    /// [`SocketCanError::NotQuiet`] when the bus has not gone quiet within 2 s.
    pub fn drain(&mut self) -> Result<(), SocketCanError> {
        const QUIET: Duration = Duration::from_millis(50);
        const LIMIT: Duration = Duration::from_secs(2);
        let give_up = Instant::now() + LIMIT;
        let mut quiet_from = Instant::now();
        loop {
            let now = Instant::now();
            let quiet_until = quiet_from + QUIET;
            if now >= quiet_until {
                return Ok(());
            }
            if now >= give_up {
                return Err(SocketCanError::NotQuiet);
            }
            match self.bus.recv(quiet_until - now) {
                Ok(None) => return Ok(()),
                // A frame starts the quiet window again.
                Ok(Some(_)) => quiet_from = Instant::now(),
                Err(source) => return Err(self.io(source)),
            }
        }
    }

    /// Waits up to `timeout` for the frame whose bytes are `expected` (the other end's frame, as
    /// its `raw_frame` writes it), with the loopback's identifier width. Other frames are skipped.
    pub fn receive(&mut self, expected: &[u8], timeout: Duration) -> Result<(), SocketCanError> {
        let extended = self.link.id_bits == 29;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SocketCanError::NoFrame);
            }
            match self.bus.recv(left) {
                Ok(Some(frame)) if frame.extended == extended && frame.bytes() == expected => {
                    return Ok(());
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(SocketCanError::NoFrame),
                Err(source) => return Err(self.io(source)),
            }
        }
    }

    /// The end's frame for `payload`, or why T0 does not allow it.
    fn own_frame(&self, payload: &[u8]) -> Result<CanFrame, SocketCanError> {
        if payload.is_empty() || payload.len() > 7 {
            return Err(SocketCanError::Refused(format!(
                "a T0 frame carries a payload of 1 to 7 bytes, not {}",
                payload.len()
            )));
        }
        let extended = self.link.id_bits == 29;
        let limit = if extended { 0x1FFF_FFFF } else { 0x7FF };
        if self.link.request_id > limit {
            return Err(SocketCanError::Refused(format!(
                "identifier {:#X} does not fit {} bits",
                self.link.request_id, self.link.id_bits
            )));
        }
        Ok(CanFrame {
            id: self.link.request_id,
            extended,
            data: payload.to_vec(),
        })
    }

    fn io(&self, source: std::io::Error) -> SocketCanError {
        SocketCanError::Io {
            interface: self.interface.clone(),
            source,
        }
    }
}

/// The Linux raw CAN socket (`PF_CAN`, `SOCK_RAW`, `CAN_RAW`).
#[cfg(target_os = "linux")]
mod raw {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    use super::{CanBus, CanFrame};

    pub struct RawCanSocket(OwnedFd);

    impl RawCanSocket {
        /// A raw socket bound to `interface`. It receives every frame other nodes send, and none
        /// of its own (the socket default).
        pub fn open(interface: &str) -> std::io::Result<Self> {
            let name = CString::new(interface).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL in the name")
            })?;
            // SAFETY: `name` is a NUL-terminated string that lives across the call.
            let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
            if index == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: plain integer arguments.
            let fd = unsafe {
                libc::socket(
                    libc::PF_CAN,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                    libc::CAN_RAW,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `fd` is a socket this function just created and owns.
            let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
            // SAFETY: an all-zero `sockaddr_can` is valid; the fields set below are plain data.
            let mut address: libc::sockaddr_can = unsafe { std::mem::zeroed() };
            address.can_family = libc::AF_CAN as libc::sa_family_t;
            address.can_ifindex = i32::try_from(index).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "interface index too large",
                )
            })?;
            // SAFETY: `address` is a `sockaddr_can` of the size passed, alive across the call.
            let bound = unsafe {
                libc::bind(
                    socket.0.as_raw_fd(),
                    (&raw const address).cast(),
                    std::mem::size_of::<libc::sockaddr_can>() as libc::socklen_t,
                )
            };
            if bound < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(socket)
        }
    }

    impl CanBus for RawCanSocket {
        fn send(&mut self, frame: &CanFrame) -> std::io::Result<()> {
            if frame.data.len() > 8 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a classical CAN frame carries at most 8 bytes",
                ));
            }
            // SAFETY: an all-zero `can_frame` is valid; the fields set below are plain data.
            let mut raw: libc::can_frame = unsafe { std::mem::zeroed() };
            raw.can_id = if frame.extended {
                (frame.id & libc::CAN_EFF_MASK) | libc::CAN_EFF_FLAG
            } else {
                frame.id & libc::CAN_SFF_MASK
            };
            raw.can_dlc = frame.data.len() as u8;
            raw.data[..frame.data.len()].copy_from_slice(&frame.data);
            let size = std::mem::size_of::<libc::can_frame>();
            // SAFETY: `raw` is a `can_frame` of `size` bytes, alive across the call.
            let written = unsafe { libc::write(self.0.as_raw_fd(), (&raw const raw).cast(), size) };
            match written {
                n if n < 0 => Err(std::io::Error::last_os_error()),
                n if n as usize == size => Ok(()),
                _ => Err(std::io::Error::other("the frame was written in part")),
            }
        }

        fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<CanFrame>> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let mut poll = libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // Rounded up, so a wait of under a millisecond still waits; after an interrupted
                // poll, only the time left.
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let millis = i32::try_from(left.as_micros().div_ceil(1_000)).unwrap_or(i32::MAX);
                // SAFETY: one `pollfd`, alive across the call.
                let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    return Ok(None);
                }
                // SAFETY: an all-zero `can_frame` is valid to read into.
                let mut raw: libc::can_frame = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<libc::can_frame>();
                // SAFETY: `raw` is a writable `can_frame` of `size` bytes.
                let read = unsafe { libc::read(self.0.as_raw_fd(), (&raw mut raw).cast(), size) };
                if read < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if read as usize != size {
                    return Err(std::io::Error::other("a short CAN frame was read"));
                }
                // Error and remote frames are not data a loopback compares.
                if raw.can_id & (libc::CAN_ERR_FLAG | libc::CAN_RTR_FLAG) != 0 {
                    continue;
                }
                let extended = raw.can_id & libc::CAN_EFF_FLAG != 0;
                let id = if extended {
                    raw.can_id & libc::CAN_EFF_MASK
                } else {
                    raw.can_id & libc::CAN_SFF_MASK
                };
                let len = usize::from(raw.can_dlc).min(8);
                return Ok(Some(CanFrame {
                    id,
                    extended,
                    data: raw.data[..len].to_vec(),
                }));
            }
        }
    }
}

#[cfg(test)]
#[path = "socketcan_tests.rs"]
mod tests;
