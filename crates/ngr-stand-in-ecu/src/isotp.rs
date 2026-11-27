//! The Linux ISO-TP socket (`PF_CAN`, `SOCK_DGRAM`, `CAN_ISOTP`) and the stop signals.
//!
//! The kernel segments a message into CAN frames and answers a first frame with flow control
//! (ISO 15765-2 (2024) clauses 9 and 10), so a read gives a whole message and a write takes one.
//! The sockets use the kernel's default options: normal addressing and no padding. An identifier
//! is a SocketCAN one: a 29-bit one carries the extended-frame flag ([`crate::args::socket_id`]).
//!
//! The kernel reports a failed transfer through the socket's error: the tester stopped in the
//! middle of a request (`ETIMEDOUT`), sent a wrong sequence number (`EILSEQ`), sent no flow
//! control for a response (`ECOMM`) or sent a bad one (`EMSGSIZE`, `EBADMSG`). The server logs one
//! line to stderr, treats the read as no message or drops the response, and goes on; any other
//! error ends it.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sim_ecu::Addressing;

use crate::args::CanIds;
use crate::serve::Transport;

/// Buffer of a read: larger than the 4095 bytes a first frame without the escape length gives,
/// and so than any response the server sends. A longer message, which the kernel also accepts,
/// is dropped, not cut.
const MAX_MESSAGE: usize = 4096;

/// An ISO-TP socket bound to one interface and one pair of identifiers.
#[derive(Debug)]
pub struct IsoTpSocket {
    fd: OwnedFd,
    rx_id: u32,
    tx_id: u32,
}

impl IsoTpSocket {
    /// A socket on `interface` that receives messages sent to `rx_id` and sends them with `tx_id`.
    /// The module that provides the `can-isotp` socket type must be loaded, and the interface up.
    pub fn open(interface: &str, rx_id: u32, tx_id: u32) -> io::Result<Self> {
        let name = CString::new(interface).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a NUL in the interface name: {e}"),
            )
        })?;
        // SAFETY: `name` is a NUL-terminated string that lives across the call.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain integer arguments.
        let fd = unsafe {
            libc::socket(
                libc::PF_CAN,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::CAN_ISOTP,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a socket this function just created and owns.
        let socket = Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            rx_id,
            tx_id,
        };
        let address = libc::sockaddr_can {
            can_family: libc::AF_CAN as libc::sa_family_t,
            can_ifindex: i32::try_from(index).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("interface index too large: {e}"),
                )
            })?,
            can_addr: libc::__c_anonymous_sockaddr_can_can_addr {
                tp: libc::__c_anonymous_sockaddr_can_tp { rx_id, tx_id },
            },
        };
        // SAFETY: `address` is a `sockaddr_can` of the size passed, alive across the call.
        let bound = unsafe {
            libc::bind(
                socket.fd.as_raw_fd(),
                (&raw const address).cast(),
                std::mem::size_of::<libc::sockaddr_can>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(socket)
    }

    /// Sends `message` as one ISO-TP message. A transfer the kernel reports as failed (module
    /// documentation) is logged and the message dropped.
    pub fn send(&self, message: &[u8]) -> io::Result<()> {
        // SAFETY: `message` is a readable buffer of the length passed.
        let written =
            unsafe { libc::write(self.fd.as_raw_fd(), message.as_ptr().cast(), message.len()) };
        match syscall_len(written) {
            Ok(n) if n == message.len() => Ok(()),
            Ok(_) => Err(io::Error::other("the message was written in part")),
            Err(error) if is_transfer_error(&error) => {
                self.log_transfer_error("sending", &error);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The next message, or `None` if none arrives within `timeout`.
    pub fn recv(&self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut polls = [readable(self)];
            let left = deadline.saturating_duration_since(Instant::now());
            match poll(&mut polls, left) {
                Ok(0) => return Ok(None),
                Ok(_) => return self.read(),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Reads the message that is ready. `None` for a transfer the kernel reports as failed
    /// (reading clears the error) and for a message longer than the buffer; both are logged.
    fn read(&self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = vec![0_u8; MAX_MESSAGE];
        // With MSG_TRUNC the length returned is the message's, even when it does not fit.
        // SAFETY: `buffer` is writable for the length passed.
        let read = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_TRUNC,
            )
        };
        let len = match syscall_len(read) {
            Ok(len) => len,
            Err(error) if is_transfer_error(&error) => {
                self.log_transfer_error("receiving", &error);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if len > buffer.len() {
            eprintln!(
                "ngr-stand-in-ecu: {}: dropped a request of {len} bytes, more than the {} the \
                 server reads",
                self.name(),
                buffer.len()
            );
            return Ok(None);
        }
        buffer.truncate(len);
        Ok(Some(buffer))
    }

    fn name(&self) -> String {
        format!(
            "ISO-TP socket (receive {:#X}, send {:#X})",
            self.rx_id, self.tx_id
        )
    }

    fn log_transfer_error(&self, action: &str, error: &io::Error) {
        eprintln!("ngr-stand-in-ecu: {} failed {action}: {error}", self.name());
    }
}

/// Whether `error` is the failure of one transfer, which the kernel reports through the socket,
/// not of the socket (module documentation).
fn is_transfer_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ETIMEDOUT | libc::EILSEQ | libc::ECOMM | libc::EMSGSIZE | libc::EBADMSG)
    )
}

/// The non-negative result of a system call, or its `errno` as an error.
fn syscall_len(result: isize) -> io::Result<usize> {
    usize::try_from(result).map_err(|_negative| io::Error::last_os_error())
}

fn readable(socket: &IsoTpSocket) -> libc::pollfd {
    libc::pollfd {
        fd: socket.fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Waits up to `timeout` (rounded up to a millisecond) for one of `polls`; the number ready.
fn poll(polls: &mut [libc::pollfd], timeout: Duration) -> io::Result<usize> {
    let millis = i32::try_from(timeout.as_micros().div_ceil(1_000)).unwrap_or(i32::MAX);
    // SAFETY: `polls` is a slice of `pollfd`, alive across the call, of the length passed.
    let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, millis) };
    syscall_len(ready as isize)
}

/// The two sockets of the stand-in ECU: physical requests and functional requests. Every
/// response goes out on the physical socket: a functional request is one frame (ISO 15765-2
/// (2024) 8.3.2.4), so a segmented response is a physical transfer and the tester's flow control
/// goes to the physical request ID. The kernel cannot restrict the functional socket to single
/// frames, so it also accepts a segmented functional request.
#[derive(Debug)]
pub struct IsoTpTransport {
    physical: IsoTpSocket,
    functional: IsoTpSocket,
}

impl IsoTpTransport {
    /// Opens both sockets on `interface`: the physical one receives on the request ID and sends
    /// on the response ID; the functional one only receives, on the functional ID (its send ID
    /// is the response ID, which it never uses).
    pub fn open(interface: &str, ids: &CanIds) -> io::Result<Self> {
        Ok(Self {
            physical: IsoTpSocket::open(interface, ids.request(), ids.response())?,
            functional: IsoTpSocket::open(interface, ids.functional(), ids.response())?,
        })
    }
}

impl Transport for IsoTpTransport {
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<(Addressing, Vec<u8>)>> {
        let mut polls = [readable(&self.physical), readable(&self.functional)];
        match poll(&mut polls, timeout) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            // A signal ends the wait; the caller looks at its stop flag and asks again.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(None),
            Err(error) => return Err(error),
        }
        // A set error flag counts as ready: the read returns the error and clears it.
        let (addressing, socket) = if polls[0].revents != 0 {
            (Addressing::Physical, &self.physical)
        } else {
            (Addressing::Functional, &self.functional)
        };
        Ok(socket.read()?.map(|message| (addressing, message)))
    }

    fn send(&mut self, message: &[u8]) -> io::Result<()> {
        self.physical.send(message)
    }
}

/// Set by SIGTERM and SIGINT.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Makes SIGTERM and SIGINT set the returned flag instead of ending the process.
///
/// `signal` restarts an interrupted system call (`SA_RESTART`). A SIGTERM that arrives while a
/// write waits for an earlier multi-frame send to finish makes the kernel cancel that send before
/// the write restarts; the cut-off response is what a power loss would also cut, so this is
/// accepted.
pub fn stop_on_signals() -> io::Result<&'static AtomicBool> {
    let handler = on_stop_signal as extern "C" fn(libc::c_int);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic, which a signal handler may do.
        let previous = unsafe { libc::signal(signal, handler as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(&STOP)
}
