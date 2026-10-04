use std::{ffi::CStr, fmt, os::raw::c_char};

use j2534_0404_sys::bindings::{self, STATUS_NOERROR};
use j2534_0404_sys::libloading;

/// Status value returned by native J2534 calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatusCode(pub u32);

impl StatusCode {
    /// Success status code.
    pub const NO_ERROR: Self = Self(STATUS_NOERROR);

    /// Returns the raw numeric status value.
    pub fn as_u32(self) -> u32 {
        self.0
    }

    /// Returns `true` if this code is `STATUS_NOERROR`.
    pub fn is_no_error(self) -> bool {
        self.0 == STATUS_NOERROR
    }

    /// Returns `true` if this code indicates an empty receive buffer.
    pub fn is_buffer_empty(self) -> bool {
        self.0 == bindings::ERR_BUFFER_EMPTY
    }
}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.0 {
            STATUS_NOERROR => "STATUS_NOERROR",
            bindings::ERR_NOT_SUPPORTED => "ERR_NOT_SUPPORTED",
            bindings::ERR_INVALID_CHANNEL_ID => "ERR_INVALID_CHANNEL_ID",
            bindings::ERR_INVALID_PROTOCOL_ID => "ERR_INVALID_PROTOCOL_ID",
            bindings::ERR_NULL_PARAMETER => "ERR_NULL_PARAMETER",
            bindings::ERR_INVALID_IOCTL_VALUE => "ERR_INVALID_IOCTL_VALUE",
            bindings::ERR_INVALID_FLAGS => "ERR_INVALID_FLAGS",
            bindings::ERR_FAILED => "ERR_FAILED",
            bindings::ERR_DEVICE_NOT_CONNECTED => "ERR_DEVICE_NOT_CONNECTED",
            bindings::ERR_TIMEOUT => "ERR_TIMEOUT",
            bindings::ERR_INVALID_MSG => "ERR_INVALID_MSG",
            bindings::ERR_INVALID_TIME_INTERVAL => "ERR_INVALID_TIME_INTERVAL",
            bindings::ERR_EXCEEDED_LIMIT => "ERR_EXCEEDED_LIMIT",
            bindings::ERR_DEVICE_IN_USE => "ERR_DEVICE_IN_USE",
            bindings::ERR_INVALID_IOCTL_ID => "ERR_INVALID_IOCTL_ID",
            bindings::ERR_INVALID_MSG_ID => "ERR_INVALID_MSG_ID",
            bindings::ERR_BUFFER_EMPTY => "ERR_BUFFER_EMPTY",
            bindings::ERR_BUFFER_FULL => "ERR_BUFFER_FULL",
            bindings::ERR_BUFFER_OVERFLOW => "ERR_BUFFER_OVERFLOW",
            bindings::ERR_PIN_INVALID => "ERR_PIN_INVALID",
            bindings::ERR_CHANNEL_IN_USE => "ERR_CHANNEL_IN_USE",
            bindings::ERR_MSG_PROTOCOL_ID => "ERR_MSG_PROTOCOL_ID",
            bindings::ERR_INVALID_FILTER_ID => "ERR_INVALID_FILTER_ID",
            bindings::ERR_NO_FLOW_CONTROL => "ERR_NO_FLOW_CONTROL",
            bindings::ERR_NOT_UNIQUE => "ERR_NOT_UNIQUE",
            bindings::ERR_INVALID_BAUDRATE => "ERR_INVALID_BAUDRATE",
            bindings::ERR_INVALID_DEVICE_ID => "ERR_INVALID_DEVICE_ID",
            bindings::ERR_PIN_IN_USE => "ERR_PIN_IN_USE",
            bindings::ERR_VOLTAGE_IN_USE => "ERR_VOLTAGE_IN_USE",
            bindings::ERR_ADDRESS_NOT_CLAIMED => "ERR_ADDRESS_NOT_CLAIMED",
            bindings::ERR_NO_CONNECTION_ESTABLISHED => "ERR_NO_CONNECTION_ESTABLISHED",
            bindings::ERR_RESOURCE_IN_USE => "ERR_RESOURCE_IN_USE",
            bindings::ERR_INVALID_IOCTL_PARAM_ID => "ERR_INVALID_IOCTL_PARAM_ID",
            _ => "UNKNOWN_STATUS",
        };
        write!(f, "{name} ({:#010x})", self.0)
    }
}

/// Error type for high-level API operations.
#[derive(Debug)]
pub enum Error {
    /// The vendor shared library failed to load.
    LibraryLoad(libloading::Error),
    /// Native API returned a failing status code.
    ///
    /// `description` is the text from `PassThruGetLastError`, fetched
    /// automatically when the failing call is made through
    /// `J2534Api0404::check`. It is `None` when the failing call was
    /// `PassThruGetLastError` itself (to avoid recursing), or when fetching
    /// the description failed or returned an empty string.
    ApiStatus {
        code: StatusCode,
        description: Option<String>,
    },
    /// Caller-provided message data exceeded PASSTHRU_MSG capacity.
    MessageTooLarge { len: usize, max: usize },
    /// Caller supplied an invalid argument.
    InvalidArgument {
        name: &'static str,
        reason: &'static str,
    },
    /// C API returned invalid UTF-8 or malformed text.
    InvalidVersionString,
    /// Operation is unavailable for this binding mode.
    OperationNotAvailable { operation: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibraryLoad(err) => write!(f, "failed to load J2534 library: {err}"),
            Self::ApiStatus {
                code,
                description: Some(desc),
            } => {
                write!(f, "J2534 call failed: {code} ({desc})")
            }
            Self::ApiStatus {
                code,
                description: None,
            } => write!(f, "J2534 call failed: {code}"),
            Self::MessageTooLarge { len, max } => {
                write!(
                    f,
                    "message payload size {len} exceeds PASSTHRU_MSG capacity {max}"
                )
            }
            Self::InvalidArgument { name, reason } => {
                write!(f, "invalid argument {name}: {reason}")
            }
            Self::InvalidVersionString => write!(f, "invalid version string returned by API"),
            Self::OperationNotAvailable { operation } => {
                write!(f, "operation requires open handle: {operation}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Firmware/DLL/API version triplet reported by the device API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub firmware: String,
    pub dll: String,
    pub api: String,
}

/// Checks a raw status code, without fetching `PassThruGetLastError` text.
///
/// Used directly by `last_error_text()` itself (to avoid recursing into
/// `PassThruGetLastError` on its own failure); other call sites should go
/// through `J2534Api0404::check`, which fetches the description too.
pub(crate) fn check(status: ::std::os::raw::c_long) -> Result<(), Error> {
    if status as u32 == STATUS_NOERROR {
        Ok(())
    } else {
        Err(Error::ApiStatus {
            code: StatusCode(status as u32),
            description: None,
        })
    }
}

pub(crate) fn c_buf_to_string(buffer: &[c_char]) -> Result<String, Error> {
    let cstr = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    cstr.to_str()
        .map(|s| s.to_string())
        .map_err(|_| Error::InvalidVersionString)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_status_display_includes_description_when_present() {
        let err = Error::ApiStatus {
            code: StatusCode(bindings::ERR_FAILED),
            description: Some("device unplugged".to_string()),
        };
        assert_eq!(
            err.to_string(),
            "J2534 call failed: ERR_FAILED (0x00000007) (device unplugged)"
        );
    }

    #[test]
    fn api_status_display_omits_parens_when_description_absent() {
        let err = Error::ApiStatus {
            code: StatusCode(bindings::ERR_FAILED),
            description: None,
        };
        assert_eq!(
            err.to_string(),
            "J2534 call failed: ERR_FAILED (0x00000007)"
        );
    }
}
