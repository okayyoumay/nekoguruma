//! VCI simulator. A cdylib exposing the J2534 PassThru API.
//! The worker actually loads it via dlopen / LoadLibrary, so CI runs without real hardware (13.4).
//!
//! Important: this crate is also the target for verifying "differences in J2534 calling conventions and type widths".
//! Round-trips against this mock confirm whether the ABI interpretation table in 7.1.2 is correct.
//!
//! Build examples:
//!   cargo build -p sim-vci --target i686-pc-windows-msvc    (win-x86)
//!   cargo build -p sim-vci --target x86_64-unknown-linux-gnu (linux-x86_64)

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};

/// J2534 `unsigned long`.
/// Windows is LLP64, so it is 32-bit even on 64-bit; Linux / AArch64 are LP64, so it is 64-bit.
/// Corresponds to the table in 7.1.2 (ARM is inferred).
#[cfg(windows)]
pub type PassThruUlong = u32;
#[cfg(not(windows))]
pub type PassThruUlong = std::os::raw::c_ulong;

// Calling convention (7.1.2): every export is `extern "system"`, which is stdcall (`WINAPI`) on
// Windows x86, as in a vendor DLL, and the standard C convention on every other target.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use j2534_defs::consts::{connect_flag, filter, ioctl, protocol, status, tx_flag};
use serde::{Deserialize, Serialize};
use sim_ecu::{Addressing, EcuConfig, EcuSnapshot, Fault, SimEcu, SystemClock};

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
pub const ERR_INVALID_FILTER_ID: PassThruUlong = status::ERR_INVALID_FILTER_ID as PassThruUlong;
pub const ERR_INVALID_MSG_ID: PassThruUlong = status::ERR_INVALID_MSG_ID as PassThruUlong;
pub const ERR_NO_FLOW_CONTROL: PassThruUlong = status::ERR_NO_FLOW_CONTROL as PassThruUlong;
pub const ERR_NOT_UNIQUE: PassThruUlong = status::ERR_NOT_UNIQUE as PassThruUlong;
pub const ERR_EXCEEDED_LIMIT: PassThruUlong = status::ERR_EXCEEDED_LIMIT as PassThruUlong;
pub const ERR_PIN_INVALID: PassThruUlong = status::ERR_PIN_INVALID as PassThruUlong;

/// `PassThruSetProgrammingVoltage` values (J2534-1 7.2.11).
pub const SHORT_TO_GROUND: PassThruUlong = 0xFFFF_FFFE;
pub const VOLTAGE_OFF: PassThruUlong = 0xFFFF_FFFF;
/// Pins that take a programming voltage; pin 15 can only be shorted to ground.
const PROGRAMMING_PINS: [PassThruUlong; 7] = [0, 6, 9, 11, 12, 13, 14];
const GROUND_PIN: PassThruUlong = 15;

/// Largest 11-bit CAN ID.
const MAX_CAN_ID: u32 = 0x7FF;

/// TxFlags for 29-bit IDs and extended addressing, which the simulated channel does not use.
const UNSUPPORTED_TX_FLAGS: PassThruUlong =
    (tx_flag::TX_FLAG_CAN_29BIT_ID | tx_flag::TX_FLAG_ISO15765_ADDR_TYPE) as PassThruUlong;

/// Filters each channel accepts (J2534-1 7.2.9 asks for at least ten).
pub const MAX_FILTERS: usize = 10;

/// Largest ISO 15765 payload that fits a single frame on classic CAN with normal addressing;
/// longer requests are segmented and need a flow-control filter.
pub const SINGLE_FRAME_MAX: usize = 7;

/// Strings `PassThruReadVersion` reports.
pub const FIRMWARE_VERSION: &str = "NGR-SIM 1.0";
pub const DLL_VERSION: &str = concat!("sim-vci ", env!("CARGO_PKG_VERSION"));
pub const API_VERSION: &str = "04.04";

/// What `PassThruGetLastError` reports: the simulator keeps no error descriptions.
pub const LAST_ERROR_TEXT: &str = "sim-vci: no error description";

/// Battery voltage on pin 16 of the J1962 connector until a test sets another
/// (`Command::SetBatteryVoltage`), in millivolts.
pub const DEFAULT_BATTERY_MV: u32 = 12_000;

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
/// Read when the process first needs the ECU (the first `PassThruOpen` or control command),
/// unless the ECU continues from a state file ([`ECU_STATE_ENV`]); without it a built-in
/// configuration is used.
pub const ECU_CONFIG_ENV: &str = "NGR_SIM_ECU_CONFIG";

/// Environment variable naming a file that keeps the simulated ECU's state across processes
/// (ADR-241). Read when the process first needs the ECU: if the file exists, the ECU continues
/// from it (and `NGR_SIM_ECU_CONFIG` is not read); otherwise the ECU starts fresh. Rewritten
/// after every change to the ECU. Without it the ECU lives only as long as the process.
pub const ECU_STATE_ENV: &str = "NGR_SIM_ECU_STATE";

/// Format of the state file; a file of another version is refused.
const STATE_FILE_VERSION: u32 = 1;

/// The contents of the [`ECU_STATE_ENV`] file (postcard).
#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u32,
    /// Wall-clock time of the write, in milliseconds since the Unix epoch: the time until the
    /// next process reads it counts against the ECU's timers.
    saved_at_ms: u64,
    ecu: EcuSnapshot,
}

fn unix_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis().try_into().unwrap_or(u64::MAX))
}

/// The ECU the state file at `path` holds, with the time since it was written counted against
/// its timers: `Ok(None)` if there is no such file, `Err(())` if it cannot be read (for any other
/// reason than not existing), is not a state file of this version, or holds a state the ECU
/// could not have been in.
fn load_ecu_state(path: &Path) -> Result<Option<SimEcu>, ()> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let state: StateFile = postcard::from_bytes(&bytes).map_err(|_| ())?;
    if state.version != STATE_FILE_VERSION {
        return Err(());
    }
    // A wall clock set back since the write counts as no time passed.
    let elapsed = unix_ms(SystemTime::now()).saturating_sub(state.saved_at_ms);
    SimEcu::restore(
        state.ecu,
        SystemClock::default(),
        Duration::from_millis(elapsed),
    )
    .map(Some)
    .map_err(|_| ())
}

/// Writes `ecu`'s state to `path`, through a temporary file renamed over it, so a process killed
/// while writing leaves the previous state.
fn save_ecu_state(path: &Path, ecu: &mut SimEcu) -> std::io::Result<()> {
    let state = StateFile {
        version: STATE_FILE_VERSION,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    let bytes = postcard::to_allocvec(&state).map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes)?;
    // On Windows another process (a virus scanner, for one) can hold the new file open for a
    // moment, and replacing it then fails; try again a few times before giving up.
    let mut attempts = 0;
    loop {
        match std::fs::rename(&tmp, path) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && attempts < 10 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// The device ID the first `PassThruOpen` returns. The simulator has one device; it gets a new
/// ID only when it is opened again after it was lost (`ERR_DEVICE_NOT_CONNECTED`).
pub const DEVICE_ID: PassThruUlong = 1;

/// 11-bit CAN IDs of the simulated ECU (ISO 15765-4 legacy OBD identifiers of the first ECU).
pub const ECU_PHYSICAL_REQUEST_ID: u32 = 0x7E0;
pub const ECU_RESPONSE_ID: u32 = 0x7E8;
pub const FUNCTIONAL_REQUEST_ID: u32 = 0x7DF;

/// Length of the CAN ID at the start of an ISO 15765 message.
const CAN_ID_LEN: usize = 4;

/// The CAN ID at the start of an ISO 15765 message (at least `CAN_ID_LEN` bytes).
fn can_id_of(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

/// The request ID a simulated responder listens on, where flow control for its segmented
/// responses goes.
fn request_id_of(response_id: u32) -> u32 {
    match response_id {
        ECU_RESPONSE_ID => ECU_PHYSICAL_REQUEST_ID,
        _ => u32::MAX,
    }
}

/// The response ID of the simulated responder listening on `request_id`, if any.
fn response_id_of(request_id: u32) -> Option<u32> {
    match request_id {
        ECU_PHYSICAL_REQUEST_ID => Some(ECU_RESPONSE_ID),
        _ => None,
    }
}

/// A response the ECU has produced, readable from `ready_at` on.
struct Pending {
    ready_at: Instant,
    /// `SimEcu::power_cycles` when the response was produced.
    power_cycle: u64,
    data: Vec<u8>,
    /// Whether the filters let the response in. Decided once it has appeared on the bus, with
    /// the filters of that moment (`Channel::settle`).
    accepted: bool,
}

/// A flow-control filter (J2534-1 7.2.9): receive from `pattern`, send to `flow_control`.
#[derive(Clone, Copy)]
struct FlowControlFilter {
    pattern: u32,
    flow_control: u32,
}

struct Channel {
    protocol_id: PassThruUlong,
    /// Ordered by `ready_at`: the order in which the responses appear on the bus.
    rx: VecDeque<Pending>,
    filters: BTreeMap<PassThruUlong, FlowControlFilter>,
    next_filter: PassThruUlong,
}

impl Channel {
    fn new(protocol_id: PassThruUlong) -> Self {
        Self {
            protocol_id,
            rx: VecDeque::new(),
            filters: BTreeMap::new(),
            next_filter: 1,
        }
    }

    /// Whether a message from `can_id` with `payload_len` bytes after the CAN ID may enter the
    /// receive queue. A single frame needs a filter with that pattern ID. A segmented message
    /// also needs the filter's flow-control ID to be the sender's request ID, since the device
    /// answers its first frame with flow control there (J2534-1 7.2.9, Appendix A); a filter
    /// whose pattern and flow-control IDs are the same receives single frames only.
    fn receives(&self, can_id: u32, payload_len: usize) -> bool {
        self.filters.values().any(|f| {
            f.pattern == can_id
                && (payload_len <= SINGLE_FRAME_MAX
                    || (f.flow_control != f.pattern && f.flow_control == request_id_of(can_id)))
        })
    }

    /// Decides every response that has appeared on the bus by `now` and is not decided yet,
    /// with the current filters, and drops those the filters keep out. Called before anything
    /// reads the queue or changes the filters, so each response is judged by the filters that
    /// were in place when it appeared.
    fn settle(&mut self, now: Instant) {
        let filters = std::mem::take(&mut self.filters);
        let probe = Channel {
            filters,
            ..Channel::new(self.protocol_id)
        };
        self.rx.retain_mut(|p| {
            if p.ready_at > now || p.accepted {
                return true;
            }
            p.accepted = probe.receives(can_id_of(&p.data), p.data.len() - CAN_ID_LEN);
            p.accepted
        });
        self.filters = probe.filters;
    }

    /// Whether a segmented message may be sent to `can_id`: a filter must send to it and
    /// receive from the partner that answers there, since the partner's flow control for the
    /// first frame arrives on the pattern ID (J2534-1 7.2.9, Appendix A). A filter whose pattern
    /// and flow-control IDs are the same serves functional single frames only, and a functional
    /// request is never segmented (ADR-055). For an ID no simulated responder listens on, the
    /// partner is unknown and any distinct pattern is taken.
    fn can_segment_to(&self, can_id: u32) -> bool {
        if can_id == FUNCTIONAL_REQUEST_ID {
            return false;
        }
        self.filters.values().any(|f| {
            f.flow_control == can_id
                && f.pattern != f.flow_control
                && response_id_of(can_id).is_none_or(|response| response == f.pattern)
        })
    }
}

/// State of the simulated device and the vehicle behind it.
struct Bus {
    /// Created on the first `PassThruOpen` and kept for the life of the process: the vehicle
    /// keeps its state while the device is closed and opened again.
    ecu: Option<SimEcu>,
    device_open: bool,
    /// ID of the open device, or the one the next `PassThruOpen` returns.
    device_id: PassThruUlong,
    /// Whether the VCI is plugged in (`Command::DisconnectVci`, `Command::ConnectVci`).
    vci_present: bool,
    /// Set when the VCI goes away while the device is open. Every call then returns
    /// `ERR_DEVICE_NOT_CONNECTED`, even once the VCI is back, until `PassThruClose` releases
    /// the device (J2534-1 6.10.1).
    device_lost: bool,
    channels: HashMap<PassThruUlong, Channel>,
    next_channel: PassThruUlong,
    epoch: Instant,
    /// Pin carrying a programming voltage, if any (J2534-1 7.2.11 allows one at a time).
    programming_pin: Option<PassThruUlong>,
    /// Directory control command files are taken from ([`CONTROL_DIR_ENV`]).
    control_dir: Option<PathBuf>,
    /// Battery voltage `READ_VBATT` reports, in millivolts.
    battery_mv: u32,
    /// File the ECU's state is kept in across processes ([`ECU_STATE_ENV`]).
    state_path: Option<PathBuf>,
}

static BUS: OnceLock<(Mutex<Bus>, Condvar)> = OnceLock::new();

fn bus() -> &'static (Mutex<Bus>, Condvar) {
    BUS.get_or_init(|| {
        (
            Mutex::new(Bus {
                ecu: None,
                device_open: false,
                device_id: DEVICE_ID,
                vci_present: true,
                device_lost: false,
                channels: HashMap::new(),
                next_channel: 1,
                epoch: Instant::now(),
                programming_pin: None,
                control_dir: std::env::var_os(CONTROL_DIR_ENV).map(PathBuf::from),
                battery_mv: DEFAULT_BATTERY_MV,
                state_path: std::env::var_os(ECU_STATE_ENV).map(PathBuf::from),
            }),
            Condvar::new(),
        )
    })
}

/// Locks the bus and applies the pending control command files.
fn lock() -> MutexGuard<'static, Bus> {
    let mut bus = lock_bus();
    bus.apply_control_files();
    bus
}

/// Locks the bus without looking at the control directory. A panic in one call must not wedge
/// every later call, so a poisoned lock is taken over.
fn lock_bus() -> MutexGuard<'static, Bus> {
    bus().0.lock().unwrap_or_else(|e| e.into_inner())
}

/// [`lock`] for a J2534 call: fails with `ERR_DEVICE_NOT_CONNECTED` while the device is lost
/// (J2534-1 6.10.1).
fn lock_connected() -> Result<MutexGuard<'static, Bus>, PassThruUlong> {
    let bus = lock();
    if bus.unreachable() {
        return Err(ERR_DEVICE_NOT_CONNECTED);
    }
    Ok(bus)
}

/// `PassThruUlong` is `u32` on Windows and `u64` elsewhere, so the cast is a no-op on some targets.
#[allow(clippy::unnecessary_cast)]
fn millis(ms: PassThruUlong) -> Duration {
    Duration::from_millis(ms as u64)
}

/// Microseconds from `epoch` (first use of the library) to `at`, the moment a response appeared
/// on the bus. Wraps as J2534 timestamps do.
fn timestamp(epoch: Instant, at: Instant) -> PassThruUlong {
    at.saturating_duration_since(epoch).as_micros() as PassThruUlong
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
        self.device_open && device_id == self.device_id
    }

    /// Whether the open device is lost: every call but `PassThruGetLastError` then fails with
    /// `ERR_DEVICE_NOT_CONNECTED`, and `PassThruClose` on the lost device also releases it. With
    /// no device open, an unplugged VCI fails only `PassThruOpen`, which checks it itself.
    fn unreachable(&self) -> bool {
        self.device_lost
    }

    /// Closes the device: its channels go with their filters and unread responses, and every
    /// programming pin is switched off.
    fn close_device(&mut self) {
        self.device_open = false;
        self.channels.clear();
        self.programming_pin = None;
        notify();
    }

    /// The simulated ECU. If the process has none yet, it continues from the state file
    /// ([`ECU_STATE_ENV`]) when there is one, and is otherwise created with the configuration
    /// from `NGR_SIM_ECU_CONFIG`. `ERR_FAILED` if the state file or the configuration cannot be
    /// read, or the new ECU's state cannot be written.
    fn ecu(&mut self) -> Result<&mut SimEcu, PassThruUlong> {
        if self.ecu.is_none() {
            let restored = match &self.state_path {
                Some(path) => load_ecu_state(path).map_err(|()| ERR_FAILED)?,
                None => None,
            };
            let ecu = match restored {
                Some(ecu) => ecu,
                None => SimEcu::new(load_ecu_config().ok_or(ERR_FAILED)?),
            };
            self.ecu = Some(ecu);
            if let Err(status) = self.save_ecu() {
                // Not created after all: the next call tries again, and fails the same way
                // until the state can be written.
                self.ecu = None;
                return Err(status);
            }
        }
        Ok(self.ecu.as_mut().expect("created above"))
    }

    /// Writes the ECU's state to the state file, if there is one. `ERR_FAILED` if it cannot be
    /// written; the ECU keeps its new state in this process either way.
    fn save_ecu(&mut self) -> Result<(), PassThruUlong> {
        match (&self.state_path, &mut self.ecu) {
            (Some(path), Some(ecu)) => save_ecu_state(path, ecu).map_err(|_| ERR_FAILED),
            _ => Ok(()),
        }
    }

    /// Applies one control command.
    fn apply(&mut self, command: Command) -> Result<(), PassThruUlong> {
        match command {
            Command::InjectFault { fault } => self.change_ecu(|ecu| ecu.inject(fault)),
            Command::ReconnectEcu {} => self.change_ecu(SimEcu::reconnect),
            Command::DisconnectVci {} => {
                self.vci_present = false;
                self.device_lost |= self.device_open;
                // Reads waiting for a response return at once.
                notify();
                Ok(())
            }
            Command::ConnectVci {} => {
                self.vci_present = true;
                Ok(())
            }
            Command::SetBatteryVoltage { millivolts } => {
                self.battery_mv = millivolts;
                Ok(())
            }
        }
    }

    fn change_ecu(&mut self, f: impl FnOnce(&mut SimEcu)) -> Result<(), PassThruUlong> {
        let ecu = self.ecu()?;
        let before = ecu.power_cycles();
        f(ecu);
        self.after_ecu_change(before);
        self.save_ecu()
    }

    /// Applies the `*.json` command files in the control directory, in file-name order. Each
    /// file is claimed by renaming it to `*.applying` first, so a command is applied at most
    /// once even if the file cannot be deleted afterwards; a file that cannot be claimed is
    /// left for a later call, and the files after it wait with it. A file applied is deleted;
    /// one that cannot be read, parsed or applied is renamed to `*.rejected`, so the test can
    /// see it failed.
    fn apply_control_files(&mut self) {
        let Some(dir) = &self.control_dir else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for path in files {
            let claimed = path.with_extension("applying");
            if std::fs::rename(&path, &claimed).is_err() {
                // Later files wait behind it, so commands never run out of order.
                break;
            }
            let applied = std::fs::read_to_string(&claimed)
                .ok()
                .and_then(|text| serde_json::from_str::<Command>(&text).ok())
                .is_some_and(|command| self.apply(command).is_ok());
            if applied {
                let _ = std::fs::remove_file(&claimed);
            } else {
                let _ = std::fs::rename(&claimed, path.with_extension("rejected"));
            }
        }
    }

    fn power_cycles(&self) -> u64 {
        self.ecu.as_ref().map_or(0, SimEcu::power_cycles)
    }

    /// Call after anything that may have power-cycled the ECU, with the count from before it.
    /// Responses still being delayed by then were never sent, so they are discarded; responses
    /// already on the bus stay in the receive buffer.
    fn after_ecu_change(&mut self, power_cycles_before: u64) {
        let current = self.power_cycles();
        if current == power_cycles_before {
            return;
        }
        let now = Instant::now();
        for channel in self.channels.values_mut() {
            channel
                .rx
                .retain(|p| p.ready_at <= now || p.power_cycle == current);
        }
    }

    /// Hands one ISO 15765 message to the ECU and queues its response on `channel_id`, if the
    /// channel has a flow-control filter for the response ID. A segmented request needs a
    /// filter whose flow-control ID is the request's ID (J2534-1 7.2.9); without one it is not
    /// sent and `ERR_NO_FLOW_CONTROL` is returned.
    fn transmit(&mut self, channel_id: PassThruUlong, data: &[u8]) -> Result<(), PassThruUlong> {
        let (id, payload) = data.split_at(CAN_ID_LEN);
        let can_id = can_id_of(id);
        let channel = self
            .channels
            .get(&channel_id)
            .ok_or(ERR_INVALID_CHANNEL_ID)?;
        if payload.len() > SINGLE_FRAME_MAX && !channel.can_segment_to(can_id) {
            return Err(ERR_NO_FLOW_CONTROL);
        }
        let addressing = match can_id {
            ECU_PHYSICAL_REQUEST_ID => Addressing::Physical,
            FUNCTIONAL_REQUEST_ID => Addressing::Functional,
            // No simulated ECU listens on this ID.
            _ => return Ok(()),
        };
        let Some(ecu) = self.ecu.as_mut() else {
            return Ok(());
        };
        let before = ecu.power_cycles();
        let exchange = ecu.exchange(addressing, payload);
        let power_cycle = ecu.power_cycles();
        self.after_ecu_change(before);
        // A request the next process must know about fails if it cannot be recorded; its
        // response is then not sent either.
        self.save_ecu()?;
        let Some(bytes) = exchange.response.to_bytes() else {
            return Ok(());
        };
        let mut data = ECU_RESPONSE_ID.to_be_bytes().to_vec();
        data.extend_from_slice(&bytes);
        let ready_at = Instant::now() + Duration::from_millis(exchange.delay_ms.into());
        let Some(channel) = self.channels.get_mut(&channel_id) else {
            return Ok(());
        };
        let at = channel.rx.partition_point(|p| p.ready_at <= ready_at);
        channel.rx.insert(
            at,
            Pending {
                ready_at,
                power_cycle,
                data,
                accepted: false,
            },
        );
        Ok(())
    }
}

/// Runs `f` on the simulated ECU, creating it with the built-in configuration if the process has
/// none yet.
/// Lets tests inspect the ECU and inject faults (`sim_ecu::Fault`) between J2534 calls.
#[cfg(test)]
fn with_ecu<R>(f: impl FnOnce(&mut SimEcu) -> R) -> R {
    let mut bus = lock();
    let before = bus.power_cycles();
    let result = f(bus
        .ecu
        .get_or_insert_with(|| SimEcu::new(default_ecu_config())));
    bus.after_ecu_change(before);
    result
}

/// Replaces the simulated vehicle with a fresh ECU and closes the device.
#[cfg(test)]
fn reset(config: EcuConfig) {
    let mut bus = lock();
    bus.ecu = Some(SimEcu::new(config));
    bus.device_open = false;
    bus.device_id = DEVICE_ID;
    bus.vci_present = true;
    bus.device_lost = false;
    bus.channels.clear();
    bus.next_channel = 1;
    bus.programming_pin = None;
    bus.control_dir = None;
    bus.battery_mv = DEFAULT_BATTERY_MV;
}

// ---------------------------------------------------------------- Control

/// Environment variable naming a directory `sim-vci` takes control commands from, so that a
/// test can reach the library inside the worker process that loaded it (ADR-238). Read once,
/// on the first call into the library.
pub const CONTROL_DIR_ENV: &str = "NGR_SIM_VCI_CONTROL_DIR";

/// How often a waiting `PassThruReadMsgs` looks for control files.
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A control command, as JSON: `{"command": "inject_fault", "fault": "power_loss"}`,
/// `{"command": "reconnect_ecu"}`, `{"command": "disconnect_vci"}`, `{"command": "connect_vci"}`,
/// `{"command": "set_battery_voltage", "millivolts": 11500}`.
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    /// Arms a `sim_ecu::Fault` on the ECU (`SimEcu::inject`).
    InjectFault { fault: Fault },
    /// `SimEcu::reconnect`: ends a power loss.
    ReconnectEcu {},
    /// Unplugs the VCI: J2534 calls return `ERR_DEVICE_NOT_CONNECTED`.
    DisconnectVci {},
    /// Plugs the VCI back in. A device that was open when it went away stays lost until it is
    /// closed (J2534-1 6.10.1).
    ConnectVci {},
    /// Sets the battery voltage `READ_VBATT` reports, in millivolts.
    SetBatteryVoltage { millivolts: u32 },
}

/// Applies one control command, given as the JSON a control file holds. For a test that loads
/// the library into its own process; a worker process is reached through [`CONTROL_DIR_ENV`].
/// Returns `ERR_FAILED` for a command that cannot be parsed, or an ECU that cannot be created.
///
/// # Safety
/// `command` must be null or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn NgrSimVciControl(command: *const c_char) -> PassThruUlong {
    if command.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees a NUL-terminated string.
    let text = unsafe { CStr::from_ptr(command) };
    let Some(command) = text
        .to_str()
        .ok()
        .and_then(|text| serde_json::from_str::<Command>(text).ok())
    else {
        return ERR_FAILED;
    };
    // The command takes effect now; the control directory is left for the next J2534 call. The
    // two ways in are not ordered against each other.
    match lock_bus().apply(command) {
        Ok(()) => STATUS_NOERROR,
        Err(status) => status,
    }
}

// ---------------------------------------------------------------- Exports

/// Side-effect-free function the worker's launch test calls once the device is open (design
/// 7.3; J2534-1 7.2.12 allows it only after `PassThruOpen`). If the ABI interpretation is wrong,
/// this surfaces as garbage values or a crash.
///
/// # Safety
/// Each pointer must be null or valid for a write of 80 bytes, the buffer size J2534-1 gives.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruReadVersion(
    device_id: PassThruUlong,
    firmware_version: *mut c_char,
    dll_version: *mut c_char,
    api_version: *mut c_char,
) -> PassThruUlong {
    let bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if !bus.device_ok(device_id) {
        return ERR_INVALID_DEVICE_ID;
    }
    drop(bus);
    if firmware_version.is_null() || dll_version.is_null() || api_version.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees 80 writable bytes each.
    unsafe {
        write_c_string(firmware_version, FIRMWARE_VERSION);
        write_c_string(dll_version, DLL_VERSION);
        write_c_string(api_version, API_VERSION);
    }
    STATUS_NOERROR
}

/// Writes `text` NUL-terminated, cut to the 80-byte buffers of the J2534 string outputs.
///
/// # Safety
/// `out` must be valid for a write of 80 bytes.
unsafe fn write_c_string(out: *mut c_char, text: &str) {
    const BUFFER: usize = 80;
    let bytes = &text.as_bytes()[..text.len().min(BUFFER - 1)];
    // SAFETY: the caller guarantees `BUFFER` writable bytes; at most that many are written.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len());
        out.add(bytes.len()).write(0);
    }
}

/// Opens the simulated device. If the process has no ECU yet, the ECU continues from the state
/// file (`NGR_SIM_ECU_STATE`) or powers up with the configuration from `NGR_SIM_ECU_CONFIG`.
///
/// # Safety
/// `device_id` must be null or valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruOpen(
    _name: *const c_void,
    device_id: *mut PassThruUlong,
) -> PassThruUlong {
    // TODO: make multiple opens configurable (for verifying 9.3 "whether multiple devices can be opened simultaneously").
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if !bus.vci_present {
        return ERR_DEVICE_NOT_CONNECTED;
    }
    if device_id.is_null() {
        return ERR_NULL_PARAMETER;
    }
    if let Err(status) = bus.ecu() {
        return status;
    }
    bus.device_open = true;
    // SAFETY: checked for null above; the caller guarantees it is writable.
    unsafe { device_id.write(bus.device_id) };
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "system" fn PassThruClose(device_id: PassThruUlong) -> PassThruUlong {
    let mut bus = lock();
    if bus.device_lost && bus.device_ok(device_id) {
        // Closing is how the application recovers from a lost device; the call still reports
        // the loss, and the next open gets a new device ID (J2534-1 6.10.1).
        bus.close_device();
        bus.device_lost = false;
        bus.device_id = bus.device_id.wrapping_add(1).max(1);
        return ERR_DEVICE_NOT_CONNECTED;
    }
    if bus.unreachable() {
        return ERR_DEVICE_NOT_CONNECTED;
    }
    if !bus.device_ok(device_id) {
        return ERR_INVALID_DEVICE_ID;
    }
    bus.close_device();
    STATUS_NOERROR
}

/// Connects an ISO 15765 channel with 11-bit CAN IDs, the only kind the simulated ECU speaks.
///
/// # Safety
/// `channel_id` must be null or valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruConnect(
    device_id: PassThruUlong,
    protocol_id: PassThruUlong,
    flags: PassThruUlong,
    _baud_rate: PassThruUlong,
    channel_id: *mut PassThruUlong,
) -> PassThruUlong {
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
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
    bus.channels.insert(id, Channel::new(protocol_id));
    // SAFETY: checked for null above; the caller guarantees it is writable.
    unsafe { channel_id.write(id) };
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "system" fn PassThruDisconnect(channel_id: PassThruUlong) -> PassThruUlong {
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
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
pub unsafe extern "system" fn PassThruReadMsgs(
    channel_id: PassThruUlong,
    msgs: *mut PassThruMsg,
    num_msgs: *mut PassThruUlong,
    timeout: PassThruUlong,
) -> PassThruUlong {
    let mut bus = lock();
    if bus.unreachable() {
        if !num_msgs.is_null() {
            // SAFETY: checked for null; the caller guarantees it is valid.
            unsafe { num_msgs.write(0) };
        }
        return ERR_DEVICE_NOT_CONNECTED;
    }
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
    let mut read = 0;
    let status = loop {
        if bus.unreachable() {
            break ERR_DEVICE_NOT_CONNECTED;
        }
        let epoch = bus.epoch;
        let Some(channel) = bus.channels.get_mut(&channel_id) else {
            break ERR_INVALID_CHANNEL_ID;
        };
        let now = Instant::now();
        channel.settle(now);
        while read < wanted && channel.rx.front().is_some_and(|p| p.ready_at <= now) {
            let pending = channel.rx.pop_front().expect("front checked above");
            let mut msg = PassThruMsg {
                protocol_id: channel.protocol_id,
                rx_status: 0,
                tx_flags: 0,
                timestamp: timestamp(epoch, pending.ready_at),
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
        // A control command may arrive while the read waits (a VCI disconnect ends it).
        let wake = match bus.control_dir {
            Some(_) => wake.min(now + CONTROL_POLL_INTERVAL),
            None => wake,
        };
        bus = wait(bus, wake.saturating_duration_since(now));
        bus.apply_control_files();
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
pub unsafe extern "system" fn PassThruWriteMsgs(
    channel_id: PassThruUlong,
    msgs: *const PassThruMsg,
    num_msgs: *mut PassThruUlong,
    _timeout: PassThruUlong,
) -> PassThruUlong {
    let mut bus = lock();
    // Nothing is sent when the call fails before the first message.
    let fail = |status| {
        if !num_msgs.is_null() {
            // SAFETY: checked for null; the caller guarantees it is valid.
            unsafe { num_msgs.write(0) };
        }
        status
    };
    if bus.unreachable() {
        return fail(ERR_DEVICE_NOT_CONNECTED);
    }
    if msgs.is_null() || num_msgs.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees it is valid.
    let wanted = unsafe { num_msgs.read() } as usize;
    let Some(protocol_id) = bus.channels.get(&channel_id).map(|c| c.protocol_id) else {
        return fail(ERR_INVALID_CHANNEL_ID);
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
        if !(CAN_ID_LEN + 1..=MAX_MSG_DATA).contains(&size)
            || msg.tx_flags & UNSUPPORTED_TX_FLAGS != 0
        {
            break ERR_INVALID_MSG;
        }
        if let Err(status) = bus.transmit(channel_id, &msg.data[..size]) {
            break status;
        }
        sent += 1;
    };
    // SAFETY: as above.
    unsafe { num_msgs.write(sent as PassThruUlong) };
    if sent > 0 {
        notify();
    }
    status
}

/// Starts a flow-control filter, the only filter type ISO 15765 channels take (J2534-1 7.2.9).
/// The simulator supports normal 11-bit addressing, so each message is a 4-byte CAN ID; the
/// mask must select the whole ID. A channel then receives only from pattern IDs and sends
/// segmented messages only to flow-control IDs of its filters.
///
/// # Safety
/// The message pointers must be null or valid for reads, and `filter_id` null or valid for a
/// write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruStartMsgFilter(
    channel_id: PassThruUlong,
    filter_type: PassThruUlong,
    mask: *const PassThruMsg,
    pattern: *const PassThruMsg,
    flow_control: *const PassThruMsg,
    filter_id: *mut PassThruUlong,
) -> PassThruUlong {
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    let Some(channel) = bus.channels.get_mut(&channel_id) else {
        return ERR_INVALID_CHANNEL_ID;
    };
    channel.settle(Instant::now());
    if filter_type != filter::FLOW_CONTROL_FILTER as PassThruUlong {
        return ERR_INVALID_FILTER_ID;
    }
    if mask.is_null() || pattern.is_null() || flow_control.is_null() || filter_id.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees they are readable.
    let (mask, pattern, flow_control) = unsafe { (&*mask, &*pattern, &*flow_control) };
    let messages = [mask, pattern, flow_control];
    if messages
        .iter()
        .any(|m| m.protocol_id != channel.protocol_id)
    {
        return ERR_MSG_PROTOCOL_ID;
    }
    if messages
        .iter()
        .any(|m| m.data_size != mask.data_size || m.tx_flags != mask.tx_flags)
    {
        return ERR_INVALID_MSG;
    }
    // The channel uses 11-bit IDs and normal addressing: no 29-bit IDs, no extended address.
    if mask.tx_flags & UNSUPPORTED_TX_FLAGS != 0 {
        return ERR_INVALID_MSG;
    }
    if mask.data_size as usize != CAN_ID_LEN || mask.data[..CAN_ID_LEN] != [0xFF; CAN_ID_LEN] {
        return ERR_INVALID_MSG;
    }
    let new = FlowControlFilter {
        pattern: can_id_of(&pattern.data),
        flow_control: can_id_of(&flow_control.data),
    };
    if new.pattern > MAX_CAN_ID || new.flow_control > MAX_CAN_ID {
        return ERR_INVALID_MSG;
    }
    // Pattern and flow-control IDs must not appear in another filter; a filter may use one ID
    // for both, to receive functionally addressed single frames.
    if channel.filters.values().any(|f| {
        [f.pattern, f.flow_control].contains(&new.pattern)
            || [f.pattern, f.flow_control].contains(&new.flow_control)
    }) {
        return ERR_NOT_UNIQUE;
    }
    if channel.filters.len() >= MAX_FILTERS {
        return ERR_EXCEEDED_LIMIT;
    }
    // IDs are reused only after wrapping around, and never while still in use.
    let mut id = channel.next_filter;
    while id == 0 || channel.filters.contains_key(&id) {
        id = id.wrapping_add(1);
    }
    channel.next_filter = id.wrapping_add(1);
    channel.filters.insert(id, new);
    // SAFETY: checked for null above; the caller guarantees it is writable.
    unsafe { filter_id.write(id) };
    STATUS_NOERROR
}

#[unsafe(no_mangle)]
pub extern "system" fn PassThruStopMsgFilter(
    channel_id: PassThruUlong,
    filter_id: PassThruUlong,
) -> PassThruUlong {
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    let Some(channel) = bus.channels.get_mut(&channel_id) else {
        return ERR_INVALID_CHANNEL_ID;
    };
    channel.settle(Instant::now());
    match channel.filters.remove(&filter_id) {
        Some(_) => STATUS_NOERROR,
        None => ERR_INVALID_FILTER_ID,
    }
}

/// Periodic messages are not simulated.
#[unsafe(no_mangle)]
pub extern "system" fn PassThruStartPeriodicMsg(
    channel_id: PassThruUlong,
    _msg: *const PassThruMsg,
    _msg_id: *mut PassThruUlong,
    _interval: PassThruUlong,
) -> PassThruUlong {
    let bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if !bus.channels.contains_key(&channel_id) {
        return ERR_INVALID_CHANNEL_ID;
    }
    ERR_NOT_SUPPORTED
}

/// No periodic message can exist, so every ID is invalid.
#[unsafe(no_mangle)]
pub extern "system" fn PassThruStopPeriodicMsg(
    channel_id: PassThruUlong,
    _msg_id: PassThruUlong,
) -> PassThruUlong {
    let bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if !bus.channels.contains_key(&channel_id) {
        return ERR_INVALID_CHANNEL_ID;
    }
    ERR_INVALID_MSG_ID
}

/// Keeps track of the programming pin by the rules of J2534-1 7.2.11; nothing is driven. One pin
/// at a time carries 5 to 20 V (switch it off before using another), and pin 15 can only be
/// shorted to ground. A voltage outside the valid values gets `ERR_FAILED`, since the clause
/// names no code for it.
#[unsafe(no_mangle)]
pub extern "system" fn PassThruSetProgrammingVoltage(
    device_id: PassThruUlong,
    pin: PassThruUlong,
    voltage: PassThruUlong,
) -> PassThruUlong {
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if !bus.device_ok(device_id) {
        return ERR_INVALID_DEVICE_ID;
    }
    if pin == GROUND_PIN {
        return match voltage {
            SHORT_TO_GROUND | VOLTAGE_OFF => STATUS_NOERROR,
            _ => ERR_PIN_INVALID,
        };
    }
    if !PROGRAMMING_PINS.contains(&pin) {
        return ERR_PIN_INVALID;
    }
    match voltage {
        VOLTAGE_OFF => {
            if bus.programming_pin == Some(pin) {
                bus.programming_pin = None;
            }
            STATUS_NOERROR
        }
        SHORT_TO_GROUND => ERR_PIN_INVALID,
        5000..=20000 => match bus.programming_pin {
            Some(other) if other != pin => ERR_PIN_INVALID,
            _ => {
                bus.programming_pin = Some(pin);
                STATUS_NOERROR
            }
        },
        _ => ERR_FAILED,
    }
}

/// Writes [`LAST_ERROR_TEXT`]: the simulator keeps no error descriptions.
///
/// # Safety
/// `description` must be null or valid for a write of 80 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruGetLastError(description: *mut c_char) -> PassThruUlong {
    if description.is_null() {
        return ERR_NULL_PARAMETER;
    }
    // SAFETY: checked for null above; the caller guarantees 80 writable bytes.
    unsafe { write_c_string(description, LAST_ERROR_TEXT) };
    STATUS_NOERROR
}

/// `CLEAR_MSG_FILTERS` and `CLEAR_RX_BUFFER` act on a channel's filters and receive queue
/// (J2534-1 7.3). `READ_VBATT` takes the device ID and reports the battery voltage in
/// millivolts, rounded to a tenth of a volt (7.3.3). Every other IOCTL is accepted and does
/// nothing.
///
/// # Safety
/// For `READ_VBATT`, `output` must be null or valid for a write of an `unsigned long`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn PassThruIoctl(
    handle_id: PassThruUlong,
    ioctl_id: PassThruUlong,
    _input: *mut c_void,
    output: *mut c_void,
) -> PassThruUlong {
    const CLEAR_MSG_FILTERS: PassThruUlong = ioctl::IOCTL_CLEAR_MSG_FILTERS as PassThruUlong;
    const CLEAR_RX_BUFFER: PassThruUlong = ioctl::IOCTL_CLEAR_RX_BUFFER as PassThruUlong;
    const READ_VBATT: PassThruUlong = ioctl::IOCTL_READ_VBATT as PassThruUlong;
    let mut bus = match lock_connected() {
        Ok(bus) => bus,
        Err(status) => return status,
    };
    if ioctl_id == READ_VBATT {
        if !bus.device_ok(handle_id) {
            return ERR_INVALID_DEVICE_ID;
        }
        if output.is_null() {
            return ERR_NULL_PARAMETER;
        }
        let millivolts = bus.battery_mv.saturating_add(50) / 100 * 100;
        // SAFETY: checked for null above; the caller guarantees it is writable.
        unsafe {
            output
                .cast::<PassThruUlong>()
                .write(millivolts as PassThruUlong)
        };
        return STATUS_NOERROR;
    }
    if ioctl_id != CLEAR_MSG_FILTERS && ioctl_id != CLEAR_RX_BUFFER {
        return STATUS_NOERROR;
    }
    let Some(channel) = bus.channels.get_mut(&handle_id) else {
        return ERR_INVALID_CHANNEL_ID;
    };
    let now = Instant::now();
    channel.settle(now);
    if ioctl_id == CLEAR_MSG_FILTERS {
        channel.filters.clear();
    } else {
        // Responses already on the bus are discarded; ones still being delayed arrive later.
        channel.rx.retain(|p| p.ready_at > now);
    }
    STATUS_NOERROR
}

#[cfg(test)]
mod tests;
