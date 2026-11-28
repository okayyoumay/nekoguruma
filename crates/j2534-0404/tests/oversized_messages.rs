//! A message whose native `DataSize` exceeds the payload array is refused before the native call.
//!
//! The channel id is never connected, so a call that reached the mock library would fail with an
//! API status; `MessageTooLarge` therefore shows the check ran first.

use j2534_0404::{Error, ISO15765, J2534Api0404, PassThruMessage, RepeatMsgSetup};

const UNCONNECTED_CHANNEL: j2534_0404::ChannelId = j2534_0404::ChannelId(0xFFFF);

fn load_mock() -> J2534Api0404 {
    let path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
    J2534Api0404::from_path(path).expect("mock cdylib should load")
}

fn message(data_size: u32) -> PassThruMessage {
    let mut message = PassThruMessage::new(ISO15765, 0, 0, 0, 0, &[0x3E, 0x00]).unwrap();
    message.0.DataSize = data_size;
    message
}

#[test]
fn fast_init_refuses_an_oversized_input_message() {
    let api = load_mock();

    let err = api
        .fast_init(UNCONNECTED_CHANNEL, Some(&message(5000)))
        .unwrap_err();
    assert!(
        matches!(err, Error::MessageTooLarge { len: 5000, .. }),
        "{err:?}"
    );

    // Control: the same call with a sound message does reach the library and fails there.
    let err = api
        .fast_init(UNCONNECTED_CHANNEL, Some(&message(2)))
        .unwrap_err();
    assert!(matches!(err, Error::ApiStatus { .. }), "{err:?}");
}

#[test]
fn start_repeat_message_refuses_any_oversized_message() {
    let api = load_mock();

    for oversized in 0..3 {
        let mut parts = [message(2), message(2), message(2)];
        parts[oversized] = message(5000);
        let [m, mask, pattern] = parts;
        let mut setup = RepeatMsgSetup::new(100, 0, m, mask, pattern);
        let err = api
            .start_repeat_message(UNCONNECTED_CHANNEL, &mut setup)
            .unwrap_err();
        assert!(
            matches!(err, Error::MessageTooLarge { len: 5000, .. }),
            "entry {oversized}: {err:?}"
        );
    }

    let mut setup = RepeatMsgSetup::new(100, 0, message(2), message(2), message(2));
    let err = api
        .start_repeat_message(UNCONNECTED_CHANNEL, &mut setup)
        .unwrap_err();
    assert!(matches!(err, Error::ApiStatus { .. }), "{err:?}");
}
