//! VCI simulator. A cdylib exposing the J2534 PassThru API.
//! The worker actually loads it via dlopen / LoadLibrary, so CI runs without real hardware (13.4).
//!
//! Important: this crate is also the target for verifying "differences in J2534 calling conventions and type widths".
//! Round-trips against this mock confirm whether the ABI interpretation table in 7.1.2 is correct.
//!
//! Build examples:
//!   cargo build -p sim-vci --target i686-pc-windows-msvc    (win-x86)
//!   cargo build -p sim-vci --target x86_64-unknown-linux-gnu (linux-x86_64)

use std::os::raw::{c_char, c_void};

/// J2534 `unsigned long`.
/// Windows is LLP64, so it is 32-bit even on 64-bit; Linux / AArch64 are LP64, so it is 64-bit.
/// Corresponds to the table in 7.1.2 (ARM is inferred).
#[cfg(windows)]
pub type PassThruUlong = u32;
#[cfg(not(windows))]
pub type PassThruUlong = std::os::raw::c_ulong;

/// Calling convention. stdcall on Windows x86 only, standard C elsewhere.
#[cfg(all(windows, target_arch = "x86"))]
macro_rules! passthru_abi {
    () => {
        "stdcall"
    };
}
#[cfg(not(all(windows, target_arch = "x86")))]
macro_rules! passthru_abi {
    () => {
        "C"
    };
}

use j2534_defs::consts::status;

pub const STATUS_NOERROR: PassThruUlong = status::STATUS_NOERROR as PassThruUlong;
pub const ERR_DEVICE_NOT_CONNECTED: PassThruUlong =
    status::ERR_DEVICE_NOT_CONNECTED as PassThruUlong;
pub const ERR_TIMEOUT: PassThruUlong = status::ERR_TIMEOUT as PassThruUlong;

/// `PASSTHRU_MSG`. Packing is made explicit (7.1.1).
#[repr(C)]
pub struct PassThruMsg {
    pub protocol_id: PassThruUlong,
    pub rx_status: PassThruUlong,
    pub tx_flags: PassThruUlong,
    pub timestamp: PassThruUlong,
    pub data_size: PassThruUlong,
    pub extra_data_index: PassThruUlong,
    pub data: [u8; 4128],
}

// ---------------------------------------------------------------- Exports

/// Side-effect-free function the worker calls right after loading (7.3).
/// If the ABI interpretation is wrong, this surfaces as garbage values or a crash.
#[unsafe(no_mangle)]
pub extern "C" fn PassThruReadVersion(
    _device_id: PassThruUlong,
    _firmware_version: *mut c_char,
    _dll_version: *mut c_char,
    _api_version: *mut c_char,
) -> PassThruUlong {
    // TODO: write fixed strings (the caller's buffers are 80 bytes).
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruOpen(
    _name: *const c_void,
    _device_id: *mut PassThruUlong,
) -> PassThruUlong {
    // TODO: make multiple opens configurable (for verifying 9.3 "whether multiple devices can be opened simultaneously").
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruClose(_device_id: PassThruUlong) -> PassThruUlong {
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruConnect(
    _device_id: PassThruUlong,
    _protocol_id: PassThruUlong,
    _flags: PassThruUlong,
    _baud_rate: PassThruUlong,
    _channel_id: *mut PassThruUlong,
) -> PassThruUlong {
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruDisconnect(_channel_id: PassThruUlong) -> PassThruUlong {
    STATUS_NOERROR
}

/// Blocking read (one of the 3 modes in 10.2).
#[unsafe(no_mangle)]
pub extern "C" fn PassThruReadMsgs(
    _channel_id: PassThruUlong,
    _msgs: *mut PassThruMsg,
    _num_msgs: *mut PassThruUlong,
    _timeout: PassThruUlong,
) -> PassThruUlong {
    // TODO: delegate to sim-ecu and apply the configured delay / no-response.
    ERR_TIMEOUT
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruWriteMsgs(
    _channel_id: PassThruUlong,
    _msgs: *const PassThruMsg,
    _num_msgs: *mut PassThruUlong,
    _timeout: PassThruUlong,
) -> PassThruUlong {
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruIoctl(
    _handle_id: PassThruUlong,
    _ioctl_id: PassThruUlong,
    _input: *mut c_void,
    _output: *mut c_void,
) -> PassThruUlong {
    // TODO: return READ_VBATT (voltage). Used for the precondition tests in 8.9.1.
    STATUS_NOERROR
}
