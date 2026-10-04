use j2534_0404::{
    J2534Api0404, StatusCode, TX_EXTENDED_ID, TX_ISO15765_FRAME_PAD,
    iso15765::{single_frame, single_frame_extended, with_padding},
};

fn parse_u32_env(name: &str, default: u32) -> Result<u32, String> {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw.trim();
            if let Some(hex) = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                u32::from_str_radix(hex, 16)
                    .map_err(|err| format!("failed to parse {name} as hex u32: {err}"))
            } else {
                value
                    .parse::<u32>()
                    .map_err(|err| format!("failed to parse {name} as decimal u32: {err}"))
            }
        }
        Err(_) => Ok(default),
    }
}

#[test]
fn connect_and_write_with_iso15765_helper_builders() {
    let enabled = std::env::var("J2534_RUN_ISO15765_TEST")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    let dll_path = match std::env::var("J2534_DLL_PATH") {
        Ok(path) => path,
        Err(_) => return,
    };

    let mode = std::env::var("J2534_ISO15765_MODE")
        .unwrap_or_else(|_| "normal11".to_string())
        .to_ascii_lowercase();
    let can_id = parse_u32_env("J2534_ISO15765_CAN_ID", 0x7E0)
        .expect("J2534_ISO15765_CAN_ID must be a valid u32");
    let baud_rate =
        parse_u32_env("J2534_BAUD_RATE", 500_000).expect("J2534_BAUD_RATE must be a valid u32");
    let write_timeout_ms = parse_u32_env("J2534_WRITE_TIMEOUT_MS", 200)
        .expect("J2534_WRITE_TIMEOUT_MS must be a valid u32");
    let read_timeout_ms = parse_u32_env("J2534_READ_TIMEOUT_MS", 50)
        .expect("J2534_READ_TIMEOUT_MS must be a valid u32");

    let api =
        J2534Api0404::from_path(&dll_path).expect("J2534 DLL should load from J2534_DLL_PATH");
    let device_id = api
        .open(None)
        .expect("PassThruOpen should succeed with configured device");
    let channel_id = api
        .connect(device_id, j2534_0404::ISO15765, 0, baud_rate)
        .expect("PassThruConnect should succeed for ISO15765");

    let payload = [0x10, 0x03];
    let mut tx_message = match mode.as_str() {
        "extended29" => single_frame_extended(can_id, &payload)
            .expect("extended helper should build a valid message"),
        _ => single_frame(can_id, &payload).expect("normal helper should build a valid message"),
    };
    tx_message = with_padding(tx_message);

    if mode == "extended29" {
        assert!(
            tx_message.tx_flags() & TX_EXTENDED_ID == TX_EXTENDED_ID,
            "extended mode should set TX_EXTENDED_ID"
        );
    }
    assert!(
        tx_message.tx_flags() & TX_ISO15765_FRAME_PAD == TX_ISO15765_FRAME_PAD,
        "helper padding should set TX_ISO15765_FRAME_PAD"
    );

    let written = api
        .write_messages(
            channel_id,
            std::slice::from_mut(&mut tx_message),
            write_timeout_ms,
        )
        .expect("PassThruWriteMsgs should succeed");
    assert!(
        written >= 1,
        "at least one message should be accepted for transmission"
    );

    match api.read_messages(channel_id, 1, read_timeout_ms) {
        Ok(messages) => {
            if let Some(first) = messages.first() {
                assert_eq!(
                    first.protocol_id(),
                    j2534_0404::ISO15765,
                    "read back message should use ISO15765 protocol"
                );
            }
        }
        Err(j2534_0404::Error::ApiStatus { code, .. }) => {
            assert!(
                code == StatusCode(j2534_0404::ERR_BUFFER_EMPTY),
                "read should only fail with ERR_BUFFER_EMPTY during timeout polling, got {code}"
            );
        }
        Err(other) => panic!("PassThruReadMsgs failed unexpectedly: {other}"),
    }

    api.disconnect(channel_id)
        .expect("PassThruDisconnect should succeed after ISO15765 test");
    api.close(device_id)
        .expect("PassThruClose should succeed after ISO15765 test");
}
