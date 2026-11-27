use std::collections::VecDeque;

use sim_ecu::persist::load_ecu_state;
use sim_ecu::{EcuConfig, Session};

use super::*;

/// Requests the station is given in order; when they run out the transport sets the stop flag.
/// `sent` holds what the station sent, and `saved_at_send` the session the state file held
/// when each message went out.
struct Fake<'a> {
    requests: VecDeque<(Addressing, Vec<u8>)>,
    sent: Vec<(Addressing, Vec<u8>)>,
    saved_at_send: Vec<Option<Session>>,
    state: Option<PathBuf>,
    stop: &'a AtomicBool,
}

impl<'a> Fake<'a> {
    fn new(stop: &'a AtomicBool, state: Option<&Path>, requests: &[(Addressing, &[u8])]) -> Self {
        Self {
            requests: requests.iter().map(|(a, r)| (*a, r.to_vec())).collect(),
            sent: Vec::new(),
            saved_at_send: Vec::new(),
            state: state.map(Path::to_path_buf),
            stop,
        }
    }
}

impl Transport for Fake<'_> {
    fn recv(&mut self, _timeout: Duration) -> io::Result<Option<(Addressing, Vec<u8>)>> {
        let next = self.requests.pop_front();
        if next.is_none() {
            self.stop.store(true, Ordering::Relaxed);
        }
        Ok(next)
    }

    fn send(&mut self, addressing: Addressing, message: &[u8]) -> io::Result<()> {
        self.sent.push((addressing, message.to_vec()));
        let session = self
            .state
            .as_deref()
            .map(|path| load_ecu_state(path).unwrap().unwrap().session);
        self.saved_at_send.push(session);
        Ok(())
    }
}

/// Runs `requests` through a station started with `config` and `state`; returns what was sent.
fn serve(
    config: Option<&Path>,
    state: Option<&Path>,
    requests: &[(Addressing, &[u8])],
) -> Vec<(Addressing, Vec<u8>)> {
    let stop = AtomicBool::new(false);
    let mut station = Station::start(config, state).expect("the station should start");
    let mut fake = Fake::new(&stop, state, requests);
    station.run(&mut fake, &stop).expect("the run should end");
    fake.sent
}

fn write_config(dir: &Path, config: &EcuConfig) -> PathBuf {
    let file = dir.join("ecu.json");
    std::fs::write(&file, serde_json::to_string(config).unwrap()).unwrap();
    file
}

const READ_VIN: [u8; 3] = [0x22, 0xF1, 0x90];
const PHYSICAL: Addressing = Addressing::Physical;
const FUNCTIONAL: Addressing = Addressing::Functional;

#[test]
fn a_physical_read_is_answered_with_the_configured_vin() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        &EcuConfig {
            vin: "WVWZZZ1KZAW000001".into(),
            ..default_ecu_config()
        },
    );
    let sent = serve(Some(&config), None, &[(PHYSICAL, &READ_VIN)]);
    let mut expected = vec![0x62, 0xF1, 0x90];
    expected.extend_from_slice(b"WVWZZZ1KZAW000001");
    assert_eq!(sent, [(PHYSICAL, expected)]);
}

#[test]
fn a_response_goes_out_on_the_addressing_of_its_request() {
    let sent = serve(
        None,
        None,
        &[(FUNCTIONAL, &READ_VIN), (PHYSICAL, &[0x22, 0x00, 0x01])],
    );
    let addressings: Vec<_> = sent.iter().map(|(a, _)| *a).collect();
    assert_eq!(addressings, [FUNCTIONAL, PHYSICAL]);
    // A functional request the ECU has no answer to gets none; the same physical one is refused.
    assert_eq!(sent[1].1, [0x7F, 0x22, 0x31]);
    let sent = serve(None, None, &[(FUNCTIONAL, &[0x22, 0x00, 0x01])]);
    assert!(sent.is_empty(), "{sent:?}");
}

#[test]
fn a_request_that_suppresses_its_positive_response_gets_none() {
    // TesterPresent with the suppress bit (ISO 14229-1 clause 8.2.2 and 7.8.1).
    let sent = serve(None, None, &[(PHYSICAL, &[0x3E, 0x80])]);
    assert!(sent.is_empty(), "{sent:?}");
}

#[test]
fn a_response_waits_for_the_delay_the_ecu_gives() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        &EcuConfig {
            response_delay_ms: 120,
            ..default_ecu_config()
        },
    );
    let started = Instant::now();
    let sent = serve(Some(&config), None, &[(PHYSICAL, &READ_VIN)]);
    assert_eq!(sent.len(), 1);
    assert!(started.elapsed() >= Duration::from_millis(120));
}

#[test]
fn the_state_is_written_before_the_response_goes_out() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let stop = AtomicBool::new(false);
    let mut station = Station::start(None, Some(&state)).unwrap();
    let mut fake = Fake::new(&stop, Some(&state), &[(PHYSICAL, &[0x10, 0x03])]);
    station.run(&mut fake, &stop).unwrap();
    // The file already held the extended session when the response left.
    assert_eq!(fake.saved_at_send, [Some(Session::Extended)]);
}

#[test]
fn the_state_is_written_when_the_run_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let stop = AtomicBool::new(false);
    let mut station = Station::start(None, Some(&state)).unwrap();
    std::fs::remove_file(&state).unwrap();
    let mut fake = Fake::new(&stop, None, &[]);
    station.run(&mut fake, &stop).unwrap();
    assert!(load_ecu_state(&state).unwrap().is_some());
}

#[test]
fn a_state_file_that_cannot_be_written_stops_the_run_before_the_response() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let stop = AtomicBool::new(false);
    let mut station = Station::start(None, Some(&state)).unwrap();
    // A directory in the way of the rename: the write fails, as a full disk would.
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut fake = Fake::new(&stop, None, &[(PHYSICAL, &READ_VIN)]);
    let error = station.run(&mut fake, &stop).unwrap_err();
    assert!(matches!(error, ServeError::Save { .. }), "{error}");
    assert!(fake.sent.is_empty());
}

#[test]
fn an_interrupted_flash_write_is_kept_across_a_restart_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let config = write_config(
        dir.path(),
        &EcuConfig {
            drop_at_block: Some(2),
            ..default_ecu_config()
        },
    );
    let sent = serve(
        Some(&config),
        Some(&state),
        &[
            (PHYSICAL, &[0x10, 0x02]),
            (PHYSICAL, &[0x31, 0x01, 0xFF, 0x00]),
            (PHYSICAL, &[0x34, 0x00, 0x44, 0, 0, 0, 0x40, 0, 0, 0, 16]),
            (PHYSICAL, &[0x36, 0x01, 1, 2, 3, 4, 5, 6, 7, 8]),
            // The second block is lost and the ECU goes quiet, so nothing answers these.
            (PHYSICAL, &[0x36, 0x02, 9, 10, 11, 12, 13, 14, 15, 16]),
            (PHYSICAL, &[0x3E, 0x00]),
        ],
    );
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[3].1, [0x76, 0x01]);

    // The next process takes the ECU from the file (the configuration, which would drop block 2
    // again, is not read), powers it up and finds the transfer where it stopped; the rest of the
    // image can be sent.
    let sent = serve(
        Some(&config),
        Some(&state),
        &[
            (PHYSICAL, &[0x22, 0xFD, 0x00]),
            (PHYSICAL, &[0x10, 0x02]),
            (PHYSICAL, &[0x34, 0x00, 0x44, 0, 0, 0, 0x48, 0, 0, 0, 8]),
            (PHYSICAL, &[0x36, 0x01, 9, 10, 11, 12, 13, 14, 15, 16]),
            (PHYSICAL, &[0x37]),
        ],
    );
    let responses: Vec<_> = sent.iter().map(|(_, m)| m.as_slice()).collect();
    // The flash-state record: phase 5 (interrupted), last block 1, 8 bytes received.
    assert_eq!(
        responses[0],
        [0x62, 0xFD, 0x00, 0x05, 0, 0, 0, 1, 0, 0, 0, 8]
    );
    assert_eq!(responses[2][0], 0x74);
    assert_eq!(&responses[3..], [&[0x76, 0x01][..], &[0x77][..]]);
}

#[test]
fn the_state_file_wins_over_the_configuration_named_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("ecu.state");
    let other = write_config(
        dir.path(),
        &EcuConfig {
            vin: "OTHERVIN000000001".into(),
            ..default_ecu_config()
        },
    );
    serve(None, Some(&state), &[]);
    // The state file wins over the configuration named with it.
    let sent = serve(Some(&other), Some(&state), &[(PHYSICAL, &READ_VIN)]);
    assert!(sent[0].1.ends_with(b"NGRSIMECU00000001"), "{sent:?}");
}

#[test]
fn a_bad_configuration_or_state_file_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none.json");
    let error = Station::start(Some(&missing), None).unwrap_err();
    assert!(
        matches!(&error, StartError::Config(m) if m.contains("none.json")),
        "{error}"
    );

    let state = dir.path().join("ecu.state");
    std::fs::write(&state, b"not a state file").unwrap();
    let error = Station::start(None, Some(&state)).unwrap_err();
    assert!(matches!(error, StartError::State { .. }), "{error}");
    assert!(error.to_string().contains("ecu.state"), "{error}");
}
