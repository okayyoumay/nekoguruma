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
    // One protocol family at both ends: a raw CAN end opposite an ISO 15765 end frames the same
    // payload differently, while an additional channel of the protocol is the same family.
    let with_protocol = |end: &str, name: &str| {
        end.replacen("protocol = \"CAN\"", &format!("protocol = \"{name}\""), 1)
    };
    refused_for(
        &loopback(
            vci_end,
            &partner(&with_protocol(&second_module, "ISO15765")),
        ),
        "both raw CAN or both ISO 15765",
    );
    parse(&loopback(
        &with_protocol(vci_end, "CAN"),
        &partner(&with_protocol(&second_module, "can_ch1")),
    ))
    .unwrap();
    // Names are matched ignoring case, aliases of one family agree, and a protocol outside the
    // two families is refused.
    refused_for(
        &loopback(
            vci_end,
            &partner(&with_protocol(&second_module, "iso15765")),
        ),
        "both raw CAN or both ISO 15765",
    );
    refused_for(
        &loopback(vci_end, &partner(&with_protocol(&second_module, "ISO-TP"))),
        "both raw CAN or both ISO 15765",
    );
    parse(&loopback(
        &with_protocol(vci_end, "iso-tp"),
        &partner(&with_protocol(&second_module, "ISO_14229_3_on_ISO_15765_2")),
    ))
    .unwrap();
    parse(&loopback(
        vci_end,
        &partner(&with_protocol(&second_module, "ISO_11898_RAW")),
    ))
    .unwrap();
    parse(&loopback(
        vci_end,
        &partner(&with_protocol(
            &second_module,
            "ISO_11783_12_on_ISO_11783_5",
        )),
    ))
    .unwrap();
    parse(&loopback(
        &with_protocol(vci_end, "ISO15765"),
        &partner(&with_protocol(
            &second_module,
            "ISO_14229_3_on_ISO_15765_2_with_ISO_11783_5",
        )),
    ))
    .unwrap();
    // A name the worker does not map, such as `CAN_PS`, is refused before any device opens.
    refused_for(
        &loopback(vci_end, &partner(&with_protocol(&second_module, "CAN_PS"))),
        "runs on raw CAN",
    );
    refused_for(
        &loopback(
            &with_protocol(vci_end, "ISO9141"),
            &partner(&with_protocol(&second_module, "ISO9141")),
        ),
        "runs on raw CAN",
    );
    // Frame padding is ISO 15765 framing, so a raw CAN end takes none.
    refused_for(
        &loopback(
            &with_protocol(vci_end, "CAN").replacen(
                "addressing = \"normal\"",
                "addressing = \"normal\"\npadding = 0xCC",
                1,
            ),
            &partner(&with_protocol(&second_module, "CAN")),
        ),
        "takes no `padding`",
    );
    // So is extended addressing.
    refused_for(
        &loopback(
            &with_protocol(vci_end, "CAN").replacen(
                "addressing = \"normal\"",
                "addressing = \"extended\"\naddress = 0x10",
                1,
            ),
            &partner(&with_protocol(&second_module, "CAN")).replacen(
                "addressing = \"normal\"",
                "addressing = \"extended\"\naddress = 0x10",
                1,
            ),
        ),
        "only `normal` addressing",
    );
    // The worker does not trim a protocol name, so `" CAN "` is no CAN name.
    refused_for(
        &loopback(
            &with_protocol(vci_end, " CAN "),
            &partner(&with_protocol(&second_module, " CAN ")),
        ),
        "runs on raw CAN",
    );
    // A bare `_CH` names no channel index, and the worker does not map `CAN_CH`.
    refused_for(
        &loopback(
            &with_protocol(vci_end, "CAN_CH"),
            &partner(&with_protocol(&second_module, "CAN_CH")),
        ),
        "runs on raw CAN",
    );

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
    // The two ends share the addressing and, when addressed, the address byte (over ISO 15765:
    // a raw CAN end takes only normal addressing).
    let iso15765 = T0_PROFILE.replace("protocol = \"CAN\"", "protocol = \"ISO15765\"");
    let extended_both = |a: &str, b: &str| {
        let first = iso15765.replacen(
            "addressing = \"normal\"",
            &format!("addressing = \"extended\"\naddress = {a}"),
            1,
        );
        first.replacen(
            "addressing = \"normal\"",
            &format!("addressing = \"extended\"\naddress = {b}"),
            1,
        )
    };
    parse(&extended_both("0x55", "0x55")).unwrap();
    refused_for(&extended_both("0x10", "0xF1"), "share one `addressing`");
    let one_extended = iso15765.replacen(
        "addressing = \"normal\"",
        "addressing = \"extended\"\naddress = 0x55",
        1,
    );
    refused_for(&one_extended, "share one `addressing`");
    let mixed_and_extended = extended_both("0x55", "0x55").replacen(
        "addressing = \"extended\"",
        "addressing = \"mixed\"",
        1,
    );
    refused_for(&mixed_and_extended, "share one `addressing`");
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

/// The first hardware check (design 13.5): listed for a T0 profile, refused on every other class,
/// and it fails without a profile.
#[test]
fn the_t0_loopback_check_is_listed_for_t0_and_refused_elsewhere() {
    assert!(CHECKS.contains(&T0_LOOPBACK_CHECK));
    assert_eq!(T0_LOOPBACK_CHECK.name, "hw_t0_two_endpoint_loopback");
    let t0 = parse(T0_PROFILE).unwrap();
    assert_eq!(
        listing(&t0, &[T0_LOOPBACK_CHECK]),
        "hw_t0_two_endpoint_loopback: permitted\n"
    );
    assert_eq!(start(&T0_LOOPBACK_CHECK, Ok(t0.clone())), Ok(t0));
    for class in ["T1", "T2", "T3"] {
        let profile = parse(&profile(class, "")).unwrap();
        assert_eq!(
            listing(&profile, &[T0_LOOPBACK_CHECK]),
            format!(
                "hw_t0_two_endpoint_loopback: refused: the check runs on [T0], not on the \
                 profile's {class}\n"
            )
        );
        assert!(matches!(
            start(&T0_LOOPBACK_CHECK, Ok(profile)),
            Err(StartError::Refused { .. })
        ));
    }
    // Without a profile it does not start.
    assert_eq!(
        start(&T0_LOOPBACK_CHECK, Err(ProfileError::NotSet)),
        Err(StartError::Profile(ProfileError::NotSet))
    );
}

/// The read check: listed for T2 and T3 profiles, refused on T0 and T1, and it fails without a
/// profile.
#[test]
fn the_t2_t3_read_check_is_listed_for_t2_and_t3_and_refused_elsewhere() {
    assert!(CHECKS.contains(&T2_T3_READ_CHECK));
    assert_eq!(T2_T3_READ_CHECK.name, "hw_t2_t3_read");
    for class in ["T2", "T3"] {
        let profile = parse(&profile(class, "")).unwrap();
        assert_eq!(
            listing(&profile, &[T2_T3_READ_CHECK]),
            "hw_t2_t3_read: permitted\n"
        );
        assert_eq!(start(&T2_T3_READ_CHECK, Ok(profile.clone())), Ok(profile));
    }
    let t0 = parse(T0_PROFILE).unwrap();
    let t1 = parse(&t1("")).unwrap();
    for profile in [t0, t1] {
        let class = profile.class;
        assert_eq!(
            listing(&profile, &[T2_T3_READ_CHECK]),
            format!(
                "hw_t2_t3_read: refused: the check runs on [T2, T3], not on the profile's \
                 {class:?}\n"
            )
        );
        assert!(matches!(
            start(&T2_T3_READ_CHECK, Ok(profile)),
            Err(StartError::Refused { .. })
        ));
    }
    assert_eq!(
        start(&T2_T3_READ_CHECK, Err(ProfileError::NotSet)),
        Err(StartError::Profile(ProfileError::NotSet))
    );
}

/// The timing check: listed for a T2 profile only, refused on T0, T1 and T3, and it fails
/// without a profile.
#[test]
fn the_t2_timing_check_is_listed_for_t2_and_refused_elsewhere() {
    assert!(CHECKS.contains(&T2_TIMING_CHECK));
    assert_eq!(T2_TIMING_CHECK.name, "hw_t2_timing");
    let t2 = parse(&profile("T2", "")).unwrap();
    assert_eq!(
        listing(&t2, &[T2_TIMING_CHECK]),
        "hw_t2_timing: permitted\n"
    );
    assert_eq!(start(&T2_TIMING_CHECK, Ok(t2.clone())), Ok(t2));
    let t0 = parse(T0_PROFILE).unwrap();
    let t1 = parse(&t1("")).unwrap();
    let t3 = parse(&profile("T3", "")).unwrap();
    for profile in [t0, t1, t3] {
        let class = profile.class;
        assert_eq!(
            listing(&profile, &[T2_TIMING_CHECK]),
            format!(
                "hw_t2_timing: refused: the check runs on [T2], not on the profile's {class:?}\n"
            )
        );
        assert!(matches!(
            start(&T2_TIMING_CHECK, Ok(profile)),
            Err(StartError::Refused { .. })
        ));
    }
    assert_eq!(
        start(&T2_TIMING_CHECK, Err(ProfileError::NotSet)),
        Err(StartError::Profile(ProfileError::NotSet))
    );
}

/// A profile's `[read]` names the definition and the read procedure's file in it; a T0 profile
/// has no ECU to read, and the procedure is a file name, not a path.
#[test]
fn a_read_definition_names_a_directory_and_a_procedure_file_in_it() {
    let read = "[read]\ndefinition = \"/defs/bench\"\nprocedure = \"read.star\"\n";
    let parsed = parse(&profile("T3", read)).unwrap();
    assert_eq!(
        parsed.read,
        Some(profile::ReadDefinition {
            definition: "/defs/bench".into(),
            procedure: "read.star".to_owned(),
        })
    );
    assert_eq!(parse(&profile("T2", "")).unwrap().read, None);
    for procedure in ["", "sub/read.star", "../read.star"] {
        let read = format!("[read]\ndefinition = \"/defs\"\nprocedure = \"{procedure}\"\n");
        assert!(
            matches!(parse(&profile("T3", &read)), Err(ProfileError::Invalid(_))),
            "{procedure:?}"
        );
    }
    let t0 = format!("{T0_PROFILE}\n[read]\ndefinition = \"/defs\"\nprocedure = \"read.star\"\n");
    assert!(matches!(parse(&t0), Err(ProfileError::Invalid(_))));
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
    // The guard writes the report when the check ends, also when an assertion panics.
    let (_profile, mut run) = begin(&PROFILE_CHECK);
    run.pass();
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

#[test]
fn two_spellings_of_one_comparam_are_refused_whatever_their_values() {
    let with = |lines: &str| t1("").replacen("CP_STmin = 0", lines, 1);
    let refused = |lines: &str, a: &str, b: &str| match parse(&with(lines)) {
        Err(ProfileError::Invalid(message)) => {
            assert!(
                message.contains(a) && message.contains(b),
                "{message} does not name {a} and {b}"
            )
        }
        other => panic!("{other:?}: expected a refusal for {lines:?}"),
    };
    // An alias of the same ComParam, with different and with equal values.
    refused("CP_BSTx = 1\nCP_BS_TX = 2", "CP_BSTx", "CP_BS_TX");
    refused("CP_BSTx = 1\nCP_BS_TX = 1", "CP_BSTx", "CP_BS_TX");
    refused(
        "CP_STminTx = 1
CP_STmin_Tx = 1",
        "CP_STminTx",
        "CP_STmin_Tx",
    );
    // Another case of one name.
    refused("CP_P2Max = 1000\nCP_P2MAX = 1000", "CP_P2Max", "CP_P2MAX");
    // Distinct ComParams still parse.
    parse(&with("CP_BSTx = 1\nCP_STminTx = 2\nCP_P2Max = 3")).unwrap();
}

#[test]
fn an_alias_of_a_physical_comparam_is_refused_like_its_name() {
    for name in ["CP_Bit_Sample_Point", "CP_Network_Line", "CP_NodeAddress"] {
        let text = t1("").replacen("CP_STmin = 0", &format!("{name} = 1"), 1);
        match parse(&text) {
            Err(ProfileError::Invalid(message)) => {
                assert!(message.contains("physical-layer"), "{name}: {message}")
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn only_comparams_the_guard_can_enforce_are_pinned_in_the_profile() {
    let with = |line: &str| t1("").replacen("CP_STmin = 0", line, 1);
    let refused = |line: &str, says: &str| match parse(&with(line)) {
        Err(ProfileError::Invalid(message)) => {
            assert!(
                message.contains(line.split(' ').next().unwrap()) && message.contains(says),
                "{message} does not name the key and say {says:?}"
            )
        }
        other => panic!("{other:?}: expected a refusal for {line:?}"),
    };
    // A byte field cannot be pinned with an integer.
    refused("CP_TesterPresentMessage = 0", "cannot be pinned");
    refused("CP_TesterPresentExpPosResp = 0", "cannot be pinned");
    // Typed fields have their own keys.
    refused("CP_Baudrate = 500000", "baud_rate");
    refused("DATA_RATE = 500000", "CP_");
    refused("CP_CanPhysReqId = 2016", "request_id");
    refused("CP_CanFillerByteHandling = 1", "padding");
    refused("CP_RequestAddrMode = 1", "functional_id");
    // Unknown and vendor names.
    refused("CP_VendorSpecific = 1", "cannot be pinned");
    // A value the workers cannot set as an unsigned 32-bit number.
    refused("CP_STmin = -1", "outside");
    refused("CP_P2Max = 4294967296", "outside");
    // A timing ComParam still parses, in any spelling the guard resolves.
    let profile = parse(&with("CP_P2Max = 1000\nCP_BSTx = 3")).unwrap();
    let Setup::Link { link, .. } = &profile.setup else {
        unreachable!()
    };
    assert_eq!(link.comparams["CP_P2Max"], 1000);
}

#[test]
fn comparams_the_worker_cannot_set_are_not_pinnable() {
    // `CP_TesterPresentImmed` is on no real-protocol allow-list of j2534-0404-service, so
    // `SetComParam` can never set it and a profile pinning it could never connect.
    let text = t1("").replacen("CP_STmin = 0", "CP_TesterPresentImmed = 1", 1);
    match parse(&text) {
        Err(ProfileError::Invalid(message)) => {
            assert!(message.contains("CP_TesterPresentImmed"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!crate::comparam::profile_may_pin("cp_testerpresentimmed"));
    assert!(!crate::comparam::ALLOWED.contains(&"cp_testerpresentimmed"));
}
