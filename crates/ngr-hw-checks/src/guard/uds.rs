//! StartComPrimitive: the UDS request a payload carries and
//! the lists each class holds it to (design 13.5 "Guarded VCI access", second and third bullet;
//! ADR-333 item 3).
//!
//! - T0 sends no ECU-addressed request: only raw frames pass, with the sending endpoint's own
//!   request identifier and address byte (the whole header), and their content is not read.
//! - T1 passes every request (the ECU is simulated), on ordinary links only.
//! - T2 and T3 pass only [`read_only`] requests. Until an authorized job exists (design 13.5
//!   "Writes on a real ECU"), T2 is held to the T3 list. A RawMode link is refused on T1, T2
//!   and T3 (`link::create`), since a worker passes raw data through and a native ISO 15765 channel
//!   segments it itself, so the guard cannot tell what a raw payload sends; no frame decoder
//!   exists here.
//!
//! [`READ_ONLY_SERVICES`] of the agent's `policy` module is the production job runner's list of
//! services (ReadDTCInformation and ReadDataByIdentifier). This list is wider on purpose: a
//! hardware check on T3 may own a session (design 13.5's class table allows the default and the
//! extended session, and tester present), and it is narrower in another way: it also looks at the
//! sub-function and the length of the request. The two are kept apart (ADR-333 item 3).
//!
//! [`READ_ONLY_SERVICES`]: agent::policy::READ_ONLY_SERVICES

use vci_service_interface::{
    ComOperationType, ComPrimitiveCtrlData, StartComPrimitiveRequest, TxFlagBit,
    com_primitive_ctrl_data,
};

use super::{BoundLink, Context, Grant, GuardRefusal};
use crate::profile::{Addressing, Class, Link};

/// The ReadDTCInformation report types ISO 14229-1 (2026) 11.3 defines.
fn defined_dtc_report(sub: u8) -> bool {
    matches!(sub, 0x01..=0x0E | 0x14..=0x1A | 0x42 | 0x55 | 0x56)
}

/// The session a DiagnosticSessionControl request enters, when it is not the default session.
fn session_of(request: &[u8]) -> Option<u8> {
    match request {
        [0x10, sub, ..] if sub & 0x7F != 0x01 => Some(sub & 0x7F),
        _ => None,
    }
}

fn granted(request: &[u8]) -> Grant {
    session_of(request).map_or(Grant::Pass, Grant::EntersSession)
}

/// Whether a request is on the read-only list of T2 and T3: the first byte is the service and
/// the second, with the suppress-response bit cleared, the sub-function. `head` holds the
/// request, or for a segmented message its leading bytes; `total` is the length of the whole
/// message. Everything not listed is refused, including an empty request, a service this list
/// does not know, a response and a length the service does not have:
///
/// - DiagnosticSessionControl (0x10), the default or extended session, two bytes
/// - ReadDataByIdentifier (0x22), one or more two-byte identifiers
/// - ReadDTCInformation (0x19), a defined report type, at least two bytes
/// - TesterPresent (0x3E), sub-function zero, two bytes
///
/// Returns the session the request enters, if it enters one.
pub(super) fn read_only(
    class: Class,
    head: &[u8],
    total: usize,
) -> Result<Option<u8>, GuardRefusal> {
    let Some(&sid) = head.first() else {
        return Err(GuardRefusal::Undecodable("an empty UDS request"));
    };
    let sub = head.get(1).map(|b| b & 0x7F);
    let allowed = match sid {
        0x10 => total == 2 && matches!(sub, Some(0x01 | 0x03)),
        0x22 => total >= 3 && total % 2 == 1,
        0x19 => total >= 2 && sub.is_some_and(defined_dtc_report),
        0x3E => total == 2 && sub == Some(0x00),
        _ => false,
    };
    if allowed {
        Ok(session_of(head))
    } else {
        Err(GuardRefusal::Request { class, sid, sub })
    }
}

/// A tester-present message set as a ComParam (`CP_TesterPresentMessage`): on T2 and T3 a
/// TesterPresent request only, since the worker repeats it. T0 sends no ECU-addressed message.
pub(super) fn check_tester_present(class: Class, message: &[u8]) -> Result<(), GuardRefusal> {
    match class {
        Class::T0 => Err(GuardRefusal::Frame(
            "T0 sends no ECU-addressed request, so it has no tester-present message",
        )),
        // The worker sends the message cyclically, so only a TesterPresent request is allowed:
        // anything else (session control included) would repeat with no session grant.
        // The ECU is simulated on T1, but a cyclic session change would need teardown state.
        Class::T1 | Class::T2 | Class::T3 => match message {
            [0x3E, 0x00 | 0x80] => Ok(()),
            [] => Err(GuardRefusal::Undecodable("an empty tester-present message")),
            _ => Err(GuardRefusal::Request {
                class,
                sid: message[0],
                sub: message.get(1).map(|b| b & 0x7F),
            }),
        },
    }
}

/// The named TxFlag bits (`TxFlagBit`) as the four-byte flag array of ISO 22900-2 (2022) D.2.1:
/// bit value `v` is bit `(v - 1) % 8` of byte `(v - 1) / 8`.
fn tx_flag_bytes(ctrl: &ComPrimitiveCtrlData) -> Result<[u8; 4], GuardRefusal> {
    const NAMED: [TxFlagBit; 6] = [
        TxFlagBit::TxFlagSuppressPosResp,
        TxFlagBit::TxFlagEnableExtraInfo,
        TxFlagBit::TxFlagWaitP3MinOnly,
        TxFlagBit::TxFlagCan29bitId,
        TxFlagBit::TxFlagIso15765AddrType,
        TxFlagBit::TxFlagIso15765FramePad,
    ];
    let mut bytes = [0u8; 4];
    let mut named_mask = [0u8; 4];
    for bit in NAMED {
        let v = bit as usize - 1;
        named_mask[v / 8] |= 1 << (v % 8);
    }
    match &ctrl.tx_flag {
        None => {}
        Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(flag)) => {
            for &value in &flag.bits {
                if !NAMED.iter().any(|&bit| bit as i32 == value) {
                    return Err(GuardRefusal::Frame("a TxFlag bit that is not a named one"));
                }
                let v = value as usize - 1;
                bytes[v / 8] |= 1 << (v % 8);
            }
        }
        Some(com_primitive_ctrl_data::TxFlag::TxFlagRaw(raw)) => {
            let Ok(raw) = <[u8; 4]>::try_from(raw.as_slice()) else {
                return Err(GuardRefusal::Frame("a raw TxFlag that is not four bytes"));
            };
            if raw.iter().zip(named_mask).any(|(b, mask)| b & !mask != 0) {
                return Err(GuardRefusal::Frame(
                    "a raw TxFlag with a bit that is not named",
                ));
            }
            bytes = raw;
        }
    }
    Ok(bytes)
}

/// The three TxFlag bits that describe the frame (the CAN identifier width, extended
/// addressing and frame padding) must equal the profile's link, set or not: on a RawMode link
/// the worker takes the frame's format from the flags alone.
fn flags_agree(flags: [u8; 4], link: &Link) -> Result<(), GuardRefusal> {
    let wide = flags[2] & 0x01 != 0;
    if wide != (link.id_bits == 29) {
        return Err(GuardRefusal::Frame(
            "the CAN_29BIT_ID flag does not match the profile's identifier width",
        ));
    }
    let addressed = flags[3] & 0x80 != 0;
    if addressed != (link.addressing != Addressing::Normal) {
        return Err(GuardRefusal::Frame(
            "the ISO15765_ADDR_TYPE flag does not match the profile's addressing",
        ));
    }
    let padded = flags[3] & 0x40 != 0;
    if padded != link.padding.is_some() {
        return Err(GuardRefusal::Frame(
            "the ISO15765_FRAME_PAD flag does not match the profile's padding",
        ));
    }
    Ok(())
}

/// TxFlag bits that would override what the profile says, whatever the link's mode: frame
/// padding when the profile has none (the J2534 worker turns padding on from the flag, apart
/// from the ComParam), the 29-bit identifier flag on an 11-bit profile and the extended
/// addressing flag on normal addressing. A flag that is not set overrides nothing here.
fn flags_override(flags: [u8; 4], link: &Link) -> Result<(), GuardRefusal> {
    if flags[3] & 0x40 != 0 && link.padding.is_none() {
        return Err(GuardRefusal::Frame(
            "the ISO15765_FRAME_PAD flag asks for padding the profile does not have",
        ));
    }
    if flags[2] & 0x01 != 0 && link.id_bits != 29 {
        return Err(GuardRefusal::Frame(
            "the CAN_29BIT_ID flag on a profile with 11-bit identifiers",
        ));
    }
    if flags[3] & 0x80 != 0 && link.addressing == Addressing::Normal {
        return Err(GuardRefusal::Frame(
            "the ISO15765_ADDR_TYPE flag on a profile with normal addressing",
        ));
    }
    Ok(())
}

/// StartComPrimitive on a bound link.
pub(super) fn start_com_primitive(
    ctx: &Context<'_>,
    bound: &BoundLink,
    link: &Link,
    request: &StartComPrimitiveRequest,
) -> Result<Grant, GuardRefusal> {
    let sends = |data: &[u8]| {
        let flags = match &request.cop_ctrl_data {
            Some(ctrl) => tx_flag_bytes(ctrl)?,
            None => [0; 4],
        };
        flags_override(flags, link)?;
        if data.is_empty() {
            Ok(Grant::Pass)
        } else {
            payload(ctx.profile.class, bound, link, flags, data)
        }
    };
    match ComOperationType::try_from(request.cop_type) {
        // The start and stop communication messages, if the primitive carries one, are
        // requests like any other.
        // T0 has no communication session: the J2534 preset would arm a cyclic tester-present
        // frame at the start, an ECU-addressed message.
        Ok(ComOperationType::CoptStartcomm | ComOperationType::CoptStopcomm)
            if ctx.profile.class == Class::T0 =>
        {
            Err(GuardRefusal::Frame(
                "T0 has no communication to start or stop; it exchanges raw frames only",
            ))
        }
        Ok(ComOperationType::CoptStartcomm | ComOperationType::CoptStopcomm) => {
            sends(&request.cop_data)
        }
        Ok(ComOperationType::CoptUpdateparam | ComOperationType::CoptDelay) => Ok(Grant::Pass),
        Ok(ComOperationType::CoptSendrecv) => {
            // One send is a one-shot, several (or -1, until cancelled) a periodic message,
            // and none a primitive that only receives; the message is read the same way.
            let cycles = request
                .cop_ctrl_data
                .as_ref()
                .map_or(0, |c| c.num_send_cycles);
            if cycles < -1 {
                return Err(GuardRefusal::Undecodable("an unsupported num_send_cycles"));
            }
            if request.cop_data.is_empty() && cycles != 0 {
                return Err(GuardRefusal::Undecodable("a send without a request"));
            }
            sends(&request.cop_data)
        }
        // It would put back the ComParams the guard compared with the profile.
        Ok(ComOperationType::CoptRestoreParam) => Err(GuardRefusal::LinkSetup(
            "COPT_RESTORE_PARAM would replace the ComParams the profile fixed".to_owned(),
        )),
        Ok(ComOperationType::CoptUnspecified) | Err(_) => Err(GuardRefusal::Undecodable(
            "a ComPrimitive type that is not specified",
        )),
    }
}

/// A non-empty payload of StartComPrimitive, by the class.
fn payload(
    class: Class,
    bound: &BoundLink,
    link: &Link,
    flags: [u8; 4],
    data: &[u8],
) -> Result<Grant, GuardRefusal> {
    // A RawMode frame is sent as its flags say: they must be the profile's, in both directions.
    if bound.raw_mode {
        flags_agree(flags, link)?;
    }
    match class {
        Class::T0 => {
            if !bound.raw_mode {
                return Err(GuardRefusal::Frame(
                    "T0 sends no ECU-addressed request; only raw frames between the endpoints",
                ));
            }
            // The whole header, as the worker passes it on: the 4-byte identifier, then the
            // address byte under extended or mixed addressing, then the payload. That is the
            // literal frame data of both workers; the payload is UDS data that the ISO 15765
            // channel segments, so it holds no transport PCI, and it is not read here.
            let id = frame_id(data)?;
            if id != link.request_id {
                return Err(GuardRefusal::Frame(
                    "T0 frames go only to the sending endpoint's request identifier",
                ));
            }
            if link.addressing != Addressing::Normal {
                match (data.get(4), link.address) {
                    (Some(&ae), Some(address)) if ae == address => {}
                    (None, _) => {
                        return Err(GuardRefusal::Frame("a raw frame shorter than its header"));
                    }
                    _ => {
                        return Err(GuardRefusal::Frame(
                            "the address byte is not the endpoint's profile address",
                        ));
                    }
                }
            }
            Ok(Grant::Pass)
        }
        // Not reachable through `create`, which refuses a RawMode link outside T0; a raw link
        // found in the context anyway is refused, never decoded.
        Class::T1 | Class::T2 | Class::T3 if bound.raw_mode => Err(GuardRefusal::Frame(
            "a RawMode link outside T0: its data is passed through and cannot be classified",
        )),
        // The ECU is simulated; its requests are not read.
        Class::T1 => Ok(granted(data)),
        Class::T2 | Class::T3 => {
            read_only(class, data, data.len()).map(|s| s.map_or(Grant::Pass, Grant::EntersSession))
        }
    }
}

/// The CAN identifier at the start of a RawMode payload (ISO 22900-2 (2022) D.2.1, Table D.4:
/// four bytes, most significant first).
fn frame_id(data: &[u8]) -> Result<u32, GuardRefusal> {
    match data.first_chunk::<4>() {
        Some(id) => Ok(u32::from_be_bytes(*id)),
        None => Err(GuardRefusal::Frame(
            "a raw frame shorter than its identifier",
        )),
    }
}
