use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::SystemTime;

use super::*;
use crate::{DidConfig, DidWrite, Fault, Session, SimResponse};

/// A state file path in a fresh temporary directory, removed when dropped.
struct StateDir(tempfile::TempDir);

impl StateDir {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("temporary directory should be writable"))
    }

    fn file(&self) -> PathBuf {
        self.0.path().join("ecu.state")
    }
}

#[test]
fn the_state_file_keeps_configured_dids_and_the_key_mask() {
    let dir = StateDir::new();
    let mut ecu = SimEcu::new(EcuConfig {
        security_key_mask: Some(0x3C3C_3C3C),
        dids: vec![DidConfig {
            did: 0x1201,
            value: vec![0, 0, 0],
            write: Some(DidWrite {
                session: Some(Session::Extended),
                security_level: None,
            }),
        }],
        ..default_ecu_config()
    });
    ecu.request(&[0x10, 0x03]);
    assert_eq!(
        ecu.request(&[0x2E, 0x12, 0x01, 7, 8, 9]),
        SimResponse::Positive(vec![0x6E, 0x12, 0x01])
    );
    save_ecu_state(&dir.file(), &mut ecu).expect("the state should be written");
    let mut loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(loaded.config.security_key_mask, Some(0x3C3C_3C3C));
    assert_eq!(loaded.key_for_seed(1), 1 ^ 0x3C3C_3C3C);
    assert_eq!(
        loaded.request(&[0x22, 0x12, 0x01]),
        SimResponse::Positive(vec![0x62, 0x12, 0x01, 7, 8, 9])
    );
}

#[test]
fn the_state_file_round_trips_the_ecu() {
    let dir = StateDir::new();
    let mut ecu = SimEcu::new(default_ecu_config());
    assert!(matches!(
        ecu.request(&[0x10, 0x03]),
        SimResponse::Positive(_)
    ));
    save_ecu_state(&dir.file(), &mut ecu).expect("the state should be written");
    assert!(!dir.0.path().join("ecu.state.tmp").exists());

    let loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(loaded.session, Session::Extended);
    assert_eq!(loaded.config.vin, "NGRSIMECU00000001");
}

#[test]
fn the_state_file_keeps_the_count_of_a_fault_for_the_nth_request() {
    let dir = StateDir::new();
    let read = [0x22, 0xF1, 0x86];
    let mut ecu = SimEcu::new(default_ecu_config());
    ecu.inject_for_nth_request(
        &read,
        NonZeroU32::new(2).expect("2 is not zero"),
        Fault::DropResponse,
    );
    assert!(matches!(ecu.request(&read), SimResponse::Positive(_)));
    save_ecu_state(&dir.file(), &mut ecu).expect("the state should be written");
    let mut loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(
        loaded
            .faults_for_request()
            .iter()
            .map(|waiting| waiting.matches_left.get())
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(loaded.request(&read), SimResponse::NoResponse);
    assert!(matches!(loaded.request(&read), SimResponse::Positive(_)));
}

#[test]
fn a_damaged_or_foreign_state_file_is_refused() {
    let dir = StateDir::new();
    assert!(
        matches!(load_ecu_state(&dir.file()), Ok(None)),
        "no file yet"
    );
    // A directory where the file should be cannot be read, which is not the same as no file.
    std::fs::create_dir(dir.file()).expect("directory should be created");
    load_ecu_state(&dir.file()).unwrap_err();
    std::fs::remove_dir(dir.file()).expect("directory should be removed");
    std::fs::write(dir.file(), b"not a state file").expect("file should be writable");
    load_ecu_state(&dir.file()).unwrap_err();

    let mut ecu = SimEcu::new(default_ecu_config());
    let other_version = StateFile {
        version: STATE_FILE_VERSION + 1,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    std::fs::write(
        dir.file(),
        postcard::to_allocvec(&other_version).expect("state should encode"),
    )
    .expect("file should be writable");
    load_ecu_state(&dir.file()).unwrap_err();

    // A file of the previous version is refused on its version, whatever follows it, before the
    // rest is decoded in this version's layout.
    let mut previous =
        postcard::to_allocvec(&(STATE_FILE_VERSION - 1)).expect("version should encode");
    previous.extend(postcard::to_allocvec(&other_version.ecu).expect("state should encode"));
    std::fs::write(dir.file(), previous).expect("file should be writable");
    load_ecu_state(&dir.file()).unwrap_err();
}

#[test]
fn time_since_the_write_counts_against_the_session() {
    let dir = StateDir::new();
    let mut ecu = SimEcu::new(EcuConfig {
        s3_server_ms: Some(1_000),
        ..default_ecu_config()
    });
    assert!(matches!(
        ecu.request(&[0x10, 0x03]),
        SimResponse::Positive(_)
    ));
    let mut state = StateFile {
        version: STATE_FILE_VERSION,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    // Written two seconds ago: tS3_Server ran out in the meantime.
    state.saved_at_ms -= 2_000;
    std::fs::write(
        dir.file(),
        postcard::to_allocvec(&state).expect("state should encode"),
    )
    .expect("file should be writable");
    let loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(loaded.session, Session::Default);
}

#[test]
fn a_configuration_file_is_read_and_checked() {
    let dir = StateDir::new();
    let file = dir.0.path().join("ecu.json");
    std::fs::write(
        &file,
        r#"{"vin": "NGRSIMECU00000001", "part_number": "P", "sw_version": "S", "response_delay_ms": 7,
            "drop_at_block": 3, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false}"#,
    )
    .expect("file should be writable");
    let config = read_ecu_config(&file).expect("the configuration should read");
    assert_eq!(
        (config.response_delay_ms, config.drop_at_block),
        (7, Some(3))
    );

    // The error names the file and says why it failed.
    let missing = read_ecu_config(&dir.0.path().join("none.json")).unwrap_err();
    assert!(missing.contains("none.json") && missing.contains("unreadable"));
    std::fs::write(&file, "{").expect("file should be writable");
    assert!(
        read_ecu_config(&file)
            .unwrap_err()
            .contains("does not parse")
    );
    std::fs::write(
        &file,
        serde_json::to_string(&EcuConfig {
            vin: "too short".into(),
            ..default_ecu_config()
        })
        .expect("configuration should encode"),
    )
    .expect("file should be writable");
    assert!(read_ecu_config(&file).unwrap_err().contains("invalid"));
}
