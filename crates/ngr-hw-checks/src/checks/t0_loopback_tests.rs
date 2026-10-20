use std::path::PathBuf;

use super::*;

/// The test executable is a library file of the host's ABI, as far as the check can tell.
fn host() -> DeviceFacts {
    let exe = std::env::current_exe().unwrap();
    let abi = detect_file(&exe).expect("the test executable has a known ABI");
    DeviceFacts {
        library: exe,
        abi,
        long_size: abi.default_long_size(),
        declared_long_size: None,
    }
}

#[test]
fn the_abi_and_the_default_width_of_the_library_pass() {
    check_abi(&host()).unwrap();
}

#[test]
fn a_declared_width_replaces_the_default() {
    let mut facts = host();
    let other = if facts.abi.default_long_size() == 8 {
        4
    } else {
        8
    };
    // The worker ran with the other width, which the definition does not declare: wrong.
    facts.long_size = other;
    assert!(matches!(check_abi(&facts), Err(CheckError::Abi(_))));
    // Declared, it is the right one (a 4 on a 64-bit ABI, which the launch accepts).
    if other == 4 {
        facts.declared_long_size = Some(4);
        check_abi(&facts).unwrap();
        // And the default is then the wrong one.
        facts.long_size = 8;
        assert!(matches!(check_abi(&facts), Err(CheckError::Abi(_))));
    }
}

#[test]
fn a_worker_of_another_abi_than_the_library_fails() {
    let mut facts = host();
    facts.abi = [Abi::LinuxX86_64, Abi::LinuxArm64, Abi::LinuxX86]
        .into_iter()
        .find(|abi| *abi != facts.abi)
        .unwrap();
    facts.long_size = facts.abi.default_long_size();
    let error = check_abi(&facts).unwrap_err().to_string();
    assert!(error.contains("is built for"), "{error}");
}

#[test]
fn a_file_that_is_not_a_library_fails() {
    let facts = DeviceFacts {
        library: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        ..host()
    };
    assert!(matches!(check_abi(&facts), Err(CheckError::Abi(_))));
    let missing = DeviceFacts {
        library: PathBuf::from("/nonexistent/library.so"),
        ..host()
    };
    assert!(matches!(check_abi(&missing), Err(CheckError::Abi(_))));
}

#[test]
fn payloads_tell_the_directions_and_positions_apart() {
    assert_eq!(payload(0, 3), [0xA0, 0xA1, 0xA2]);
    assert_eq!(payload(1, 2), [0xB0, 0xB1]);
}

#[test]
fn a_refusal_is_refused_and_every_other_failure_failed() {
    assert_eq!(
        CheckError::Refused(String::new()).outcome(),
        Outcome::Refused
    );
    assert_eq!(CheckError::Frame(String::new()).outcome(), Outcome::Failed);
    assert_eq!(CheckError::Abi(String::new()).outcome(), Outcome::Failed);
    assert_eq!(CheckError::Send(String::new()).outcome(), Outcome::Failed);
    assert_eq!(
        CheckError::Monitor(String::new()).outcome(),
        Outcome::Failed
    );
}
