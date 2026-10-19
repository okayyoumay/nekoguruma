//! The hardware profile: a TOML file outside the repository that describes the attached setup
//! (design 13.5, ADR-275 item 3). `docs/hardware-profile.md` describes its keys.
//!
//! [`parse`] checks a profile against its class before any check runs: it may only narrow what
//! the class allows, except for the three additions design 13.5 names (reprogramming of an
//! expendable ECU on T2, and power commands and hardware controls on T1 and T2).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// The environment variable that names the profile file.
pub const PROFILE_VAR: &str = "NGR_HW_PROFILE";

/// A target class (design 13.5). The classes are not ordered: none stands in for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
pub enum Class {
    /// T0: a loopback between two CAN endpoints, no ECU.
    T0,
    /// T1: `sim-ecu` on a real CAN bus.
    T1,
    /// T2: a bench ECU.
    T2,
    /// T3: a vehicle.
    T3,
}

/// An operation beyond the reads of a class that a check needs and a profile may allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Operation {
    /// A write of a configuration value.
    ConfigurationWrite,
    /// A routine.
    Routine,
    /// Reprogramming.
    Reprogramming,
    /// Fault injection on the stand-in ECU.
    FaultInjection,
    /// Switching the power supply off and on with the profile's commands.
    PowerSwitching,
    /// The hardware control with this name, as the profile lists it.
    HardwareControl(&'static str),
}

impl Operation {
    /// The name a profile's `allow` list uses, for the operations a class can allow by itself.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "configuration-write" => Self::ConfigurationWrite,
            "routine" => Self::Routine,
            "reprogramming" => Self::Reprogramming,
            "fault-injection" => Self::FaultInjection,
            _ => return None,
        })
    }
}

impl Class {
    /// The operations beyond reads the class allows by itself, before a profile narrows them.
    /// On T2 they are allowed only inside an authorized job, which the guard enforces, and
    /// reprogramming only on an expendable ECU, which is a profile addition.
    pub fn operations(self) -> &'static [Operation] {
        match self {
            Class::T0 | Class::T3 => &[],
            Class::T1 => &[
                Operation::ConfigurationWrite,
                Operation::Routine,
                Operation::Reprogramming,
                Operation::FaultInjection,
            ],
            Class::T2 => &[Operation::ConfigurationWrite, Operation::Routine],
        }
    }
}

/// The API of a VCI's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Api {
    #[serde(rename = "j2534-0404")]
    J2534V0404,
    #[serde(rename = "iso22900")]
    Iso22900,
}

/// The VCI a link runs on (design 13.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vci {
    pub api: Api,
    /// The library name, resolved as in design 7.1.
    pub library: String,
    /// The device name or serial number the library reports for the module.
    pub module: String,
    /// J2534 only: the label of the configured `modules` entry the check's worker is started
    /// with (ADR-281).
    pub label: Option<String>,
    /// J2534 only: that entry's device name.
    pub pname: Option<String>,
    /// The firmware version the device reports, which a check compares after opening.
    pub firmware_version: String,
    /// The library version the device reports.
    pub library_version: String,
}

impl Vci {
    /// Whether `other` may name the same module. Names are compared exactly, as the library
    /// resolvers match them (design 7.1). A J2534 worker opens the configured entry its label
    /// selects, and its `pname` identifies the device (ADR-281), so for J2534 the same label or
    /// the same `pname` is the same module, whatever `module` says; for the D-PDU API, the module
    /// the library reports is.
    pub fn identifies_same_module(&self, other: &Vci) -> bool {
        self.api == other.api
            && self.library == other.library
            && match self.api {
                Api::J2534V0404 => {
                    (self.label.is_some() && self.label == other.label)
                        || (self.pname.is_some() && self.pname == other.pname)
                }
                Api::Iso22900 => self.module == other.module,
            }
    }
}

/// How a link addresses its ECU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Addressing {
    Normal,
    Extended,
    Mixed,
}

/// A link's physical layer, addressing and transport, as an IR `Protocol` declares them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub protocol: String,
    pub baud_rate: u32,
    /// The connector pins, CAN high first.
    pub pins: [u8; 2],
    pub request_id: u32,
    pub response_id: u32,
    pub functional_id: Option<u32>,
    /// The width of the CAN identifiers: 11 or 29.
    pub id_bits: u8,
    pub addressing: Addressing,
    /// The address byte of extended or mixed addressing.
    pub address: Option<u8>,
    /// The padding byte, or none for no padding.
    pub padding: Option<u8>,
    /// The transport ComParams by name, such as `CP_BlockSize`.
    #[serde(default)]
    pub comparams: BTreeMap<String, i64>,
}

/// One end of a T0 loopback: a VCI channel or a SocketCAN interface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub vci: Option<Vci>,
    /// The VCI's channel, such as the protocol name of an SAE J2534-2 clause 7 additional
    /// channel. Required at both ends when both are channels of the same VCI, and then
    /// different.
    pub channel: Option<String>,
    /// A SocketCAN interface name, such as `can0`.
    pub socketcan: Option<String>,
    pub link: Link,
}

/// The commands that switch the power supply off and on, each a program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Power {
    pub off: Vec<String>,
    pub on: Vec<String>,
}

/// A hardware control the wiring is safe for, with its complete parameters (design 13.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HardwareControl {
    /// Programming voltage on one pin.
    ProgrammingVoltage {
        name: String,
        pin: u8,
        millivolts: u32,
    },
    /// A pin change.
    PinSwitch { name: String, pins: Vec<u8> },
    /// A vendor IOCTL with its exact payload, or the range of values its payload may hold.
    Ioctl {
        name: String,
        id: u32,
        payload: Option<Vec<u8>>,
        range: Option<[i64; 2]>,
    },
}

impl HardwareControl {
    pub fn name(&self) -> &str {
        match self {
            HardwareControl::ProgrammingVoltage { name, .. }
            | HardwareControl::PinSwitch { name, .. }
            | HardwareControl::Ioctl { name, .. } => name,
        }
    }
}

/// The file as written, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    class: Class,
    vci: Option<Vci>,
    link: Option<Link>,
    #[serde(default, rename = "endpoint")]
    endpoints: Vec<Endpoint>,
    allow: Option<Vec<String>>,
    #[serde(default)]
    expendable_ecu: bool,
    power: Option<Power>,
    #[serde(default, rename = "hardware-control")]
    hardware_controls: Vec<HardwareControl>,
    #[serde(default)]
    report_full_identifiers: bool,
}

/// The attached setup: one link, or a T0 profile's two endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a profile is read once per check, so its size does not matter"
)]
pub enum Setup {
    Link { vci: Vci, link: Link },
    Loopback([Endpoint; 2]),
}

/// A checked hardware profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub class: Class,
    pub setup: Setup,
    /// The operations beyond reads the profile allows: its class's, narrowed by `allow`, and
    /// reprogramming of an expendable ECU on T2.
    pub operations: Vec<Operation>,
    pub power: Option<Power>,
    pub hardware_controls: Vec<HardwareControl>,
    /// The profile asks for VINs and serial numbers in full in the run's report (design 13.5
    /// "Reports"); without it they are masked. Off unless the profile says `true`.
    pub report_full_identifiers: bool,
}

impl Profile {
    /// Whether the profile allows `operation`.
    pub fn allows(&self, operation: Operation) -> bool {
        match operation {
            Operation::PowerSwitching => self.power.is_some(),
            Operation::HardwareControl(name) => {
                self.hardware_controls.iter().any(|c| c.name() == name)
            }
            _ => self.operations.contains(&operation),
        }
    }
}

/// Why a profile is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("{PROFILE_VAR} is not set: a hardware check needs a hardware profile (design 13.5)")]
    NotSet,
    #[error("cannot read the hardware profile {path}: {reason}")]
    Read { path: String, reason: String },
    #[error("the hardware profile is not valid TOML for a profile: {0}")]
    Syntax(String),
    #[error("{0}")]
    Invalid(String),
    /// The profile allows more than its class (design 13.5).
    #[error("a {class:?} profile cannot add {what}")]
    Adds { class: Class, what: String },
}

fn invalid(message: impl Into<String>) -> ProfileError {
    ProfileError::Invalid(message.into())
}

/// Reads and checks the profile `NGR_HW_PROFILE` names.
pub fn from_env() -> Result<Profile, ProfileError> {
    let path = std::env::var_os(PROFILE_VAR).ok_or(ProfileError::NotSet)?;
    from_path(Path::new(&path))
}

/// Reads and checks the profile at `path`.
pub fn from_path(path: &Path) -> Result<Profile, ProfileError> {
    let text = std::fs::read_to_string(path).map_err(|e| ProfileError::Read {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    parse(&text)
}

/// Checks a profile's text.
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    let raw: RawProfile =
        toml::from_str(text).map_err(|e| ProfileError::Syntax(e.message().to_owned()))?;
    let class = raw.class;
    let adds = |what: &str| ProfileError::Adds {
        class,
        what: what.to_owned(),
    };

    let setup = match (class, raw.vci, raw.link, raw.endpoints.len()) {
        (Class::T0, None, None, 2) => {
            let [a, b]: [Endpoint; 2] = raw.endpoints.try_into().expect("two endpoints");
            for endpoint in [&a, &b] {
                check_endpoint(endpoint)?;
            }
            check_loopback(&a, &b)?;
            Setup::Loopback([a, b])
        }
        (Class::T0, ..) => {
            return Err(invalid(
                "a T0 profile names exactly two `endpoint` tables and no `vci` or `link`",
            ));
        }
        (_, Some(vci), Some(link), 0) => {
            check_vci(&vci)?;
            check_link(&link)?;
            Setup::Link { vci, link }
        }
        _ => {
            return Err(invalid(
                "a T1, T2 or T3 profile names one `vci` and one `link`, and no `endpoint`",
            ));
        }
    };

    // Narrowing: `allow` keeps a subset of the class's operations.
    let mut operations: Vec<Operation> = match raw.allow {
        None => class.operations().to_vec(),
        Some(names) => {
            let mut kept = Vec::new();
            for name in names {
                let operation = Operation::from_name(&name).ok_or_else(|| {
                    invalid(format!("`allow` names an unknown operation {name:?}"))
                })?;
                if !class.operations().contains(&operation) {
                    return Err(adds(&format!("the operation {name:?}")));
                }
                if !kept.contains(&operation) {
                    kept.push(operation);
                }
            }
            kept
        }
    };

    // The three additions of design 13.5.
    if raw.expendable_ecu {
        if class != Class::T2 {
            return Err(adds("an expendable ECU"));
        }
        operations.push(Operation::Reprogramming);
    }
    if let Some(power) = &raw.power {
        if !matches!(class, Class::T1 | Class::T2) {
            return Err(adds("power commands"));
        }
        if [&power.off, &power.on]
            .iter()
            .any(|command| command.first().is_none_or(String::is_empty))
        {
            return Err(invalid("`power` needs both an `off` and an `on` command"));
        }
    }
    if !raw.hardware_controls.is_empty() && !matches!(class, Class::T1 | Class::T2) {
        return Err(adds("hardware controls"));
    }
    for (index, control) in raw.hardware_controls.iter().enumerate() {
        check_control(control)?;
        if raw.hardware_controls[..index]
            .iter()
            .any(|other| other.name() == control.name())
        {
            return Err(invalid(format!(
                "two hardware controls are named {:?}",
                control.name()
            )));
        }
    }

    Ok(Profile {
        class,
        setup,
        operations,
        power: raw.power,
        hardware_controls: raw.hardware_controls,
        report_full_identifiers: raw.report_full_identifiers,
    })
}

/// The pins of the diagnostic connector, numbered 1 to 16 (SAE J2534-1 6.8 lists their use).
const PINS: std::ops::RangeInclusive<u8> = 1..=16;

/// The ComParams of the physical layer, which the profile's typed keys give and a check may not
/// change (design 13.5).
/// The physical-layer ComParams: the bus type class (`PDU_PC_BUSTYPE`, ISO 22900-2 (2022)
/// Annex B.3.2), by the names of `j2534-0404-service`'s `BUSTYPE_UNUM32` and `BUSTYPE_BYTES`
/// (`crates/j2534-0404-service/src/service/comparam_support.rs`), which this list mirrors in
/// full; a name added there belongs here too. Compared ignoring ASCII case.
pub(crate) const PHYSICAL_COMPARAMS: &[&str] = &[
    "CP_Baudrate",
    "CP_BitSamplePoint",
    "CP_BitSamplePoint_Ecu",
    "CP_SamplesPerBit",
    "CP_SamplesPerBit_Ecu",
    "CP_SyncJumpWidth",
    "CP_SyncJumpWidth_Ecu",
    "CP_ListenOnly",
    "CP_TerminationType",
    "CP_TerminationType_Ecu",
    "CP_NetworkLine",
    "CP_K_L_LineInit",
    "CP_K_LinePullup",
    "CP_UartConfig",
    "CP_Parity",
    "CP_CANFDBaudrate",
    "CP_CANFDBitSamplePoint",
    "CP_CANFDSyncJumpWidth",
    "CP_J1850IFRCtrl",
    "CP_Node_Address",
    "CP_AnalogSampleRate",
    "CP_AnalogActiveChannels",
    "CP_AnalogSamplesPerReading",
    "CP_AnalogReadingsPerMsg",
    "CP_AnalogAveragingMethod",
    "CP_CanBaudrateRecord",
];

/// A channel name as the J2534 worker reads it: ASCII case ignored, surrounding spaces dropped,
/// and a `_CH` index read as a number, so `CAN_CH1`, `can_ch1` and `CAN_CH01` are one channel.
pub(crate) fn channel_key(name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase();
    if let Some(index) = upper.rfind("_CH") {
        let (head, digits) = (&upper[..index], &upper[index + 3..]);
        if !head.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let number = digits.trim_start_matches('0');
            return format!("{head}_CH{}", if number.is_empty() { "0" } else { number });
        }
    }
    upper
}

fn check_vci(vci: &Vci) -> Result<(), ProfileError> {
    if [
        &vci.library,
        &vci.module,
        &vci.firmware_version,
        &vci.library_version,
    ]
    .iter()
    .any(|value| value.trim().is_empty())
    {
        return Err(invalid(
            "a `vci` needs its `library`, `module`, `firmware_version` and `library_version`",
        ));
    }
    // The D-PDU API reports its versions as coded numbers, which the module match compares as
    // `major.minor.revision` (ISO 22900-2 (2022) 10.1.4.15).
    if vci.api == Api::Iso22900
        && ![&vci.firmware_version, &vci.library_version]
            .iter()
            .all(|version| crate::device::is_coded_version(version.trim()))
    {
        return Err(invalid(
            "a D-PDU API `vci` writes `firmware_version` and `library_version` as \
             `major.minor.revision`, each 0 to 255 with no leading zero",
        ));
    }
    match vci.api {
        // A J2534 library reports no serial number, so the module is matched through its
        // configured entry (design 13.5). An empty label would list every entry (ADR-281).
        Api::J2534V0404
            if vci.label.as_deref().is_none_or(|s| s.trim().is_empty())
                || vci.pname.as_deref().is_none_or(|s| s.trim().is_empty()) =>
        {
            Err(invalid(
                "a J2534 `vci` names the `label` and `pname` of its configured `modules` entry",
            ))
        }
        // The label a worker gives its default entry when it reads no `modules`: a worker that
        // reads another file than the harness would report it and be taken for the profile's.
        Api::J2534V0404
            if vci
                .label
                .as_deref()
                .is_some_and(|s| s.trim() == "j2534-0404") =>
        {
            Err(invalid(
                "a J2534 `vci`'s `label` is not `j2534-0404`, which a worker without `modules`                  gives its default entry",
            ))
        }
        Api::Iso22900 if vci.label.is_some() || vci.pname.is_some() => Err(invalid(
            "`label` and `pname` name a J2534 `modules` entry; a D-PDU API `vci` has none",
        )),
        _ => Ok(()),
    }
}

fn check_link(link: &Link) -> Result<(), ProfileError> {
    if link.protocol.is_empty() || link.baud_rate == 0 {
        return Err(invalid(
            "a `link` needs its `protocol` and a `baud_rate` above 0",
        ));
    }
    if link.pins[0] == link.pins[1] || !link.pins.iter().all(|pin| PINS.contains(pin)) {
        return Err(invalid(
            "a `link`'s two `pins` must differ and be connector pins 1 to 16",
        ));
    }
    let max = match link.id_bits {
        11 => 0x7FF,
        29 => 0x1FFF_FFFF,
        _ => return Err(invalid("`id_bits` is 11 or 29")),
    };
    for id in [
        Some(link.request_id),
        Some(link.response_id),
        link.functional_id,
    ]
    .into_iter()
    .flatten()
    {
        if id > max {
            return Err(invalid(format!(
                "the CAN identifier {id:#X} does not fit {} bits",
                link.id_bits
            )));
        }
    }
    if link.request_id == link.response_id
        || link
            .functional_id
            .is_some_and(|id| id == link.request_id || id == link.response_id)
    {
        return Err(invalid(
            "`request_id`, `response_id` and `functional_id` must differ",
        ));
    }
    match (link.addressing, link.address) {
        (Addressing::Normal, None) | (Addressing::Extended | Addressing::Mixed, Some(_)) => {}
        (Addressing::Normal, Some(_)) => {
            return Err(invalid("normal addressing takes no `address`"));
        }
        (_, None) => {
            return Err(invalid("extended and mixed addressing need an `address`"));
        }
    }
    if let Some(name) = link
        .comparams
        .keys()
        .find(|name| name.strip_prefix("CP_").is_none_or(str::is_empty))
    {
        return Err(invalid(format!(
            "the ComParam {name:?} is not a ComParam name (`CP_...`)"
        )));
    }
    // Two spellings of one ComParam (an alias, or another case) cannot both be the profile's
    // value for it, whatever the values.
    let keys: Vec<&String> = link.comparams.keys().collect();
    for (i, a) in keys.iter().enumerate() {
        if let Some(b) = keys[..i]
            .iter()
            .find(|b| crate::comparam::canonical(b) == crate::comparam::canonical(a))
        {
            return Err(invalid(format!(
                "the `link.comparams` keys {b:?} and {a:?} name one ComParam"
            )));
        }
    }
    if let Some(name) = link.comparams.keys().find(|name| {
        PHYSICAL_COMPARAMS
            .iter()
            .any(|p| p.eq_ignore_ascii_case(name))
    }) {
        return Err(invalid(format!(
            "the ComParam {name:?} is physical-layer; the `link`'s own keys give it"
        )));
    }
    Ok(())
}

fn check_endpoint(endpoint: &Endpoint) -> Result<(), ProfileError> {
    match (&endpoint.vci, &endpoint.socketcan) {
        (Some(vci), None)
            if endpoint
                .channel
                .as_deref()
                .is_none_or(|c| !c.trim().is_empty()) =>
        {
            check_vci(vci)?
        }
        (None, Some(interface)) if !interface.is_empty() && endpoint.channel.is_none() => {}
        _ => {
            return Err(invalid(
                "an `endpoint` names either a `vci`, optionally with its `channel`, or a \
                 `socketcan` interface",
            ));
        }
    }
    check_link(&endpoint.link)
}

/// A T0 loopback runs between two different ends, at least one a VCI channel, on one bus: the
/// same rate and identifier width, each end sending what the other receives (design 13.5).
fn check_loopback(a: &Endpoint, b: &Endpoint) -> Result<(), ProfileError> {
    if a.vci.is_none() && b.vci.is_none() {
        return Err(invalid(
            "a T0 loopback has a VCI channel at one end at least",
        ));
    }
    // Two channels of one VCI are the preferred loopback; they differ by their channel. The
    // module is one device whatever else its two descriptions say, so they must agree.
    let same_module = match (&a.vci, &b.vci) {
        (Some(x), Some(y)) if x.identifies_same_module(y) => {
            if x != y {
                return Err(invalid(
                    "a T0 loopback's two ends name one VCI module but describe it differently",
                ));
            }
            true
        }
        _ => false,
    };
    let same_vci_channel = same_module
        && match (&a.channel, &b.channel) {
            (Some(x), Some(y)) => channel_key(x) == channel_key(y),
            _ => true,
        };
    if same_vci_channel {
        return Err(invalid(
            "a T0 loopback's two endpoints must be different channels; two ends on one VCI name \
             different `channel`s",
        ));
    }
    let (x, y) = (&a.link, &b.link);
    if x.baud_rate != y.baud_rate || x.id_bits != y.id_bits {
        return Err(invalid(
            "a T0 loopback's endpoints share one bus: the same `baud_rate` and `id_bits`",
        ));
    }
    if x.request_id != y.response_id || x.response_id != y.request_id {
        return Err(invalid(
            "each T0 endpoint receives on the identifier the other sends on",
        ));
    }
    Ok(())
}

fn check_control(control: &HardwareControl) -> Result<(), ProfileError> {
    if control.name().is_empty() {
        return Err(invalid("a hardware control needs a `name`"));
    }
    match control {
        HardwareControl::ProgrammingVoltage { pin, .. } if !PINS.contains(pin) => Err(invalid(
            "a `programming-voltage` control's `pin` is a connector pin 1 to 16",
        )),
        HardwareControl::ProgrammingVoltage { .. } => Ok(()),
        HardwareControl::PinSwitch { pins, .. }
            if pins.is_empty()
                || !pins.iter().all(|pin| PINS.contains(pin))
                || pins
                    .iter()
                    .enumerate()
                    .any(|(i, pin)| pins[..i].contains(pin)) =>
        {
            Err(invalid(
                "a `pin-switch` control names its `pins`, each a connector pin 1 to 16, once",
            ))
        }
        HardwareControl::PinSwitch { .. } => Ok(()),
        HardwareControl::Ioctl { payload, range, .. } => match (payload, range) {
            (Some(_), None) => Ok(()),
            (None, Some([low, high])) if low <= high => Ok(()),
            (None, Some(_)) => Err(invalid("an `ioctl` control's `range` is low, then high")),
            _ => Err(invalid(
                "an `ioctl` control names either its exact `payload` or its safe `range`",
            )),
        },
    }
}
