//! Which bytes of a message the communication log keeps (design 8.11, ADR-347).

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::artifact::to_hex;

/// The direction of a message, for [`redact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Request,
    Response,
}

/// A message as the log carries it: the bytes that stay in clear, hex-encoded, and what became
/// of the rest. Only [`redact`] builds one, so no entry can carry a secret by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LoggedMessage {
    clear: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Redacted>,
}

/// The part of a message that is not written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Redacted {
    /// A secret: its length only, since a key can have so few values that any digest of it could
    /// be recovered by trying them all (ADR-347). A seed also says whether it is all zero.
    Secret {
        len: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        all_zero: Option<bool>,
    },
    /// Bulk data: its length and SHA-256.
    Bulk { len: usize, sha256: String },
}

const SID_SECURITY_ACCESS: u8 = 0x27;
const SID_SECURITY_ACCESS_POSITIVE: u8 = 0x67;
const SID_AUTHENTICATION: u8 = 0x29;
const SID_AUTHENTICATION_POSITIVE: u8 = 0x69;
const SID_TRANSFER_DATA: u8 = 0x36;
const SID_TRANSFER_DATA_POSITIVE: u8 = 0x76;

/// `message` as the log carries it (design 8.11, ADR-347). By service (ISO 14229-1:2026 9.4
/// SecurityAccess, 14.4 TransferData):
/// - a SecurityAccess request whose sub-function (without the suppress bit) is even carries a
///   key: the service and sub-function stay, the key is its length only;
/// - a SecurityAccess response whose sub-function (without the suppress bit) is odd carries a
///   seed: its length and whether it is all zero (an ECU whose level is unlocked answers a
///   zero seed); an empty seed is not a zero seed;
/// - an Authentication request or response (ISO 14229-1:2026 29): the service and sub-function
///   stay, the rest is its length only;
/// - a TransferData request, and a response with bytes after the block counter: the service and
///   block counter stay, the data is its length and SHA-256;
/// - everything else, negative responses included, stays whole.
///
/// A message too short for its row stays as far as it goes.
pub(crate) fn redact(direction: Direction, message: &[u8]) -> LoggedMessage {
    let whole = || LoggedMessage {
        clear: to_hex(message),
        data: None,
    };
    let (&sid, rest) = match message.split_first() {
        Some(split) => split,
        None => return whole(),
    };
    let with_sub = |data: fn(&[u8]) -> Redacted| match rest.split_first() {
        Some((_, tail)) => LoggedMessage {
            clear: to_hex(&message[..2]),
            data: Some(data(tail)),
        },
        None => whole(),
    };
    let secret = |tail: &[u8]| Redacted::Secret {
        len: tail.len(),
        all_zero: None,
    };
    match (direction, sid) {
        (Direction::Request, SID_SECURITY_ACCESS)
            if rest.first().is_some_and(|s| s & 0x7F & 1 == 0) =>
        {
            with_sub(secret)
        }
        (Direction::Response, SID_SECURITY_ACCESS_POSITIVE)
            if rest.first().is_some_and(|s| s & 0x7F & 1 == 1) =>
        {
            with_sub(|tail| Redacted::Secret {
                len: tail.len(),
                all_zero: Some(!tail.is_empty() && tail.iter().all(|byte| *byte == 0)),
            })
        }
        (Direction::Request, SID_AUTHENTICATION)
        | (Direction::Response, SID_AUTHENTICATION_POSITIVE) => with_sub(secret),
        (Direction::Request, SID_TRANSFER_DATA) => with_sub(bulk),
        (Direction::Response, SID_TRANSFER_DATA_POSITIVE) if rest.len() > 1 => with_sub(bulk),
        _ => whole(),
    }
}

fn bulk(data: &[u8]) -> Redacted {
    Redacted::Bulk {
        len: data.len(),
        sha256: to_hex(&Sha256::digest(data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_of(direction: Direction, message: &[u8]) -> (String, Option<Redacted>) {
        let logged = redact(direction, message);
        (logged.clear, logged.data)
    }

    #[test]
    fn a_security_access_key_is_its_length_only() {
        let (clear, data) = clear_of(Direction::Request, &[0x27, 0x02, 1, 2, 3, 4]);
        assert_eq!(clear, "2702");
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 4,
                all_zero: None
            })
        );
        // The suppress bit stays in the sub-function; an even sub-function is still a key.
        let (clear, data) = clear_of(Direction::Request, &[0x27, 0x82, 9, 9]);
        assert_eq!(clear, "2782");
        assert!(matches!(data, Some(Redacted::Secret { len: 2, .. })));
        // A key request with no key is a key of length 0.
        let (clear, data) = clear_of(Direction::Request, &[0x27, 0x04]);
        assert_eq!(clear, "2704");
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 0,
                all_zero: None
            })
        );
    }

    #[test]
    fn a_security_access_seed_request_stays_whole() {
        let (clear, data) = clear_of(Direction::Request, &[0x27, 0x01]);
        assert_eq!((clear.as_str(), data), ("2701", None));
        let (clear, data) = clear_of(Direction::Request, &[0x27, 0x03, 0xAA]);
        assert_eq!((clear.as_str(), data), ("2703aa", None));
    }

    #[test]
    fn a_security_access_seed_is_its_length_and_whether_it_is_zero() {
        let (clear, data) = clear_of(Direction::Response, &[0x67, 0x01, 0, 0, 0, 0]);
        assert_eq!(clear, "6701");
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 4,
                all_zero: Some(true)
            })
        );
        let (_, data) = clear_of(Direction::Response, &[0x67, 0x01, 0, 0, 1, 0]);
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 4,
                all_zero: Some(false)
            })
        );
        // The suppress bit stays in the sub-function.
        let (clear, data) = clear_of(Direction::Response, &[0x67, 0x81, 7]);
        assert_eq!(clear, "6781");
        assert!(matches!(data, Some(Redacted::Secret { len: 1, .. })));
        // An empty seed is not a zero seed.
        let (_, data) = clear_of(Direction::Response, &[0x67, 0x01]);
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 0,
                all_zero: Some(false)
            })
        );
    }

    #[test]
    fn a_security_access_key_response_stays_whole() {
        let (clear, data) = clear_of(Direction::Response, &[0x67, 0x02]);
        assert_eq!((clear.as_str(), data), ("6702", None));
    }

    #[test]
    fn authentication_keeps_the_service_and_sub_function_only() {
        let (clear, data) = clear_of(Direction::Request, &[0x29, 0x03, 5, 6, 7]);
        assert_eq!(clear, "2903");
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 3,
                all_zero: None
            })
        );
        let (clear, data) = clear_of(Direction::Response, &[0x69, 0x83, 1]);
        assert_eq!(clear, "6983");
        assert_eq!(
            data,
            Some(Redacted::Secret {
                len: 1,
                all_zero: None
            })
        );
    }

    #[test]
    fn transfer_data_is_a_length_and_a_hash() {
        let data = [1u8, 2, 3, 4, 5];
        let mut request = vec![0x36, 0x07];
        request.extend_from_slice(&data);
        let (clear, redacted) = clear_of(Direction::Request, &request);
        assert_eq!(clear, "3607");
        assert_eq!(
            redacted,
            Some(Redacted::Bulk {
                len: 5,
                sha256: to_hex(&Sha256::digest(data))
            })
        );
        let (clear, redacted) = clear_of(Direction::Response, &[0x76, 0x07, 9, 9]);
        assert_eq!(clear, "7607");
        assert!(matches!(redacted, Some(Redacted::Bulk { len: 2, .. })));
        // A response that is only the counter is whole.
        let (clear, redacted) = clear_of(Direction::Response, &[0x76, 0x07]);
        assert_eq!((clear.as_str(), redacted), ("7607", None));
    }

    #[test]
    fn other_messages_stay_whole() {
        for (direction, message) in [
            (Direction::Request, &[0x22, 0xF1, 0x90][..]),
            (Direction::Response, &[0x62, 0xF1, 0x90, 0x41][..]),
            (Direction::Response, &[0x7F, 0x27, 0x35][..]),
            (Direction::Response, &[0x7F, 0x36, 0x73][..]),
            // A request is not a response of the same service number.
            (Direction::Request, &[0x67, 0x01, 0, 0][..]),
            (Direction::Response, &[0x27, 0x02, 1][..]),
        ] {
            let (clear, data) = clear_of(direction, message);
            assert_eq!(clear, to_hex(message));
            assert_eq!(data, None, "{message:02x?}");
        }
    }

    #[test]
    fn a_message_too_short_for_its_row_stays_as_far_as_it_goes() {
        for (direction, message) in [
            (Direction::Request, &[][..]),
            (Direction::Request, &[0x27][..]),
            (Direction::Response, &[0x67][..]),
            (Direction::Request, &[0x29][..]),
            (Direction::Request, &[0x36][..]),
            (Direction::Response, &[0x76][..]),
        ] {
            let (clear, data) = clear_of(direction, message);
            assert_eq!(clear, to_hex(message));
            assert_eq!(data, None);
        }
    }

    #[test]
    fn a_logged_message_serializes_without_the_redacted_bytes() {
        let json = serde_json::to_value(redact(Direction::Response, &[0x67, 1, 0, 0])).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "clear": "6701",
                "data": { "kind": "secret", "len": 2, "all_zero": true }
            })
        );
        let json = serde_json::to_value(redact(Direction::Request, &[0x10, 0x03])).unwrap();
        assert_eq!(json, serde_json::json!({ "clear": "1003" }));
    }
}
