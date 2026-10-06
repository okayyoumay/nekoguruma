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
#[expect(unused_macros, reason = "the exports do not use it yet")]
macro_rules! passthru_abi {
    () => {
        "stdcall"
    };
}
#[cfg(not(all(windows, target_arch = "x86")))]
#[expect(unused_macros, reason = "the exports do not use it yet")]
macro_rules! passthru_abi {
    () => {
        "C"
    };
}

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use j2534_defs::consts::{connect_flag, protocol, status};
use sim_ecu::{Addressing, EcuConfig, SimEcu};

pub const STATUS_NOERROR: PassThruUlong = status::STATUS_NOERROR as PassThruUlong;
pub const ERR_NOT_SUPPORTED: PassThruUlong = status::ERR_NOT_SUPPORTED as PassThruUlong;
pub const ERR_INVALID_CHANNEL_ID: PassThruUlong = status::ERR_INVALID_CHANNEL_ID as PassThruUlong;
pub const ERR_NULL_PARAMETER: PassThruUlong = status::ERR_NULL_PARAMETER as PassThruUlong;
pub const ERR_FAILED: PassThruUlong = status::ERR_FAILED as PassThruUlong;
pub const ERR_DEVICE_NOT_CONNECTED: PassThruUlong =
    status::ERR_DEVICE_NOT_CONNECTED as PassThruUlong;
pub const ERR_TIMEOUT: PassThruUlong = status::ERR_TIMEOUT as PassThruUlong;
pub const ERR_INVALID_MSG: PassThruUlong = status::ERR_INVALID_MSG as PassThruUlong;
pub const ERR_BUFFER_EMPTY: PassThruUlong = status::ERR_BUFFER_EMPTY as PassThruUlong;
pub const ERR_MSG_PROTOCOL_ID: PassThruUlong = status::ERR_MSG_PROTOCOL_ID as PassThruUlong;
pub const ERR_INVALID_DEVICE_ID: PassThruUlong = status::ERR_INVALID_DEVICE_ID as PassThruUlong;

/// Size of `PASSTHRU_MSG::data`.
pub const MAX_MSG_DATA: usize = 4128;

/// `PASSTHRU_MSG`. Packing is made explicit (7.1.1).
#[repr(C)]
pub struct PassThruMsg {
    pub protocol_id: PassThruUlong,
    pub rx_status: PassThruUlong,
    pub tx_flags: PassThruUlong,
    pub timestamp: PassThruUlong,
    pub data_size: PassThruUlong,
    pub extra_data_index: PassThruUlong,
    pub data: [u8; MAX_MSG_DATA],
}

// ---------------------------------------------------------------- Simulated bus

/// Environment variable naming a JSON file with the `sim_ecu::EcuConfig` of the simulated ECU.
/// Read on the first `PassThruOpen` of the process; without it a built-in configuration is used.
pub const ECU_CONFIG_ENV: &str = "NGR_SIM_ECU_CONFIG";

/// The device ID `PassThruOpen` returns. The simulator has one device.
pub const DEVICE_ID: PassThruUlong = 1;

/// 11-bit CAN IDs of the simulated ECU (ISO 15765-4 legacy OBD identifiers of the first ECU).
pub const ECU_PHYSICAL_REQUEST_ID: u32 = 0x7E0;
pub const ECU_RESPONSE_ID: u32 = 0x7E8;
pub const FUNCTIONAL_REQUEST_ID: u32 = 0x7DF;

/// Length of the CAN ID at the start of an ISO 15765 message.
const CAN_ID_LEN: usize = 4;

/// A response the ECU has produced, readable from `ready_at` on.
struct Pending {
    ready_at: Instant,
    data: Vec<u8>,
}

struct Channel {
    protocol_id: PassThruUlong,
    /// Ordered by `ready_at`: the order in which the responses appear on the bus.
    rx: VecDeque<Pending>,
}

/// State of the simulated device and the vehicle behind it.
struct Bus {
    /// Created on the first `PassThruOpen` and kept for the life of the process: the vehicle
    /// keeps its state while the device is closed and opened again.
    ecu: Option<SimEcu>,
    device_open: bool,
    channels: HashMap<PassThruUlong, Channel>,
    next_channel: PassThruUlong,
    epoch: Instant,
}

static BUS: OnceLock<(Mutex<Bus>, Condvar)> = OnceLock::new();

fn bus() -> &'static (Mutex<Bus>, Condvar) {
    BUS.get_or_init(|| {
        (
            Mutex::new(Bus {
                ecu: None,
                device_open: false,
                channels: HashMap::new(),
                next_channel: 1,
                epoch: Instant::now(),
            }),
            Condvar::new(),
        )
    })
}

/// A panic in one call must not wedge every later call, so a poisoned lock is taken over.
fn lock() -> MutexGuard<'static, Bus> {
    bus().0.lock().unwrap_or_else(|e| e.into_inner())
}

/// `PassThruUlong` is `u32` on Windows and `u64` elsewhere, so the cast is a no-op on some targets.
#[allow(clippy::unnecessary_cast)]
fn millis(ms: PassThruUlong) -> Duration {
    Duration::from_millis(ms as u64)
}

/// Wakes reads waiting for a response.
fn notify() {
    bus().1.notify_all();
}

fn wait(guard: MutexGuard<'static, Bus>, timeout: Duration) -> MutexGuard<'static, Bus> {
    bus()
        .1
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|e| e.into_inner())
        .0
}

/// Configuration used when `NGR_SIM_ECU_CONFIG` is not set.
fn default_ecu_config() -> EcuConfig {
    EcuConfig {
        vin: "NGRSIMECU00000001".into(),
        part_number: "NGR-SIM-ECU".into(),
        sw_version: "1.0.0".into(),
        ..Default::default()
    }
}

fn load_ecu_config() -> Option<EcuConfig> {
    match std::env::var_os(ECU_CONFIG_ENV) {
        None => Some(default_ecu_config()),
        Some(path) => {
            let text = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&text).ok()
        }
    }
}

impl Bus {
    fn device_ok(&self, device_id: PassThruUlong) -> bool {
        self.device_open && device_id == DEVICE_ID
    }

    /// Microseconds since the library was first used, as J2534 timestamps wrap.
    fn timestamp(&self) -> PassThruUlong {
        self.epoch.elapsed().as_micros() as PassThruUlong
    }

    /// Hands one ISO 15765 message to the ECU and queues its response on `channel_id`.
    fn transmit(&mut self, channel_id: PassThruUlong, data: &[u8]) {
        let (id, payload) = data.split_at(CAN_ID_LEN);
        let addressing = match u32::from_be_bytes([id[0], id[1], id[2], id[3]]) {
            ECU_PHYSICAL_REQUEST_ID => Addressing::Physical,
            FUNCTIONAL_REQUEST_ID => Addressing::Functional,
            // No simulated ECU listens on this ID.
            _ => return,
        };
        let Some(ecu) = self.ecu.as_mut() else {
            return;
        };
        let exchange = ecu.exchange(addressing, payload);
        let Some(bytes) = exchange.response.to_bytes() else {
            return;
        };
        let mut data = ECU_RESPONSE_ID.to_be_bytes().to_vec();
        data.extend_from_slice(&bytes);
        let ready_at = Instant::now() + Duration::from_millis(exchange.delay_ms.into());
        let Some(channel) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let at = channel.rx.partition_point(|p| p.ready_at <= ready_at);
        channel.rx.insert(at, Pending { ready_at, data });
    }
}

/// Runs `f` on the simulated ECU, creating it with `config` if the process has none yet.
/// Lets tests inspect the ECU and inject faults (`sim_ecu::Fault`) between J2534 calls.
#[cfg(test)]
fn with_ecu<R>(f: impl FnOnce(&mut SimEcu) -> R) -> R {
    let mut bus = lock();
    f(bus
        .ecu
        .get_or_insert_with(|| SimEcu::new(default_ecu_config())))
}

/// Replaces the simulated vehicle with a fresh ECU and closes the device.
#[cfg(test)]
fn reset(config: EcuConfig) {
    let mut bus = lock();
    bus.ecu = Some(SimEcu::new(config));
    bus.device_open = false;
    bus.channels.clear();
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

/// Opens the simulated device and, on the first open of the process, powers up the ECU with the
/// configuration from `NGR_SIM_ECU_CONFIG`.
///
/// # Safety
/// `device_id` must be null or valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn PassThruOpen(
    _name: *const c_void,
    device_id: *mut PassThruUlong,
) -> PassThruUlong {
    // TODO: make multiple opens configurable (for verifying 9.3 "whether multiple devices can be opened simultaneously").
    if device_id.is_null() {
        return ERR_NULL_PARAMETER;
    }
    let mut bus = lock();
    if bus.ecu.is_none() {
        let Some(config) = load_ecu_config() else {
            return ERR_FAILED;
        };
        bus.ecu = Some(SimEcu::new(config));
    }
    bus.device_open = true;
    // SAFETY: checked for null above; the caller guarantees it is writable.
    unsafe { device_id.write(DEVICE_ID) };
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruClose(device_id: PassThruUlong) -> PassThruUlong {
    let mut bus = lock();
    if !bus.device_ok(device_id) {
        return ERR_INVALID_DEVICE_ID;
    }
    bus.device_open = false;
    bus.channels.clear();
    notify();
    STATUS_NOERROR
}

/// Connects an ISO 15765 channel with 11-bit CAN IDs, the only kind the simulated ECU speaks.
///
/// # Safety
/// `channel_id` must be null or valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn PassThruConnect(
    device_id: PassThruUlong,
    protocol_id: PassThruUlong,
    flags: PassThruUlong,
    _baud_rate: PassThruUlong,
    channel_id: *mut PassThruUlong,
) -> PassThruUlong {
    let mut bus = lock();
    if !bus.device_ok(device_id) {
        return ERR_INVALID_DEVICE_ID;
    }
    if channel_id.is_null() {
        return ERR_NULL_PARAMETER;
    }
    if protocol_id != protocol::PROTOCOL_ISO15765 as PassThruUlong
        || flags & connect_flag::CONNECT_FLAG_CAN_29BIT_ID as PassThruUlong != 0
    {
        return ERR_NOT_SUPPORTED;
    }
    let id = bus.next_channel;
    bus.next_channel += 1;
    bus.channels.insert(
        id,
        Channel {
            protocol_id,
            rx: VecDeque::new(),
        },
    );
    // SAFETY: checked for null above; the caller guarantees it is writable.
    unsafe { channel_id.write(id) };
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "C" fn PassThruDisconnect(channel_id: PassThruUlong) -> PassThruUlong {
    let mut bus = lock();
    if bus.channels.remove(&channel_id).is_none() {
        return ERR_INVALID_CHANNEL_ID;
    }
    notify();
    STATUS_NOERROR
}

/// Blocking read (one of the 3 modes in 10.2), following J2534-1 7.2.5: returns once
/// `*num_msgs` responses are read or `timeout` ms have passed; `timeout == 0` returns at once.
///
/// # Safety
/// `num_msgs` must be null or valid for reads and writes, and `msgs` null or valid for writes of
/// `*num_msgs` messages.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn PassThruReadMsgs(
    channel_id: PassThruUlong,
    msgs: *mut PassThruMsg,
    num_msgs: *mut PassThruUlong,
    timeout: PassThruUlong,
) -> PassThruUlong {
    if msgs.is_null() || num_msgs.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees it is valid.
    let wanted = unsafe { num_msgs.read() } as usize;
    // A timeout too large for `Instant` (e.g. `ULONG_MAX` on LP64) waits for a year instead of
    // panicking across the FFI boundary.
    let now = Instant::now();
    let deadline = now
        .checked_add(millis(timeout))
        .unwrap_or(now + Duration::from_secs(365 * 24 * 3600));
    let mut bus = lock();
    let mut read = 0;
    let status = loop {
        let timestamp = bus.timestamp();
        let Some(channel) = bus.channels.get_mut(&channel_id) else {
            break ERR_INVALID_CHANNEL_ID;
        };
        let now = Instant::now();
        while read < wanted && channel.rx.front().is_some_and(|p| p.ready_at <= now) {
            let pending = channel.rx.pop_front().expect("front checked above");
            let mut msg = PassThruMsg {
                protocol_id: channel.protocol_id,
                rx_status: 0,
                tx_flags: 0,
                timestamp,
                data_size: pending.data.len() as PassThruUlong,
                extra_data_index: pending.data.len() as PassThruUlong,
                data: [0; MAX_MSG_DATA],
            };
            msg.data[..pending.data.len()].copy_from_slice(&pending.data);
            // SAFETY: `read < wanted`, and the caller guarantees room for `wanted` messages.
            unsafe { msgs.add(read).write(msg) };
            read += 1;
        }
        if read == wanted {
            break STATUS_NOERROR;
        }
        if now >= deadline {
            break match (read, timeout) {
                (0, _) => ERR_BUFFER_EMPTY,
                (_, 0) => STATUS_NOERROR,
                _ => ERR_TIMEOUT,
            };
        }
        let wake = channel
            .rx
            .front()
            .map_or(deadline, |p| p.ready_at.min(deadline));
        bus = wait(bus, wake.saturating_duration_since(now));
    };
    // SAFETY: as above.
    unsafe { num_msgs.write(read as PassThruUlong) };
    status
}

/// Sends each message to the simulated ECU. The ECU handles the request at once; its response
/// becomes readable after the delay it reports. No TxDone indications or loopback messages
/// are generated.
///
/// # Safety
/// `num_msgs` must be null or valid for reads and writes, and `msgs` null or valid for reads of
/// `*num_msgs` messages.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn PassThruWriteMsgs(
    channel_id: PassThruUlong,
    msgs: *const PassThruMsg,
    num_msgs: *mut PassThruUlong,
    _timeout: PassThruUlong,
) -> PassThruUlong {
    if msgs.is_null() || num_msgs.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees it is valid.
    let wanted = unsafe { num_msgs.read() } as usize;
    let mut bus = lock();
    let Some(protocol_id) = bus.channels.get(&channel_id).map(|c| c.protocol_id) else {
        return ERR_INVALID_CHANNEL_ID;
    };
    let mut sent = 0;
    let status = loop {
        if sent == wanted {
            break STATUS_NOERROR;
        }
        // SAFETY: `sent < wanted`, and the caller guarantees `wanted` readable messages.
        let msg = unsafe { &*msgs.add(sent) };
        if msg.protocol_id != protocol_id {
            break ERR_MSG_PROTOCOL_ID;
        }
        let size = msg.data_size as usize;
        if !(CAN_ID_LEN + 1..=MAX_MSG_DATA).contains(&size) {
            break ERR_INVALID_MSG;
        }
        bus.transmit(channel_id, &msg.data[..size]);
        sent += 1;
    };
    // SAFETY: as above.
    unsafe { num_msgs.write(sent as PassThruUlong) };
    if sent > 0 {
        notify();
    }
    status
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

#[cfg(test)]
mod tests;
