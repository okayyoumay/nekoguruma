use super::*;

fn parse_line(line: &str) -> Result<Command, ArgsError> {
    parse(line.split_whitespace().map(str::to_owned))
}

fn serve(line: &str) -> Args {
    match parse_line(line).expect("the line should parse") {
        Command::Serve(args) => args,
        Command::Help => panic!("expected a server command"),
    }
}

#[test]
fn an_interface_alone_takes_the_legacy_obd_ids_in_11_bits() {
    let args = serve("--interface vcan0");
    assert_eq!(args.interface, "vcan0");
    assert_eq!(args.ids, CanIds::new(0x7E0, 0x7E8, 0x7DF, false).unwrap());
    // 11-bit identifiers go to the socket as they are.
    assert_eq!(
        [
            args.ids.request(),
            args.ids.response(),
            args.ids.functional()
        ],
        [0x7E0, 0x7E8, 0x7DF]
    );
    assert_eq!((args.config, args.state), (None, None));
}

#[test]
fn flags_set_the_ids_the_files_and_accept_hex_or_decimal() {
    let args = serve(
        "--interface can1 --request-id 0x18DA00F1 --response-id 0x18DAF100 \
         --functional-id 0x18DB33F1 --extended --config ecu.json --state ecu.state",
    );
    assert_eq!(args.interface, "can1");
    assert_eq!(args.config, Some(PathBuf::from("ecu.json")));
    assert_eq!(args.state, Some(PathBuf::from("ecu.state")));
    let decimal = serve("--interface vcan0 --request-id 2016");
    assert_eq!(decimal.ids.request(), 0x7E0);
    assert_eq!(
        serve("--interface vcan0 --request-id 0X7E1").ids.request(),
        0x7E1
    );
}

#[test]
fn a_29_bit_id_carries_the_extended_frame_flag_and_keeps_its_value() {
    let args = serve("--interface vcan0 --extended --request-id 0x18DA00F1");
    let id = args.ids.request();
    assert_ne!(id & 0x8000_0000, 0, "the extended flag is set");
    assert_eq!(id & 0x1FFF_FFFF, 0x18DA_00F1);
    // The same value is a different socket address without the flag.
    assert_ne!(
        socket_id(0x7E0, true).unwrap(),
        socket_id(0x7E0, false).unwrap()
    );
    assert_eq!(socket_id(0x7E0, true).unwrap() & 0x1FFF_FFFF, 0x7E0);
}

#[test]
fn an_id_too_wide_for_its_mode_is_refused() {
    assert_eq!(
        socket_id(0x800, false),
        Err(ArgsError::IdTooWide {
            id: 0x800,
            bits: 11
        })
    );
    assert_eq!(socket_id(0x7FF, false), Ok(0x7FF));
    assert_eq!(socket_id(0x1FFF_FFFF, true), Ok(0x9FFF_FFFF));
    // An ID with the flag bit already set would turn an 11-bit setup into a 29-bit one.
    socket_id(0x8000_07E0, true).unwrap_err();
    socket_id(0x8000_07E0, false).unwrap_err();
    // A 29-bit request ID needs --extended.
    assert!(matches!(
        parse_line("--interface vcan0 --request-id 0x18DA00F1"),
        Err(ArgsError::IdTooWide { .. })
    ));
}

#[test]
fn the_three_ids_must_differ() {
    assert_eq!(
        parse_line("--interface vcan0 --response-id 0x7E0"),
        Err(ArgsError::SameId)
    );
    assert_eq!(
        parse_line("--interface vcan0 --functional-id 0x7E0"),
        Err(ArgsError::SameId)
    );
    assert_eq!(
        parse_line("--interface vcan0 --functional-id 0x7E8"),
        Err(ArgsError::SameId)
    );
}

#[test]
fn a_malformed_line_is_refused_with_the_reason() {
    assert_eq!(parse_line(""), Err(ArgsError::NoInterface));
    assert_eq!(
        parse_line("--interface"),
        Err(ArgsError::MissingValue("--interface"))
    );
    assert_eq!(
        parse_line("--interface vcan0 --bogus"),
        Err(ArgsError::Unknown("--bogus".into()))
    );
    assert_eq!(
        parse_line("--interface vcan0 --interface vcan1"),
        Err(ArgsError::Repeated("--interface"))
    );
    let error = parse_line("--interface vcan0 --request-id 0xZZ").unwrap_err();
    assert!(matches!(
        error,
        ArgsError::NotANumber {
            flag: "--request-id",
            ..
        }
    ));
    // The text keeps the parser's cause.
    assert!(error.to_string().contains("invalid digit"), "{error}");
}

#[test]
fn help_wins_over_a_missing_interface() {
    assert_eq!(parse_line("--help"), Ok(Command::Help));
    assert_eq!(parse_line("-h"), Ok(Command::Help));
}
