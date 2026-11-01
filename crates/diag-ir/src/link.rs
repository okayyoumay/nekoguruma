//! The rules a declared link must meet (design 8.2.4, ADR-324, ADR-406): the one set the agent
//! applies when a job starts and the CSV conversion applies when it resolves a definition, so
//! that a definition the agent would refuse is refused at conversion (ADR-397: both sides call
//! these functions, nothing is copied).
//!
//! Only static rules live here: name lists, ranges and counts. What needs the worker (a
//! ComParam name it does not know, two names it resolves to one id, a value it refuses) or
//! several documents (ADR-395) stays with the agent.

use std::fmt;

use crate::declaration::Protocol;

/// The one protocol the runner supports: UDS on CAN, by its ISO 22900-2 (2022) short name
/// (Annex B.1). Both worker kinds resolve the link's resource from the standard names (ADR-319).
pub const PROTOCOL_UDS_ON_CAN: &str = "ISO_14229_3_on_ISO_15765_2";
/// The protocol names a document may declare UDS on CAN by (ADR-324 item 2): the transport's
/// name the CSV tables use, or the ISO 22900-2 (2022) short name the link names its resource by
/// (Annex B.1, ADR-319).
pub const UDS_ON_CAN_NAMES: [&str; 2] = ["ISO15765", PROTOCOL_UDS_ON_CAN];
/// The application layer a link carries.
pub const UDS_LAYER: &str = "UDS";
/// 11-bit IDs only: a 29-bit ID needs `CP_CanPhysReqFormat` / `CP_CanRespUSDTFormat`, which
/// the link does not set yet.
pub const MAX_CAN_ID: u32 = 0x7FF;
/// Longest timing the worker's microsecond ComParams can hold, in milliseconds.
pub const MAX_TIMING_MS: u32 = u32::MAX / 1_000;
/// [`MAX_TIMING_MS`] in microseconds, the unit a declared timing ComParam is given in.
pub const MAX_TIMING_US: u32 = MAX_TIMING_MS * 1_000;
/// The ComParams the link takes the host's timings from, which a document declares in
/// microseconds ([`Timing`]).
pub const TIMING_COMPARAMS: [&str; 3] = ["CP_P2Max", "CP_P2Star", "CP_RC78CompletionTimeout"];
/// The ComParams the link sets itself (the agent's link set-up): from the protocol row, or for
/// its response pending handling, indications and addressing. A further one would override it.
pub const SET_BY_LINK: [&str; 14] = [
    "CP_Baudrate",
    "CP_P2Max",
    "CP_P2Star",
    "CP_RC78Handling",
    "CP_RCByteOffset",
    "CP_RC78CompletionTimeout",
    "CP_RC21Handling",
    "CP_RC23Handling",
    "CP_TransmitIndEnable",
    "CP_StartMsgIndEnable",
    "CP_CanPhysReqId",
    "CP_CanRespUSDTId",
    "CP_RequestAddrMode",
    "CP_CanFuncReqId",
];
/// ComParams that let a primitive run longer than the host's send-receive ceiling allows:
/// the limits and request times of repeating the request on a busy (0x21) or not-yet-complete
/// (0x23) answer, which the link turns off (`CP_RC21Handling`, `CP_RC23Handling`), repeating it
/// at the application or transport layer, and taking P2 and P2* from the ECU's session response.
pub const OUTLAST_THE_HOST: [&str; 7] = [
    "CP_RC21CompletionTimeout",
    "CP_RC21RequestTime",
    "CP_RC23CompletionTimeout",
    "CP_RC23RequestTime",
    "CP_RepeatReqCountApp",
    "CP_RepeatReqCountTrans",
    "CP_ModifyTiming",
];

/// A timing ComParam the host's deadline is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    P2Max,
    P2Star,
    Rc78Completion,
}

impl Timing {
    /// The ComParam's name as the standard spells it.
    pub const fn comparam(self) -> &'static str {
        match self {
            Self::P2Max => TIMING_COMPARAMS[0],
            Self::P2Star => TIMING_COMPARAMS[1],
            Self::Rc78Completion => TIMING_COMPARAMS[2],
        }
    }
}

/// Which timing `name` is, ignoring ASCII case, or `None` for any other ComParam.
pub fn timing_of(name: &str) -> Option<Timing> {
    [Timing::P2Max, Timing::P2Star, Timing::Rc78Completion]
        .into_iter()
        .find(|timing| name.eq_ignore_ascii_case(timing.comparam()))
}

/// A protocol as a document declares it, borrowed from either a FlatBuffers reader or the
/// frontend's typed rows.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredProtocol<'a> {
    pub short_name: Option<&'a str>,
    pub application_layer: Option<&'a str>,
    pub baud_rate: u32,
    pub tx_id: u32,
    pub rx_id: u32,
    pub functional_id: u32,
    pub extended_addressing: bool,
    pub params: Vec<DeclaredComParam<'a>>,
}

/// A ComParam as a document declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredComParam<'a> {
    pub short_name: Option<&'a str>,
    pub value: i64,
    pub unit_id: u32,
}

impl<'a> From<Protocol<'a>> for DeclaredProtocol<'a> {
    fn from(protocol: Protocol<'a>) -> Self {
        Self {
            short_name: protocol.short_name(),
            application_layer: protocol.application_layer(),
            baud_rate: protocol.baud_rate(),
            tx_id: protocol.tx_id(),
            rx_id: protocol.rx_id(),
            functional_id: protocol.functional_id(),
            extended_addressing: protocol.extended_addressing(),
            params: protocol
                .params()
                .into_iter()
                .flatten()
                .map(|param| DeclaredComParam {
                    short_name: param.short_name(),
                    value: param.value(),
                    unit_id: param.unit_id(),
                })
                .collect(),
        }
    }
}

/// What the link takes from an accepted protocol: the IR's own units (microseconds) and no
/// defaults; the agent rounds the timings up and fills the defaults in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredLink {
    pub baud_rate: u32,
    pub tx_id: u32,
    pub rx_id: u32,
    pub p2_max_us: Option<u32>,
    pub p2_star_us: Option<u32>,
    pub rc78_completion_us: Option<u32>,
    /// The further ComParams, in declaration order.
    pub params: Vec<(String, u32)>,
}

/// Which identifier of a protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identifier {
    Tx,
    Rx,
    Functional,
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tx => "transmit",
            Self::Rx => "receive",
            Self::Functional => "functional",
        })
    }
}

/// The refusal of an identifier wider than 11 bits ([`MAX_CAN_ID`]), or `None`. The one bound
/// check: [`check_protocol`] and the identification plan's whole-plan refusal of a functional
/// identifier (ADR-395) both call it.
pub fn wide_identifier(which: Identifier, id: u32) -> Option<LinkRefusal> {
    (id > MAX_CAN_ID).then_some(LinkRefusal::WideIdentifier { which, id })
}

/// Why a protocol cannot be the link of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRefusal {
    NoProtocol,
    SeveralProtocols,
    /// The protocol is not UDS on CAN: its name is not one of [`UDS_ON_CAN_NAMES`] or its
    /// layer is not [`UDS_LAYER`] (absent ones are empty here).
    NotUdsOnCan {
        name: String,
        layer: String,
    },
    ExtendedAddressing,
    WideIdentifier {
        which: Identifier,
        id: u32,
    },
    ZeroBaudRate,
    /// The ComParam at index `at` of the protocol's list.
    ComParam {
        at: usize,
        name: String,
        why: ComParamRefusal,
    },
}

/// Why a ComParam cannot be set on the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComParamRefusal {
    /// The same name (ignoring ASCII case) at the earlier index.
    DeclaredTwice {
        earlier: usize,
    },
    HasUnit,
    NotUnsigned32(i64),
    /// A timing, in microseconds, outside `1..=`[`MAX_TIMING_US`].
    TimingOutOfRange(i64),
    SetByLink,
    OutlastsHost,
    NotStandardName,
}

impl fmt::Display for ComParamRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeclaredTwice { .. } => f.write_str("is declared twice"),
            Self::HasUnit => {
                f.write_str("has a unit; give it in the ComParam's own unit, without one")
            }
            Self::NotUnsigned32(value) => write!(f, "is {value}, not a 32-bit unsigned value"),
            Self::TimingOutOfRange(value) => write!(
                f,
                "is {value} us; P2, P2* and the 0x78 completion timeout must be 1 ms to \
                 {MAX_TIMING_MS} ms"
            ),
            Self::SetByLink => f.write_str("is one the link sets itself"),
            Self::OutlastsHost => {
                f.write_str("lets a primitive outlast the host's deadline (ADR-324 item 3)")
            }
            Self::NotStandardName => f.write_str("must have a standard name, starting with CP_"),
        }
    }
}

impl fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProtocol => f.write_str("the variant declares no protocol"),
            Self::SeveralProtocols => f.write_str(
                "the variant declares several protocols; choosing one is not supported yet",
            ),
            Self::NotUdsOnCan { name, layer } => {
                write!(f, "protocol {name:?} carrying {layer:?} is not UDS on CAN")
            }
            Self::ExtendedAddressing => {
                f.write_str("extended addressing is not supported: the link does not set it")
            }
            Self::WideIdentifier { which, id } => write!(
                f,
                "only 11-bit CAN IDs are supported: the link does not set the ID format yet \
                 ({which} identifier {id:#x})"
            ),
            Self::ZeroBaudRate => f.write_str("the baud rate must not be zero"),
            Self::ComParam { name, why, .. } => write!(f, "ComParam {name} {why}"),
        }
    }
}

/// The one protocol of a variant (ADR-324 item 1): the link is chosen by the document, and no
/// document gives a rule to choose among several.
pub fn only_protocol<T>(protocols: &[T]) -> Result<&T, LinkRefusal> {
    match protocols {
        [only] => Ok(only),
        [] => Err(LinkRefusal::NoProtocol),
        _ => Err(LinkRefusal::SeveralProtocols),
    }
}

/// Whether a ComParam other than a timing may be set on the link: it is not one the link sets
/// itself, does not let a primitive outlast the host, and has a standard name (starts with
/// `CP_`, ignoring ASCII case; the J2534 worker also resolves names of its own, which could name
/// what the link sets under another name). Names compare ignoring ASCII case, as the workers
/// resolve them.
pub fn further_comparam(name: &str) -> Result<(), ComParamRefusal> {
    let named = |list: &[&str]| list.iter().any(|n| n.eq_ignore_ascii_case(name));
    if named(&SET_BY_LINK) {
        return Err(ComParamRefusal::SetByLink);
    }
    if named(&OUTLAST_THE_HOST) {
        return Err(ComParamRefusal::OutlastsHost);
    }
    // A byte prefix, so that a multi-byte character in the name is not a panic.
    let standard = name
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"CP_"));
    if standard {
        Ok(())
    } else {
        Err(ComParamRefusal::NotStandardName)
    }
}

/// Every refusal of one protocol: the protocol-level ones first (name and layer, addressing,
/// identifiers, baud rate), then at most one per offending ComParam in declaration order. `Ok`
/// carries what the link takes. All are listed so that the conversion reports every error of a
/// stage (ADR-295 item 11); the agent reports the first.
pub fn check_protocol(protocol: &DeclaredProtocol<'_>) -> Result<DeclaredLink, Vec<LinkRefusal>> {
    let mut refusals = Vec::new();
    let name = protocol.short_name.unwrap_or_default();
    let layer = protocol.application_layer.unwrap_or_default();
    if !UDS_ON_CAN_NAMES.contains(&name) || layer != UDS_LAYER {
        refusals.push(LinkRefusal::NotUdsOnCan {
            name: name.to_owned(),
            layer: layer.to_owned(),
        });
    }
    if protocol.extended_addressing {
        refusals.push(LinkRefusal::ExtendedAddressing);
    }
    for (which, id) in [
        (Identifier::Tx, protocol.tx_id),
        (Identifier::Rx, protocol.rx_id),
        (Identifier::Functional, protocol.functional_id),
    ] {
        refusals.extend(wide_identifier(which, id));
    }
    if protocol.baud_rate == 0 {
        refusals.push(LinkRefusal::ZeroBaudRate);
    }
    let mut link = DeclaredLink {
        baud_rate: protocol.baud_rate,
        tx_id: protocol.tx_id,
        rx_id: protocol.rx_id,
        p2_max_us: None,
        p2_star_us: None,
        rc78_completion_us: None,
        params: Vec::new(),
    };
    for (at, param) in protocol.params.iter().enumerate() {
        let name = param.short_name.unwrap_or_default();
        let refuse = |why| LinkRefusal::ComParam {
            at,
            name: name.to_owned(),
            why,
        };
        if let Some(earlier) = protocol.params[..at]
            .iter()
            .position(|p| p.short_name.unwrap_or_default().eq_ignore_ascii_case(name))
        {
            refusals.push(refuse(ComParamRefusal::DeclaredTwice { earlier }));
            continue;
        }
        // A unit is an id of the server's unit table, which the agent does not have: a value is
        // taken in the ComParam's own unit (microseconds for a time).
        if param.unit_id != 0 {
            refusals.push(refuse(ComParamRefusal::HasUnit));
            continue;
        }
        let Ok(value) = u32::try_from(param.value) else {
            refusals.push(refuse(ComParamRefusal::NotUnsigned32(param.value)));
            continue;
        };
        // A timing is a timing although the link also sets it (it is in `SET_BY_LINK`): the
        // declared value replaces the default.
        match timing_of(name) {
            Some(timing) => {
                // A zero would be read differently by the worker (it replaces a zero P2 or P2*
                // with its own default and reads a zero 0x78 timeout as no limit) than by the
                // host, which would compute its deadline from zero.
                if value == 0 || value > MAX_TIMING_US {
                    refusals.push(refuse(ComParamRefusal::TimingOutOfRange(param.value)));
                    continue;
                }
                match timing {
                    Timing::P2Max => link.p2_max_us = Some(value),
                    Timing::P2Star => link.p2_star_us = Some(value),
                    Timing::Rc78Completion => link.rc78_completion_us = Some(value),
                }
            }
            None => match further_comparam(name) {
                Ok(()) => link.params.push((name.to_owned(), value)),
                Err(why) => refusals.push(refuse(why)),
            },
        }
    }
    if refusals.is_empty() {
        Ok(link)
    } else {
        Err(refusals)
    }
}

#[cfg(test)]
mod tests;
