//! StartComPrimitive: the UDS request a payload carries, directly or inside a raw CAN frame, and
//! the lists each class holds it to (design 13.5 "Guarded VCI access", second and third bullet;
//! ADR-333 item 3).
//!
//! - T0 sends no ECU-addressed request: only raw frames to the sending endpoint's own request
//!   identifier pass, and their content is not read.
//! - T1 passes every request (the ECU is simulated); the frame is not decoded.
//! - T2 and T3 pass only [`read_only`] requests. Until an authorized job exists (design 13.5
//!   "Writes on a real ECU"), T2 is held to the T3 list.
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

/// A tester-present message set as a ComParam (`CP_TesterPresentMessage`): the class's rule for
/// a request. T0 sends no ECU-addressed message.
pub(super) fn check_tester_present(class: Class, message: &[u8]) -> Result<(), GuardRefusal> {
    match class {
        Class::T0 => Err(GuardRefusal::Frame(
            "T0 sends no ECU-addressed request, so it has no tester-present message",
        )),
        Class::T1 => Ok(()),
        Class::T2 | Class::T3 => read_only(class, message, message.len()).map(|_| ()),
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

/// The two TxFlag bits that describe the frame (the CAN identifier width and extended
/// addressing) must agree with the profile's link.
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
        if data.is_empty() {
            Ok(Grant::Pass)
        } else {
            payload(ctx.profile.class, bound, link, request, data)
        }
    };
    match ComOperationType::try_from(request.cop_type) {
        // The start and stop communication messages, if the primitive carries one, are
        // requests like any other.
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
    request: &StartComPrimitiveRequest,
    data: &[u8],
) -> Result<Grant, GuardRefusal> {
    let flags = match &request.cop_ctrl_data {
        Some(ctrl) => tx_flag_bytes(ctrl)?,
        None => [0; 4],
    };
    match class {
        Class::T0 => {
            if !bound.raw_mode {
                return Err(GuardRefusal::Frame(
                    "T0 sends no ECU-addressed request; only raw frames between the endpoints",
                ));
            }
            let id = frame_id(data)?;
            if id != link.request_id {
                return Err(GuardRefusal::Frame(
                    "T0 frames go only to the sending endpoint's request identifier",
                ));
            }
            flags_agree(flags, link)?;
            Ok(Grant::Pass)
        }
        // The ECU is simulated; its requests are not read.
        Class::T1 => Ok(if bound.raw_mode {
            Grant::Pass
        } else {
            granted(data)
        }),
        Class::T2 | Class::T3 if !bound.raw_mode => {
            read_only(class, data, data.len()).map(|s| s.map_or(Grant::Pass, Grant::EntersSession))
        }
        Class::T2 | Class::T3 => {
            flags_agree(flags, link)?;
            raw_frame(class, link, data)
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

/// A raw frame on T2 or T3: `[4-byte CAN id][address byte, for extended or mixed
/// addressing][PCI and data]`. Only the profile's request identifiers pass, the address byte
/// must be the profile's, and the transport PCI byte (ISO 15765-2 (2024) 9.6) says what is
/// carried:
///
/// - a single frame is decoded, including the escape length of 9.6.2;
/// - a first frame is decoded from its leading bytes and its declared length;
/// - a flow control frame carries no request and passes;
/// - a consecutive frame is refused, since what it continues is not known here;
/// - any other PCI type, and a frame too short for its PCI, is refused.
fn raw_frame(class: Class, link: &Link, data: &[u8]) -> Result<Grant, GuardRefusal> {
    let id = frame_id(data)?;
    if id != link.request_id && Some(id) != link.functional_id {
        return Err(GuardRefusal::Frame(
            "the frame does not go to the profile's request identifiers",
        ));
    }
    let mut at = 4;
    if link.addressing != Addressing::Normal {
        match data.get(at) {
            Some(&ae) if Some(ae) == link.address => at += 1,
            _ => {
                return Err(GuardRefusal::Frame(
                    "the address byte is not the profile's address",
                ));
            }
        }
    }
    let Some(&pci) = data.get(at) else {
        return Err(GuardRefusal::Frame("a frame without a PCI byte"));
    };
    let low = usize::from(pci & 0x0F);
    let session = match pci >> 4 {
        0x0 => {
            let (len, start) = if low != 0 {
                if low > 7 {
                    return Err(GuardRefusal::Frame("a single frame length above 7"));
                }
                (low, at + 1)
            } else {
                match data.get(at + 1) {
                    Some(&len) => (usize::from(len), at + 2),
                    None => return Err(GuardRefusal::Frame("a single frame without its length")),
                }
            };
            let Some(body) = start.checked_add(len).and_then(|end| data.get(start..end)) else {
                return Err(GuardRefusal::Frame(
                    "a single frame shorter than its length",
                ));
            };
            read_only(class, body, len)?
        }
        0x1 => {
            let Some(&second) = data.get(at + 1) else {
                return Err(GuardRefusal::Frame("a first frame without its length"));
            };
            let short = (low << 8) | usize::from(second);
            let (total, start) = if short != 0 {
                (short, at + 2)
            } else {
                match data.get(at + 2..at + 6) {
                    Some(long) => {
                        let long = u32::from_be_bytes([long[0], long[1], long[2], long[3]]);
                        (long as usize, at + 6)
                    }
                    None => return Err(GuardRefusal::Frame("a first frame without its length")),
                }
            };
            let head = &data[start.min(data.len())..];
            if head.len() < 2 || total <= head.len() {
                return Err(GuardRefusal::Frame(
                    "a first frame whose length does not fit its bytes",
                ));
            }
            read_only(class, head, total)?
        }
        0x2 => {
            return Err(GuardRefusal::Frame(
                "a consecutive frame is not decoded: its request began in an earlier frame",
            ));
        }
        0x3 => None,
        _ => {
            return Err(GuardRefusal::Frame(
                "a PCI type that is not a transport frame",
            ));
        }
    };
    Ok(session.map_or(Grant::Pass, Grant::EntersSession))
}
