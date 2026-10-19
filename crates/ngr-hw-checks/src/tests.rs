use super::*;
use crate::profile::{Setup, parse};

/// A T1 profile on a J2534 VCI; `extra` is appended.
fn t1(extra: &str) -> String {
    profile("T1", extra)
}

fn profile(class: &str, extra: &str) -> String {
    format!(
        r#"
class = "{class}"
{extra}

[vci]
api = "j2534-0404"
library = "Vendor VCI"
module = "VCI-1"
label = "bench"
pname = "Vendor VCI 1"
firmware_version = "1.2.3"
library_version = "4.5"

[link]
protocol = "ISO15765"
baud_rate = 500000
pins = [6, 14]
request_id = 0x7E0
response_id = 0x7E8
functional_id = 0x7DF
id_bits = 11
addressing = "normal"
padding = 0xCC

[link.comparams]
CP_BlockSize = 0
CP_STmin = 0
"#
    )
}

const T0_PROFILE: &str = r#"
class = "T0"

[[endpoint]]
socketcan = "can0"
[endpoint.link]
protocol = "CAN"
baud_rate = 500000
pins = [6, 14]
request_id = 0x123
response_id = 0x124
id_bits = 11
addressing = "normal"

[[endpoint]]
[endpoint.vci]
api = "iso22900"
library = "Vendor PDU"
module = "SN-0001"
firmware_version = "2.0.0"
library_version = "1.0.0"
[endpoint.link]
protocol = "CAN"
baud_rate = 500000
pins = [6, 14]
request_id = 0x124
response_id = 0x123
id_bits = 11
addressing = "normal"
"#;

#[test]
fn a_profile_names_its_vci_and_complete_link() {
    let profile = parse(&t1("")).unwrap();
    assert_eq!(profile.class, Class::T1);
    let Setup::Link { vci, link } = &profile.setup else {
        panic!("{profile:?}");
    };
    assert_eq!(vci.label.as_deref(), Some("bench"));
    assert_eq!(link.request_id, 0x7E0);
    assert_eq!(link.padding, Some(0xCC));
    assert_eq!(link.comparams["CP_BlockSize"], 0);
    // Without `allow`, the class's operations.
    assert_eq!(profile.operations, Class::T1.operations());

    let profile = parse(T0_PROFILE).unwrap();
    assert!(matches!(profile.setup, Setup::Loopback(_)));
    assert!(profile.operations.is_empty());
}

#[test]
fn a_profile_can_only_narrow_its_class() {
    // T1 narrowed to reads and routines.
    let narrowed = parse(&t1(r#"allow = ["routine"]"#)).unwrap();
    assert_eq!(narrowed.operations, [Operation::Routine]);
    assert!(!narrowed.allows(Operation::Reprogramming));

    // A T3 profile can add nothing (design 13.5).
    for extra in [
        r#"allow = ["configuration-write"]"#,
        r#"allow = ["reprogramming"]"#,
        "expendable_ecu = true",
        "[power]\noff = [\"relay\", \"off\"]\non = [\"relay\", \"on\"]",
        "[[hardware-control]]\nkind = \"pin-switch\"\nname = \"k-line\"\npins = [7]",
    ] {
        let refused = parse(&profile("T3", extra));
        assert!(
            matches!(
                refused,
                Err(ProfileError::Adds {
                    class: Class::T3,
                    ..
                })
            ),
            "{extra}: {refused:?}"
        );
    }
    // T2 allows writes and routines, but reprogramming only on an expendable ECU.
    assert!(matches!(
        parse(&profile("T2", r#"allow = ["reprogramming"]"#)),
        Err(ProfileError::Adds {
            class: Class::T2,
            ..
        })
    ));
    let expendable = parse(&profile("T2", "expendable_ecu = true")).unwrap();
    assert!(expendable.allows(Operation::Reprogramming));
    assert!(matches!(
        parse(&t1("expendable_ecu = true")),
        Err(ProfileError::Adds {
            class: Class::T1,
            ..
        })
    ));
    // T0 allows no ECU operation and no addition.
    assert!(matches!(
        parse(&T0_PROFILE.replace("class = \"T0\"", "class = \"T0\"\nallow = [\"routine\"]")),
        Err(ProfileError::Adds {
            class: Class::T0,
            ..
        })
    ));
    assert!(matches!(
        parse(&t1(r#"allow = ["flash"]"#)),
        Err(ProfileError::Invalid(_))
    ));
}

#[test]
fn power_and_hardware_controls_are_t1_and_t2_additions() {
    let extra = r#"
[power]
off = ["relay", "off"]
on = ["relay", "on"]

[[hardware-control]]
kind = "programming-voltage"
name = "feps"
pin = 12
millivolts = 18000

[[hardware-control]]
kind = "ioctl"
name = "vendor-led"
id = 0x10001
payload = [1]
"#;
    for class in ["T1", "T2"] {
        let profile = parse(&profile(class, extra)).unwrap();
        assert!(profile.allows(Operation::PowerSwitching));
        assert!(profile.allows(Operation::HardwareControl("feps")));
        assert!(!profile.allows(Operation::HardwareControl("other")));
    }
    let plain = parse(&t1("")).unwrap();
    assert!(!plain.allows(Operation::PowerSwitching));
    // A control without its complete parameters, or named twice.
    for control in [
        "kind = \"ioctl\"\nname = \"x\"\nid = 1",
        "kind = \"ioctl\"\nname = \"x\"\nid = 1\npayload = [1]\nrange = [0, 1]",
        "kind = \"ioctl\"\nname = \"x\"\nid = 1\nrange = [2, 1]",
        "kind = \"pin-switch\"\nname = \"x\"\npins = []",
        "kind = \"programming-voltage\"\nname = \"x\"\npin = 12",
    ] {
        let text = t1(&format!("[[hardware-control]]\n{control}"));
        assert!(parse(&text).is_err(), "{control}");
    }
    let twice = "[[hardware-control]]\nkind = \"pin-switch\"\nname = \"x\"\npins = [7]\n";
    assert!(parse(&t1(&format!("{twice}{twice}"))).is_err());
    assert!(parse(&t1("[power]\noff = []\non = [\"relay\"]")).is_err());
}

#[test]
fn an_incomplete_or_inconsistent_profile_is_refused() {
    let cases: &[(&str, &str)] = &[
        ("id_bits = 11", "id_bits = 12"),
        ("request_id = 0x7E0", "request_id = 0x800"),
        ("response_id = 0x7E8", "response_id = 0x7E0"),
        ("pins = [6, 14]", "pins = [6, 6]"),
        ("baud_rate = 500000", "baud_rate = 0"),
        ("addressing = \"normal\"", "addressing = \"extended\""),
        (
            "addressing = \"normal\"",
            "addressing = \"normal\"\naddress = 0xF1",
        ),
        ("CP_STmin = 0", "STmin = 0"),
        ("label = \"bench\"\n", ""),
        ("api = \"j2534-0404\"", "api = \"iso22900\""),
        ("module = \"VCI-1\"", "module = \"\""),
        ("padding = 0xCC", "padding = 0xCC\nunknown = 1"),
        ("class = \"T1\"", "class = \"T4\""),
    ];
    for (from, to) in cases {
        let text = t1("").replacen(from, to, 1);
        assert_ne!(text, t1(""), "{from}");
        assert!(parse(&text).is_err(), "{to}");
    }
    // 29-bit identifiers fit 29 bits.
    let wide = t1("")
        .replace("id_bits = 11", "id_bits = 29")
        .replace("0x7E0", "0x18DA10F1");
    parse(&wide).unwrap();
    // Extended addressing with its address.
    parse(&t1("").replace(
        "addressing = \"normal\"",
        "addressing = \"extended\"\naddress = 0xF1",
    ))
    .unwrap();
    // T0 needs two endpoints, and the other classes one link.
    let one_endpoint = T0_PROFILE.rsplit_once("[[endpoint]]").unwrap().0;
    assert!(parse(one_endpoint).is_err());
    assert!(parse(&T0_PROFILE.replace("class = \"T0\"", "class = \"T1\"")).is_err());
    assert!(parse(&t1("").replace("class = \"T1\"", "class = \"T0\"")).is_err());
    // An endpoint names a VCI or a SocketCAN interface, not both.
    assert!(parse(&T0_PROFILE.replace("socketcan = \"can0\"", "")).is_err());
}

#[test]
fn more_fields_are_checked_against_the_setup() {
    let cases: &[(&str, &str)] = &[
        // Connector pins, and identifiers that all differ.
        ("pins = [6, 14]", "pins = [0, 14]"),
        ("pins = [6, 14]", "pins = [6, 17]"),
        ("functional_id = 0x7DF", "functional_id = 0x7E0"),
        ("functional_id = 0x7DF", "functional_id = 0x800"),
        // The physical layer comes from the typed keys, and a ComParam has a name.
        ("CP_STmin = 0", "CP_STmin = 0\nCP_Baudrate = 250000"),
        ("CP_STmin = 0", "CP_ = 0"),
        // Empty and missing values, and integers that do not fit.
        ("label = \"bench\"", "label = \"\""),
        ("pname = \"Vendor VCI 1\"\n", ""),
        ("firmware_version = \"1.2.3\"", "firmware_version = \"\""),
        ("padding = 0xCC", "padding = 256"),
        ("padding = 0xCC", "padding = -1"),
        ("baud_rate = 500000", "baud_rate = 0x100000000"),
    ];
    for (from, to) in cases {
        let text = t1("").replacen(from, to, 1);
        assert_ne!(text, t1(""), "{from}");
        assert!(parse(&text).is_err(), "{to}");
    }
    assert!(parse(&t1("").replace("addressing = \"normal\"", "addressing = \"mixed\"")).is_err());
    for control in [
        "kind = \"pin-switch\"\nname = \"x\"\npins = [7, 7]",
        "kind = \"pin-switch\"\nname = \"x\"\npins = [17]",
        "kind = \"programming-voltage\"\nname = \"x\"\npin = 0\nmillivolts = 18000",
    ] {
        let text = t1(&format!("[[hardware-control]]\n{control}"));
        assert!(parse(&text).is_err(), "{control}");
    }
    assert!(parse(&t1("[power]\noff = [\"\"]\non = [\"relay\"]")).is_err());
    // The worker's default entry label cannot name the profile's entry, also with spaces round it.
    for label in ["j2534-0404", " j2534-0404 "] {
        let text = t1("").replacen("label = \"bench\"", &format!("label = \"{label}\""), 1);
        assert!(parse(&text).is_err(), "{label}");
    }
    // A blank value is an empty one.
    assert!(parse(&t1("").replacen("label = \"bench\"", "label = \" \"", 1)).is_err());
    // A D-PDU API module reports coded versions, so its profile writes them as such; a J2534
    // device reports strings, which are taken as written.
    parse(T0_PROFILE).unwrap();
    parse(&T0_PROFILE.replacen("\"2.0.0\"", "\" 2.0.0 \"", 1)).unwrap();
    for (from, to) in [
        ("firmware_version = \"2.0.0\"", "firmware_version = \"2.0\""),
        (
            "library_version = \"1.0.0\"",
            "library_version = \"1.0.256\"",
        ),
    ] {
        let text = T0_PROFILE.replacen(from, to, 1);
        assert_ne!(text, T0_PROFILE, "{from}");
        assert!(parse(&text).is_err(), "{to}");
    }
}

#[test]
fn a_loopback_runs_between_two_ends_of_one_bus() {
    parse(T0_PROFILE).unwrap();
    parse(&T0_PROFILE.replacen("\"2.0.0\"", "\" 2.0.0 \"", 1)).unwrap();
    // T0 takes no addition either.
    for extra in [
        "expendable_ecu = true",
        "[power]\noff = [\"relay\", \"off\"]\non = [\"relay\", \"on\"]",
        "[[hardware-control]]\nkind = \"pin-switch\"\nname = \"x\"\npins = [7]",
    ] {
        let text = T0_PROFILE.replace("class = \"T0\"", &format!("class = \"T0\"\n{extra}"));
        assert!(
            matches!(
                parse(&text),
                Err(ProfileError::Adds {
                    class: Class::T0,
                    ..
                })
            ),
            "{extra}"
        );
    }
    // The two ends of `T0_PROFILE`: a SocketCAN end sending 0x123, and a VCI end sending 0x124.
    let can_end = T0_PROFILE.split("[[endpoint]]").nth(1).unwrap();
    let vci_end = T0_PROFILE.split("[[endpoint]]").nth(2).unwrap();
    // The same end sending 0x123 instead, so that it pairs with `vci_end`.
    let partner = |end: &str| {
        end.replacen("request_id = 0x124", "request_id = 0x999", 1)
            .replacen("response_id = 0x123", "response_id = 0x124", 1)
            .replacen("request_id = 0x999", "request_id = 0x123", 1)
    };
    let with_channel = |end: &str, name: &str| {
        end.replacen(
            "[endpoint.vci]",
            &format!("channel = \"{name}\"\n[endpoint.vci]"),
            1,
        )
    };
    let loopback = |a: &str, b: &str| format!("class = \"T0\"\n[[endpoint]]{a}[[endpoint]]{b}");
    // Refused, and for the reason the message names.
    let refused_for = |text: &str, reason: &str| match parse(text) {
        Err(ProfileError::Invalid(message)) => {
            assert!(
                message.contains(reason),
                "{message} does not say {reason:?}"
            )
        }
        other => panic!("{other:?}: expected a refusal for {reason:?}"),
    };

    // Two SocketCAN ends and no VCI.
    let can_partner = can_end
        .replace("can0", "can1")
        .replacen("request_id = 0x123", "request_id = 0x999", 1)
        .replacen("response_id = 0x124", "response_id = 0x123", 1)
        .replacen("request_id = 0x999", "request_id = 0x124", 1);
    refused_for(&loopback(can_end, &can_partner), "VCI channel at one end");
    // One VCI end with a channel, opposite a SocketCAN end, is a valid loopback.
    parse(&loopback(can_end, &with_channel(vci_end, "CAN_CH1"))).unwrap();

    // Two channels of one VCI, the preferred loopback, differ by their channel.
    let ch1 = with_channel(vci_end, "CAN_CH1");
    parse(&loopback(&ch1, &partner(&with_channel(vci_end, "CAN_CH2")))).unwrap();
    // The same channel, however it is spelled, or a channel at one end only, in either order.
    for other in ["CAN_CH1", "can_ch1", " CAN_CH01 "] {
        refused_for(
            &loopback(&ch1, &partner(&with_channel(vci_end, other))),
            "must be different channels",
        );
    }
    refused_for(
        &loopback(vci_end, &partner(vci_end)),
        "must be different channels",
    );
    refused_for(
        &loopback(&ch1, &partner(vci_end)),
        "must be different channels",
    );
    refused_for(
        &loopback(vci_end, &partner(&ch1)),
        "must be different channels",
    );
    assert!(parse(&loopback(&with_channel(vci_end, " "), &partner(&ch1))).is_err());

    // Names are matched exactly, as the library resolvers match them: a library in another
    // case is another library.
    let shouted = vci_end.replacen("library = \"Vendor PDU\"", "library = \"VENDOR PDU\"", 1);
    parse(&loopback(vci_end, &partner(&shouted))).unwrap();
    // One module described with two versions.
    let other_version = vci_end.replacen(
        "firmware_version = \"2.0.0\"",
        "firmware_version = \"2.1.0\"",
        1,
    );
    refused_for(
        &loopback(&ch1, &partner(&with_channel(&other_version, "CAN_CH2"))),
        "describe it differently",
    );
    // Two modules of one library are two devices.
    let second_module = vci_end.replacen("module = \"SN-0001\"", "module = \"SN-0002\"", 1);
    parse(&loopback(vci_end, &partner(&second_module))).unwrap();

    // J2534 names its module by the configured entry: the same label or `pname` is one device,
    // and two entries reporting the same `module` are two.
    let j2534 = |label: &str, pname: &str, module: &str| {
        vci_end
            .replacen("api = \"iso22900\"", "api = \"j2534-0404\"", 1)
            .replacen(
                "module = \"SN-0001\"",
                &format!("module = \"{module}\"\nlabel = \"{label}\"\npname = \"{pname}\""),
                1,
            )
    };
    let first = j2534("bench-a", "Vendor VCI A", "VCI");
    parse(&loopback(
        &first,
        &partner(&j2534("bench-b", "Vendor VCI B", "VCI")),
    ))
    .unwrap();
    for other in [
        j2534("bench-a", "Vendor VCI B", "VCI-2"),
        j2534("bench-b", "Vendor VCI A", "VCI-2"),
    ] {
        assert!(parse(&loopback(&first, &partner(&other))).is_err());
    }
    // A loopback across the two APIs is two modules (design 13.5).
    parse(&loopback(
        vci_end,
        &partner(&j2534("bench-a", "Vendor VCI A", "SN-0001")),
    ))
    .unwrap();

    // A SocketCAN end has no channel.
    let can_channel = T0_PROFILE.replacen(
        "socketcan = \"can0\"",
        "socketcan = \"can0\"\nchannel = \"1\"",
        1,
    );
    refused_for(&can_channel, "socketcan");
    // Other physical-layer ComParams, in any case, are refused like `CP_Baudrate`.
    for name in [
        "CP_CANFDBaudrate",
        "CP_BAUDRATE",
        "CP_TerminationType",
        "CP_NetworkLine",
        "CP_UartConfig",
        "CP_J1850IFRCtrl",
    ] {
        let text = t1("").replacen("CP_STmin = 0", &format!("{name} = 1"), 1);
        refused_for(&text, "physical-layer");
    }
    let rate = T0_PROFILE.replacen("baud_rate = 500000", "baud_rate = 250000", 1);
    refused_for(&rate, "one bus");
    let width = T0_PROFILE.replacen("id_bits = 11", "id_bits = 29", 1);
    refused_for(&width, "one bus");
    let not_crossed = T0_PROFILE.replacen("request_id = 0x124", "request_id = 0x125", 1);
    refused_for(&not_crossed, "receives on the identifier");
}

#[test]
fn reading_the_profile_needs_the_variable() {
    // The variable is not set in the test environment unless a hardware run sets it.
    if std::env::var_os(profile::PROFILE_VAR).is_none() {
        assert_eq!(profile::from_env(), Err(ProfileError::NotSet));
    }
}

const READ: CheckDecl = CheckDecl {
    name: "hw_reads_the_vin",
    classes: &[Class::T1, Class::T2, Class::T3],
    operations: &[],
};
const WRITE: CheckDecl = CheckDecl {
    name: "hw_writes_a_configuration_value",
    classes: &[Class::T1, Class::T2],
    operations: &[Operation::ConfigurationWrite],
};
const LOOPBACK: CheckDecl = CheckDecl {
    name: "hw_loops_a_frame_back",
    classes: &[Class::T0],
    operations: &[],
};
const POWER_LOSS: CheckDecl = CheckDecl {
    name: "hw_survives_power_loss_mid_transfer",
    classes: &[Class::T1],
    operations: &[Operation::Reprogramming, Operation::PowerSwitching],
};

#[test]
fn a_check_runs_only_on_its_classes_with_its_operations() {
    let t1 = parse(&t1("")).unwrap();
    permits(&t1, &READ).unwrap();
    permits(&t1, &WRITE).unwrap();
    assert_eq!(
        permits(&t1, &LOOPBACK),
        Err(Refusal::Class {
            class: Class::T1,
            needs: vec![Class::T0]
        })
    );
    assert_eq!(
        permits(&t1, &POWER_LOSS),
        Err(Refusal::Operation(Operation::PowerSwitching))
    );
    let t3 = parse(&profile("T3", "")).unwrap();
    permits(&t3, &READ).unwrap();
    assert!(matches!(permits(&t3, &WRITE), Err(Refusal::Class { .. })));
    let narrowed = parse(&profile("T2", r#"allow = ["routine"]"#)).unwrap();
    assert_eq!(
        permits(&narrowed, &WRITE),
        Err(Refusal::Operation(Operation::ConfigurationWrite))
    );
}

#[test]
fn the_listing_shows_what_a_profile_permits_without_running_anything() {
    let t1 = parse(&t1("")).unwrap();
    let listing = listing(&t1, &[READ, LOOPBACK, POWER_LOSS]);
    assert_eq!(
        listing,
        "hw_reads_the_vin: permitted\n\
         hw_loops_a_frame_back: refused: the check runs on [T0], not on the profile's T1\n\
         hw_survives_power_loss_mid_transfer: refused: the check needs PowerSwitching, which \
         the profile does not allow\n"
    );
}

#[test]
fn a_hardware_check_without_a_profile_fails() {
    assert_eq!(
        start(&READ, Err(ProfileError::NotSet)),
        Err(StartError::Profile(ProfileError::NotSet))
    );
    let t3 = parse(&profile("T3", "")).unwrap();
    assert!(matches!(
        start(&WRITE, Ok(t3.clone())),
        Err(StartError::Refused {
            check: "hw_writes_a_configuration_value",
            ..
        })
    ));
    assert_eq!(start(&READ, Ok(t3.clone())), Ok(t3));
}

#[test]
fn a_profile_is_read_from_its_file() {
    let dir = std::env::temp_dir().join(format!("ngr-hw-checks-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("profile.toml");
    std::fs::write(&path, t1("")).unwrap();
    assert_eq!(profile::from_path(&path).unwrap(), parse(&t1("")).unwrap());
    assert!(matches!(
        profile::from_path(&dir.join("missing.toml")),
        Err(ProfileError::Read { .. })
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "real-hardware check, design 13.5: run with `cargo nextest run --profile hardware --run-ignored=only`"]
fn hw_the_profile_permits_a_check() {
    let profile = begin(&PROFILE_CHECK);
    let mut report = report::Report::new(&profile);
    let result = permits(&profile, &PROFILE_CHECK);
    match &result {
        Ok(()) => report.record(PROFILE_CHECK.name, report::Outcome::Passed, ""),
        Err(refusal) => report.record_refusal(PROFILE_CHECK.name, refusal),
    }
    report.write().unwrap();
    result.unwrap();
}

/// Every declared check is a `hw_` test that runs somewhere, declared once.
#[test]
fn every_declared_check_is_named_hw_and_has_a_class() {
    for (index, check) in CHECKS.iter().enumerate() {
        assert!(
            CHECKS[..index].iter().all(|other| other.name != check.name),
            "{} is declared twice",
            check.name
        );
        assert!(check.name.starts_with("hw_"), "{}", check.name);
        assert!(!check.classes.is_empty(), "{}", check.name);
    }
}
