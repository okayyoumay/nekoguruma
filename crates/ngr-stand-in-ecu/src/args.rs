//! The command line of `ngr-stand-in-ecu` and the CAN identifiers it names.

use std::path::PathBuf;

/// Bit 31 of a SocketCAN identifier: the identifier has 29 bits instead of 11.
const CAN_EFF_FLAG: u32 = 0x8000_0000;
/// Largest 11-bit and 29-bit CAN identifier.
const MAX_STANDARD_ID: u32 = 0x7FF;
const MAX_EXTENDED_ID: u32 = 0x1FFF_FFFF;

/// Default physical request ID, response ID and functional request ID: the 11-bit IDs `sim-vci`
/// also uses for its simulated ECU.
const DEFAULT_REQUEST_ID: u32 = 0x7E0;
const DEFAULT_RESPONSE_ID: u32 = 0x7E8;
const DEFAULT_FUNCTIONAL_ID: u32 = 0x7DF;

/// The usage text.
pub const USAGE: &str = "\
Usage: ngr-stand-in-ecu --interface <ifname> [options]

Serves a simulated ECU (sim-ecu) over an ISO-TP socket of a Linux SocketCAN interface.

Options:
  --interface <ifname>   CAN interface to serve on, for example vcan0 (required)
  --request-id <id>      physical request CAN ID (default 0x7E0)
  --response-id <id>     CAN ID of the responses (default 0x7E8)
  --functional-id <id>   functional request CAN ID (default 0x7DF)
  --extended             the three IDs are 29-bit IDs (default: 11-bit)
  --config <file>        JSON EcuConfig of a new ECU (not read when --state names a file)
  --state <file>         file that keeps the ECU's state across runs
  -h, --help             print this text

IDs are decimal, or hexadecimal with a 0x prefix.";

/// The three CAN identifiers of the stand-in ECU as the kernel's ISO-TP sockets take them: a
/// 29-bit identifier carries the extended-frame flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanIds {
    request: u32,
    response: u32,
    functional: u32,
}

impl CanIds {
    /// The identifiers, each checked against the width `extended` selects. The three must differ,
    /// since the ECU could not tell a request from its own response, nor physical addressing from
    /// functional.
    pub fn new(
        request: u32,
        response: u32,
        functional: u32,
        extended: bool,
    ) -> Result<Self, ArgsError> {
        if request == response || request == functional || response == functional {
            return Err(ArgsError::SameId);
        }
        Ok(Self {
            request: socket_id(request, extended)?,
            response: socket_id(response, extended)?,
            functional: socket_id(functional, extended)?,
        })
    }

    /// The physical request ID, in the form the ISO-TP socket takes.
    pub fn request(&self) -> u32 {
        self.request
    }

    /// The response ID, in the form the ISO-TP socket takes.
    pub fn response(&self) -> u32 {
        self.response
    }

    /// The functional request ID, in the form the ISO-TP socket takes.
    pub fn functional(&self) -> u32 {
        self.functional
    }
}

/// `id` as a SocketCAN identifier: itself for an 11-bit one, with the extended-frame flag for a
/// 29-bit one. An identifier too wide for the selected width is refused: the kernel would read
/// its upper bits as flags.
pub fn socket_id(id: u32, extended: bool) -> Result<u32, ArgsError> {
    let (max, bits) = if extended {
        (MAX_EXTENDED_ID, 29)
    } else {
        (MAX_STANDARD_ID, 11)
    };
    if id > max {
        return Err(ArgsError::IdTooWide { id, bits });
    }
    Ok(if extended { id | CAN_EFF_FLAG } else { id })
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub interface: String,
    pub ids: CanIds,
    pub config: Option<PathBuf>,
    pub state: Option<PathBuf>,
}

/// The result of reading the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(Args),
    Help,
}

/// Why the command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("unknown argument {0}")]
    Unknown(String),
    #[error("{0} needs a value")]
    MissingValue(&'static str),
    #[error("{flag}: {value:?} is not a number: {source}")]
    NotANumber {
        flag: &'static str,
        value: String,
        source: std::num::ParseIntError,
    },
    #[error("--interface is required")]
    NoInterface,
    #[error("CAN ID {id:#X} does not fit {bits} bits")]
    IdTooWide { id: u32, bits: u8 },
    #[error("the request, response and functional IDs must differ")]
    SameId,
    #[error("{0} given twice")]
    Repeated(&'static str),
}

/// Reads the arguments after the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, ArgsError> {
    let mut args = args.into_iter();
    let mut interface = None;
    let mut config = None;
    let mut state = None;
    let mut request = None;
    let mut response = None;
    let mut functional = None;
    let mut extended = false;
    while let Some(arg) = args.next() {
        let mut value = |flag: &'static str| args.next().ok_or(ArgsError::MissingValue(flag));
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--extended" => extended = true,
            "--interface" => set_once(&mut interface, "--interface", value("--interface")?)?,
            "--config" => set_once(&mut config, "--config", value("--config")?.into())?,
            "--state" => set_once(&mut state, "--state", value("--state")?.into())?,
            "--request-id" => {
                set_once(
                    &mut request,
                    "--request-id",
                    number("--request-id", value("--request-id")?)?,
                )?;
            }
            "--response-id" => {
                set_once(
                    &mut response,
                    "--response-id",
                    number("--response-id", value("--response-id")?)?,
                )?;
            }
            "--functional-id" => set_once(
                &mut functional,
                "--functional-id",
                number("--functional-id", value("--functional-id")?)?,
            )?,
            _ => return Err(ArgsError::Unknown(arg)),
        }
    }
    let ids = CanIds::new(
        request.unwrap_or(DEFAULT_REQUEST_ID),
        response.unwrap_or(DEFAULT_RESPONSE_ID),
        functional.unwrap_or(DEFAULT_FUNCTIONAL_ID),
        extended,
    )?;
    Ok(Command::Serve(Args {
        interface: interface.ok_or(ArgsError::NoInterface)?,
        ids,
        config,
        state,
    }))
}

fn set_once<T>(slot: &mut Option<T>, flag: &'static str, value: T) -> Result<(), ArgsError> {
    if slot.replace(value).is_some() {
        return Err(ArgsError::Repeated(flag));
    }
    Ok(())
}

fn number(flag: &'static str, text: String) -> Result<u32, ArgsError> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|source| ArgsError::NotANumber {
        flag,
        value: text,
        source,
    })
}

#[cfg(test)]
mod tests;
