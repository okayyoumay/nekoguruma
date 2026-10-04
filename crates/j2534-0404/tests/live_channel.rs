use j2534_0404::{J2534Api0404, PassThruMessage, StatusCode};

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

fn parse_data_bytes_env(name: &str) -> Result<Vec<u8>, String> {
    let raw = match std::env::var(name) {
        Ok(v) => v,
        Err(_) => return Ok(vec![0x00]),
    };

    let value = raw.trim();
    if value.is_empty() {
        return Ok(vec![0x00]);
    }

    value
        .split(',')
        .map(|part| {
            let token = part.trim();
            let token = token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or(token);
            u8::from_str_radix(token, 16)
                .map_err(|err| format!("invalid byte '{part}' in {name}: {err}"))
        })
        .collect()
}

#[test]
fn open_connect_write_read_with_env_configuration() {
    let enabled = std::env::var("J2534_RUN_CHANNEL_TEST")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    let dll_path = match std::env::var("J2534_DLL_PATH") {
        Ok(path) => path,
        Err(_) => return,
    };

    let protocol_id = parse_u32_env("J2534_PROTOCOL_ID", j2534_0404::ISO15765)
        .expect("J2534_PROTOCOL_ID must be a valid u32 (decimal or 0x-prefixed hex)");
    let flags = parse_u32_env("J2534_CONNECT_FLAGS", 0)
        .expect("J2534_CONNECT_FLAGS must be a valid u32 (decimal or 0x-prefixed hex)");
    let baud_rate =
        parse_u32_env("J2534_BAUD_RATE", 500_000).expect("J2534_BAUD_RATE must be a valid u32");
    let write_timeout_ms = parse_u32_env("J2534_WRITE_TIMEOUT_MS", 200)
        .expect("J2534_WRITE_TIMEOUT_MS must be a valid u32");
    let read_timeout_ms = parse_u32_env("J2534_READ_TIMEOUT_MS", 50)
        .expect("J2534_READ_TIMEOUT_MS must be a valid u32");
    let request_data = parse_data_bytes_env("J2534_REQUEST_DATA")
        .expect("J2534_REQUEST_DATA must be comma-separated hex bytes, e.g. 02,10,01");

    let api =
        J2534Api0404::from_path(&dll_path).expect("J2534 DLL should load from J2534_DLL_PATH");
    let device_id = api
        .open(None)
        .expect("PassThruOpen should succeed with configured device");
    let channel_id = api
        .connect(device_id, protocol_id, flags, baud_rate)
        .expect("PassThruConnect should succeed with configured protocol");

    let mut tx_message = PassThruMessage::new(protocol_id, 0, flags, 0, 0, &request_data)
        .expect("request payload should build a valid J2534 message");

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
                    protocol_id,
                    "received message protocol should match configured protocol"
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
        .expect("PassThruDisconnect should succeed after the channel test");
    api.close(device_id)
        .expect("PassThruClose should succeed after the channel test");
}
