use super::*;
use crate::profile::parse;

const VIN: &str = "WVWZZZ1JZ3W386752";

fn profile_text(api: &str, extra: &str) -> String {
    let names = if api == "j2534-0404" {
        "label = \"bench\"\npname = \"Vendor VCI 1\""
    } else {
        ""
    };
    format!(
        r#"
class = "T3"
{extra}

[vci]
api = "{api}"
library = "Vendor VCI"
module = "SN-1234567890"
{names}
firmware_version = "1.2.3"
library_version = "4.5.6"

[link]
protocol = "ISO15765"
baud_rate = 500000
pins = [6, 14]
request_id = 0x7E0
response_id = 0x7E8
id_bits = 11
addressing = "normal"
"#
    )
}

fn profile(api: &str, extra: &str) -> Profile {
    parse(&profile_text(api, extra)).unwrap()
}

fn vci(profile: &Profile) -> &Vci {
    match &profile.setup {
        Setup::Link { vci, .. } => vci,
        Setup::Loopback(_) => unreachable!(),
    }
}

fn version() -> ReportedVersion {
    ReportedVersion {
        hw_name: "fw 1.2.3".into(),
        fw_name: "dll 4.5.6".into(),
        fw_version: 0x0201_0000,
        pdu_api_sw_version: 0x0300_0100,
    }
}

fn module(name: &str) -> ReportedModule {
    ReportedModule {
        name: name.into(),
        additional_info: String::new(),
    }
}

#[test]
fn a_vin_in_free_text_is_masked() {
    let masker = Masker::for_profile(&profile("j2534-0404", ""));
    let masked = masker.text(&format!("read {VIN}, then (VIN={VIN})"));
    assert!(!masked.contains(VIN), "{masked}");
    assert_eq!(masked, "read ****6752, then (VIN=****6752)");
    // Not VIN-shaped: 16 characters, 17 letters only.
    assert_eq!(masker.text("WVWZZZ1JZ3W38675"), "WVWZZZ1JZ3W38675");
    assert_eq!(masker.text("ABCDEFGHJKLMNPRST"), "ABCDEFGHJKLMNPRST");
}

#[test]
fn the_profile_can_ask_for_full_identifiers() {
    let profile = profile("iso22900", "report_full_identifiers = true");
    assert!(profile.report_full_identifiers);
    let mut report = Report::new(&profile);
    report.record(
        "hw_x",
        Outcome::Failed,
        &format!("VIN {VIN} on SN-1234567890"),
    );
    report.set_device(
        vci(&profile),
        Abi::LinuxX86_64,
        &module("Serial='SN-1234567890'"),
        &version(),
    );
    let text = report.to_toml().unwrap();
    assert!(
        text.contains(VIN) && text.contains("SN-1234567890"),
        "{text}"
    );
    assert!(text.contains("identifiers_in_full = true"));
    assert!(
        !parse(&profile_text("iso22900", ""))
            .unwrap()
            .report_full_identifiers
    );
}

#[test]
fn a_serial_number_is_masked_in_the_module_name() {
    let profile = profile("iso22900", "");
    let masker = Masker::for_profile(&profile);
    assert_eq!(
        masker.module_name(
            Api::Iso22900,
            "Name='Vendor Box' SerialNumber='1234567890' Fw='2'"
        ),
        "Name='Vendor Box' SerialNumber='****7890' Fw='2'"
    );
    // Plain text is a device name or a serial number: masked whole, short ones fully.
    assert_eq!(masker.module_name(Api::Iso22900, "ABC123"), "****");
    assert_eq!(
        masker.module_name(Api::Iso22900, "ABCDEF123456"),
        "****3456"
    );
    // A J2534 label is not a serial number.
    assert_eq!(masker.module_name(Api::J2534V0404, "bench"), "bench");
    // The profile's own D-PDU `module` is a serial number wherever it appears.
    assert_eq!(
        masker.text("module SN-1234567890 absent"),
        "module ****7890 absent"
    );
}

#[test]
fn the_report_has_every_field_and_no_identifier() {
    let profile = profile("iso22900", "");
    let mut report = Report::new(&profile);
    report.set_device(
        vci(&profile),
        Abi::LinuxArm64,
        &module("SerialNumber='1234567890'"),
        &version(),
    );
    report.record("hw_a", Outcome::Passed, "");
    report.record("hw_b", Outcome::Failed, &format!("VIN {VIN} differs"));
    report.record_refusal("hw_c", &Refusal::Operation(crate::Operation::Reprogramming));
    let text = report.to_toml().unwrap();
    let table: toml::Table = text.parse().unwrap();
    let started = table["started_at"].as_str().unwrap();
    assert_eq!(started.len(), 20, "{started}");
    assert!(started.ends_with('Z') && started.as_bytes()[10] == b'T');
    assert!(!table["host"].as_str().unwrap().is_empty());
    assert!(table["os"].as_str().unwrap().contains(std::env::consts::OS));
    assert_eq!(table["class"].as_str(), Some("T3"));
    let device = table["device"].as_table().unwrap();
    assert_eq!(device["abi"].as_str(), Some("linux-arm64"));
    assert_eq!(device["vendor"].as_str(), Some("Vendor VCI"));
    assert_eq!(device["model"].as_str(), Some("SerialNumber='****7890'"));
    assert_eq!(device["firmware_version"].as_str(), Some("2.1.0"));
    assert_eq!(device["library_version"].as_str(), Some("3.0.1"));
    let checks = table["check"].as_array().unwrap();
    let outcomes: Vec<_> = checks
        .iter()
        .map(|c| c["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["passed", "failed", "refused"]);
    assert!(
        !text.contains(VIN) && !text.contains("1234567890"),
        "{text}"
    );
}

#[test]
fn j2534_versions_are_the_reported_strings_and_no_device_leaves_them_out() {
    let profile = profile("j2534-0404", "");
    let mut report = Report::new(&profile);
    assert!(!report.to_toml().unwrap().contains("[device]"));
    report.set_device(vci(&profile), Abi::WinX86, &module("bench"), &version());
    let device = report.device.unwrap();
    assert_eq!(device.firmware_version, "fw 1.2.3");
    assert_eq!(device.library_version, "dll 4.5.6");
    assert_eq!(device.model, "bench");
}

#[test]
fn the_time_is_utc_rfc_3339() {
    let at = |secs| rfc3339(UNIX_EPOCH + std::time::Duration::from_secs(secs));
    assert_eq!(at(0), "1970-01-01T00:00:00Z");
    assert_eq!(at(951_782_400 + 86_399), "2000-02-29T23:59:59Z");
    assert_eq!(at(1_781_000_000), "2026-06-09T10:13:20Z");
}

#[test]
fn the_data_directory_follows_the_platform() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    };
    if cfg!(unix) && !cfg!(target_os = "macos") {
        let xdg = env(&[("XDG_DATA_HOME", "/x/data"), ("HOME", "/home/u")]);
        assert_eq!(data_dir(xdg), Some(PathBuf::from("/x/data")));
        let home = env(&[("HOME", "/home/u")]);
        assert_eq!(data_dir(home), Some(PathBuf::from("/home/u/.local/share")));
        let relative = env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/home/u")]);
        assert_eq!(
            data_dir(relative),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_dir(env(&[])), None);
    }
}

#[cfg(unix)]
#[test]
fn the_directory_and_file_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("nekoguruma").join("hw-reports");
    let report = Report::new(&profile("j2534-0404", ""));
    let first = report.write_to(&dir).unwrap();
    let second = report.write_to(&dir).unwrap();
    assert_ne!(
        first, second,
        "a second report does not overwrite the first"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(dir.parent().unwrap()), 0o700);
    assert_eq!(mode(&first), 0o600);
    assert_eq!(mode(&second), 0o600);
    assert!(
        std::fs::read_to_string(&first)
            .unwrap()
            .contains("class = \"T3\"")
    );
}

#[cfg(unix)]
#[test]
fn an_existing_directory_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = Report::new(&profile("j2534-0404", ""))
        .write_to(&dir)
        .unwrap_err();
    assert!(
        matches!(error, ReportError::TooOpen { mode: 0o755, .. }),
        "{error}"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn a_directory_inside_this_repository_is_refused() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = crate_dir
        .join("target")
        .join("report-guard-test")
        .join("reports");
    let error = Report::new(&profile("j2534-0404", ""))
        .write_to(&dir)
        .unwrap_err();
    assert!(matches!(error, ReportError::InsideRepository(_)), "{error}");
    assert!(!dir.exists());
}

#[test]
fn a_directory_inside_another_work_tree_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let dir = root.path().join("reports");
    let path = Report::new(&profile("j2534-0404", ""))
        .write_to(&dir)
        .unwrap();
    assert!(path.starts_with(&dir));
}

#[test]
fn a_vin_shaped_host_name_is_masked_unless_asked_for() {
    let masked = Report::with_host(&profile("j2534-0404", ""), VIN, Some(&format!("OS {VIN}")));
    assert_eq!(masked.host, "****6752");
    assert_eq!(masked.os_version.as_deref(), Some("OS ****6752"));
    let full = Report::with_host(
        &profile("j2534-0404", "report_full_identifiers = true"),
        VIN,
        None,
    );
    assert_eq!(full.host, VIN);
}

#[test]
fn a_panicking_check_still_leaves_a_failed_report() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    let profile = profile("j2534-0404", "");
    let result = std::panic::catch_unwind(|| {
        let _run = RunGuard::start_in(&profile, "hw_x", dir.clone());
        panic!("assertion failed");
    });
    assert!(result.is_err());
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(files.len(), 1);
    let table: toml::Table = std::fs::read_to_string(files[0].as_ref().unwrap().path())
        .unwrap()
        .parse()
        .unwrap();
    let check = &table["check"].as_array().unwrap()[0];
    assert_eq!(check["name"].as_str(), Some("hw_x"));
    assert_eq!(check["outcome"].as_str(), Some("failed"));
    assert!(check["message"].as_str().unwrap().contains("panicked"));
}

#[test]
fn a_finished_check_keeps_its_recorded_result() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    {
        let mut run = RunGuard::start_in(&profile("j2534-0404", ""), "hw_y", dir.clone());
        run.pass();
    }
    let path = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("outcome = \"passed\""), "{text}");
}

#[cfg(unix)]
#[test]
fn a_path_that_leads_into_the_repository_is_refused() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let report = Report::new(&profile("j2534-0404", ""));
    // `..` after an existing directory resolves into the repository.
    let dotdot = crate_dir
        .join("docs")
        .join("..")
        .join("report-guard-dotdot");
    assert!(matches!(
        report.write_to(&dotdot),
        Err(ReportError::InsideRepository(_))
    ));
    // `..` below a directory that does not exist is refused outright.
    let missing = crate_dir
        .join("no-such-dir")
        .join("..")
        .join("report-guard-dotdot");
    assert!(report.write_to(&missing).is_err());
    // A symlink outside the repository that points into it.
    let root = tempfile::tempdir().unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(crate_dir.join("docs"), &link).unwrap();
    let through = link.join("report-guard-symlink");
    assert!(matches!(
        report.write_to(&through),
        Err(ReportError::InsideRepository(_))
    ));
    for name in ["report-guard-dotdot", "report-guard-symlink"] {
        assert!(!crate_dir.join(name).exists());
        assert!(!crate_dir.join("docs").join(name).exists());
    }
}

#[test]
fn no_text_field_of_a_report_carries_a_vin() {
    let text = profile_text("j2534-0404", "").replace("Vendor VCI\"", &format!("Vendor {VIN}\""));
    let profile = parse(&text).unwrap();
    let mut report = Report::with_host(&profile, VIN, Some(VIN));
    let version = ReportedVersion {
        hw_name: format!("fw {VIN}"),
        fw_name: format!("dll {VIN}"),
        ..version()
    };
    report.set_device(vci(&profile), Abi::WinX86, &module(VIN), &version);
    report.record("hw_x", Outcome::Failed, &format!("read {VIN}"));
    let toml = report.to_toml().unwrap();
    assert!(!toml.contains(VIN), "{toml}");
}
