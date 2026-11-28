//! The stand-in ECU on a virtual CAN interface (design 13.5, ADR-275): the binary serves
//! `sim-ecu` over the kernel's ISO-TP sockets, and a client socket of the test reads a DID and
//! drives a flash write that is interrupted and continued after a restart. A functional request
//! is sent from a socket that sends on the functional ID, and its answer is read, with the flow
//! control of a segmented one, by a physical client.
//!
//! The tests run only when `NGR_TEST_VCAN=1`, and then a missing `vcan0` or `can-isotp` module
//! fails them rather than skipping them. They need
//!
//! ```text
//! sudo modprobe vcan can-isotp
//! sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
//! ```
//!
//! done beforehand (creating the interface needs `CAP_NET_ADMIN`; the tests do not). Each test
//! uses CAN IDs of its own, so they can run side by side.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use ngr_stand_in_ecu::isotp::IsoTpSocket;
use sim_ecu::EcuConfig;
use sim_ecu::persist::default_ecu_config;

const INTERFACE: &str = "vcan0";
/// How long the client waits for a response the ECU gives.
const RESPONSE_WAIT: Duration = Duration::from_secs(3);
/// How long it waits to be sure the ECU gives none.
const SILENCE_WAIT: Duration = Duration::from_millis(500);

/// Whether the tests are asked for.
fn enabled() -> bool {
    std::env::var_os("NGR_TEST_VCAN").is_some_and(|value| value == "1")
}

/// Fails with what is missing, if `vcan0` is not there.
fn require_interface() {
    assert!(
        Path::new("/sys/class/net").join(INTERFACE).exists(),
        "NGR_TEST_VCAN=1 but {INTERFACE} does not exist: sudo modprobe vcan && \
         sudo ip link add dev {INTERFACE} type vcan && sudo ip link set up {INTERFACE}"
    );
}

/// The stand-in ECU as a child process, with the lines it writes to stderr.
struct Server {
    child: Child,
    stderr: Receiver<String>,
}

impl Server {
    /// Starts the binary on `vcan0` with `args` and waits until it serves.
    fn start(args: &[&str]) -> Self {
        require_interface();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ngr-stand-in-ecu"))
            .args(["--interface", INTERFACE])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the stand-in ECU should start");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        // A thread forwards the lines so that a test can wait for one with a bound. It ends when
        // the process closes stderr.
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            stderr: lines,
        };
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("the stand-in ECU should print its first line");
        // An empty line is the end of its output: it exited without serving, and its own message
        // (on stderr) says why: usually that the can-isotp module is not loaded.
        assert!(
            line.starts_with("ngr-stand-in-ecu: serving"),
            "the stand-in ECU did not start serving: {line:?}, status {:?}, stderr {:?}",
            server.child.try_wait(),
            server.stderr.try_iter().collect::<Vec<_>>()
        );
        server
    }

    /// The next line the process wrote to stderr, which must come within `RESPONSE_WAIT`.
    fn next_stderr_line(&self) -> String {
        self.stderr
            .recv_timeout(RESPONSE_WAIT)
            .expect("the stand-in ECU should write a line to stderr")
    }

    /// Ends the process with SIGTERM, as a user would, and checks it exited cleanly.
    fn stop(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill should run");
        assert!(status.success());
        let exit = self.child.wait().expect("the stand-in ECU should exit");
        assert!(exit.success(), "{exit:?}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // Ignored: the process may have exited in between, and the test has its result.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A client: sends on `request_id`, receives on `response_id` (raw SocketCAN identifiers).
struct Client(IsoTpSocket);

impl Client {
    fn open(request_id: u32, response_id: u32) -> Self {
        require_interface();
        Self(
            IsoTpSocket::open(INTERFACE, response_id, request_id)
                .expect("an ISO-TP socket on vcan0 should open (is can-isotp loaded?)"),
        )
    }

    fn send(&self, request: &[u8]) {
        self.0.send(request).expect("the request should be sent");
    }

    /// The next message, within `RESPONSE_WAIT`.
    fn receive(&self) -> Vec<u8> {
        self.0
            .recv(RESPONSE_WAIT)
            .expect("the receive should work")
            .expect("a message should arrive")
    }

    /// Checks that no message arrives within `SILENCE_WAIT`.
    fn expect_silence(&self, request: &[u8]) {
        let response = self.0.recv(SILENCE_WAIT).expect("the receive should work");
        assert_eq!(response, None, "an answer to {request:02X?}");
    }

    /// Sends `request` and returns the response.
    fn ask(&self, request: &[u8]) -> Vec<u8> {
        self.send(request);
        self.receive()
    }

    /// Sends `request` and checks that nothing comes back.
    fn ask_silence(&self, request: &[u8]) {
        self.send(request);
        self.expect_silence(request);
    }
}

fn write_json(dir: &Path, config: &EcuConfig) -> String {
    let file = dir.join("ecu.json");
    std::fs::write(&file, serde_json::to_string(config).unwrap()).unwrap();
    file.to_str().expect("a UTF-8 path").to_owned()
}

#[test]
fn a_did_is_read_with_physical_and_functional_addressing() {
    if !enabled() {
        return;
    }
    let mut server = Server::start(&[]);
    let mut expected = vec![0x62, 0xF1, 0x90];
    expected.extend_from_slice(b"NGRSIMECU00000001");

    // The 20-byte response is segmented: the kernel runs the flow control on both sides.
    let physical = Client::open(0x7E0, 0x7E8);
    assert_eq!(physical.ask(&[0x22, 0xF1, 0x90]), expected);
    // A functional request comes from a socket that sends on the functional ID and listens on an
    // ID nothing uses; the segmented answer goes to the physical client, which gives the flow
    // control to the request ID.
    let functional = Client::open(0x7DF, 0x7DE);
    functional.send(&[0x22, 0xF1, 0x90]);
    assert_eq!(physical.receive(), expected);
    // A functional request the ECU has no answer to gets none.
    functional.send(&[0x22, 0x00, 0x01]);
    physical.expect_silence(&[0x22, 0x00, 0x01]);
    server.stop();
}

#[test]
fn a_did_is_read_with_29_bit_ids_and_ids_from_the_command_line() {
    if !enabled() {
        return;
    }
    let mut server = Server::start(&[
        "--extended",
        "--request-id",
        "0x18DA00F1",
        "--response-id",
        "0x18DAF100",
        "--functional-id",
        "0x18DB33F1",
    ]);
    let extended = |id| ngr_stand_in_ecu::args::socket_id(id, true).unwrap();
    let client = Client::open(extended(0x18DA_00F1), extended(0x18DA_F100));
    let response = client.ask(&[0x22, 0xF1, 0x90]);
    assert_eq!(&response[..3], [0x62, 0xF1, 0x90]);
    server.stop();
}

#[test]
fn an_interrupted_write_is_kept_across_a_restart() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = write_json(
        dir.path(),
        &EcuConfig {
            drop_at_block: Some(2),
            ..default_ecu_config()
        },
    );
    let state = dir.path().join("ecu.state");
    let state = state.to_str().expect("a UTF-8 path");
    let ids = [
        "--request-id",
        "0x6E0",
        "--response-id",
        "0x6E8",
        "--functional-id",
        "0x6DF",
    ];

    let mut args = ids.to_vec();
    args.extend(["--config", &config, "--state", state]);
    let mut server = Server::start(&args);
    let client = Client::open(0x6E0, 0x6E8);
    assert_eq!(client.ask(&[0x10, 0x02])[0], 0x50);
    assert_eq!(client.ask(&[0x31, 0x01, 0xFF, 0x00])[0], 0x71);
    assert_eq!(
        client.ask(&[0x34, 0x00, 0x44, 0, 0, 0, 0x40, 0, 0, 0, 16])[0],
        0x74
    );
    // A block of 8 bytes is a segmented request: the kernel answers its first frame with flow
    // control.
    assert_eq!(
        client.ask(&[0x36, 0x01, 1, 2, 3, 4, 5, 6, 7, 8]),
        [0x76, 0x01]
    );
    // The second block is lost and the ECU goes quiet.
    client.ask_silence(&[0x36, 0x02, 9, 10, 11, 12, 13, 14, 15, 16]);
    server.stop();

    // A new process continues from the state file, without the configuration that would drop
    // block 2 again, and finds the transfer where it stopped.
    let mut args = ids.to_vec();
    args.extend(["--state", state]);
    let mut server = Server::start(&args);
    // The flash-state record: phase 5 (interrupted), last block 1, 8 bytes received.
    assert_eq!(
        client.ask(&[0x22, 0xFD, 0x00]),
        [0x62, 0xFD, 0x00, 0x05, 0, 0, 0, 1, 0, 0, 0, 8]
    );
    // The rest of the image can be sent.
    assert_eq!(client.ask(&[0x10, 0x02])[0], 0x50);
    assert_eq!(
        client.ask(&[0x34, 0x00, 0x44, 0, 0, 0, 0x48, 0, 0, 0, 8])[0],
        0x74
    );
    assert_eq!(
        client.ask(&[0x36, 0x01, 9, 10, 11, 12, 13, 14, 15, 16]),
        [0x76, 0x01]
    );
    assert_eq!(client.ask(&[0x37]), [0x77]);
    server.stop();
}

#[test]
fn a_failed_transfer_does_not_end_the_server() {
    if !enabled() {
        return;
    }
    let mut server = Server::start(&[
        "--request-id",
        "0x6A0",
        "--response-id",
        "0x6A8",
        "--functional-id",
        "0x69F",
    ]);
    // This client listens on an ID the response does not use, so nobody gives flow control for
    // the segmented response. The kernel ends the send when its 1 s timer for the flow control
    // runs out and reports it through the socket; the server logs it. The wait is the event: the
    // log line.
    let deaf = Client::open(0x6A0, 0x6B0);
    deaf.send(&[0x22, 0xF1, 0x90]);
    let line = server.next_stderr_line();
    assert!(line.contains("failed"), "{line}");
    // The server still answers, and the answer is whole.
    let client = Client::open(0x6A0, 0x6A8);
    let response = client.ask(&[0x22, 0xF1, 0x90]);
    assert_eq!(&response[..3], [0x62, 0xF1, 0x90]);
    assert_eq!(response.len(), 20);
    server.stop();
}

#[test]
fn a_missing_interface_is_reported_and_ends_the_process_before_the_state_file_is_touched() {
    // Needs no vcan0: the ISO-TP socket cannot be opened on an interface that is not there.
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let output = Command::new(env!("CARGO_BIN_EXE_ngr-stand-in-ecu"))
        .args(["--interface", "ngrnone0", "--state"])
        .arg(&state)
        .output()
        .expect("the stand-in ECU should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ngrnone0"), "{stderr}");
    assert!(!state.exists(), "the state file was created");
}
