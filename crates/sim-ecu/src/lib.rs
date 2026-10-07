//! Vehicle simulator (ECU side). For testing per 13.4.
//!
//! Simulates responses to UDS services, negative response codes (NRC), flash session state transitions,
//! response delay and no-response, and security access seed generation.
//! Used for interruption / resumption tests (8.2.5).
//!
//! The services follow ISO 14229-1 (2026 edition). Clause numbers in this crate refer to that
//! edition. Choices the standard leaves to the vehicle manufacturer (which DIDs and routines exist,
//! the seed/key algorithm, the session each service is available in) are listed in
//! `crates/sim-ecu/docs/simulated-ecu.md`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

mod services;
#[cfg(test)]
mod tests;

// ---------------------------------------------------------------- Constants

/// Service identifier of a negative response message (clause 7.7, Annex A.1).
pub const NEGATIVE_RESPONSE_SID: u8 = 0x7F;

/// Bit 7 of a SubFunction byte: suppressPosRspMsgIndicationBit.
pub const SUPPRESS_POS_RSP: u8 = 0x80;

/// Offset added to a request SID to form the positive response SID.
pub const POSITIVE_RESPONSE_OFFSET: u8 = 0x40;

pub const DID_ACTIVE_DIAGNOSTIC_SESSION: u16 = 0xF186;
pub const DID_SPARE_PART_NUMBER: u16 = 0xF187;
pub const DID_SOFTWARE_VERSION: u16 = 0xF189;
pub const DID_VIN: u16 = 0xF190;
/// Simulator-specific DID (system supplier range) reporting the flash state, so that a resume can
/// start with a state check (8.2.5). Record: phase code (1 byte), block number (4 bytes) and
/// bytes received of the current download (4 bytes), big endian.
pub const DID_FLASH_STATE: u16 = 0xFD00;

/// eraseMemory routine (Annex F).
pub const RID_ERASE_MEMORY: u16 = 0xFF00;
/// checkProgrammingDependencies routine (Annex F). The simulator uses it as the integrity check
/// after a download.
pub const RID_CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;

/// Simulated flash memory window accepted by RequestDownload.
pub const FLASH_START: u32 = 0x0000_0000;
pub const FLASH_SIZE: u32 = 0x0010_0000;

/// maxNumberOfBlockLength reported by RequestDownload: the whole TransferData request,
/// SID and blockSequenceCounter included (clause 14.2.3).
pub const MAX_BLOCK_LENGTH: u16 = 0x0102;

/// Most DIDs accepted in one ReadDataByIdentifier request.
pub const MAX_DIDS_PER_READ: usize = 8;

/// Longest response the simulated transport can carry (ISO-TP on classical CAN).
pub const MAX_RESPONSE_LENGTH: usize = 4095;

/// False sendKey attempts that activate the security delay timer.
pub const MAX_SECURITY_ATTEMPTS: u8 = 3;

/// tS3_Server when `EcuConfig::s3_server_ms` is unset: the value ISO 14229-2 (2021) clause 9.5
/// (Table 5) gives for how long a non-default session stays active without a request.
pub const DEFAULT_S3_SERVER_MS: u32 = 5_000;

/// Security delay when `EcuConfig::security_delay_ms` is unset. ISO 14229-1 (2026) clause 9.4.1
/// leaves the length to the vehicle manufacturer.
pub const DEFAULT_SECURITY_DELAY_MS: u32 = 10_000;

/// Value the simulator XORs with the seed to form the expected key.
pub const SECURITY_KEY_MASK: u32 = 0x5A5A_5A5A;

/// DTCStatusAvailabilityMask: the simulator supports every status bit (Annex D.2).
pub const DTC_STATUS_AVAILABILITY_MASK: u8 = 0xFF;

/// statusOfDTC after a successful ClearDiagnosticInformation: only the two
/// "test not completed" bits (4 and 6) are set (Annex D.2).
pub const DTC_STATUS_AFTER_CLEAR: u8 = 0x50;

/// tL7_P2_Server_Max reported in the DiagnosticSessionControl response (1 ms resolution).
pub const P2_SERVER_MAX_MS: u16 = 50;
/// tL7_P2*_Server_Max reported in the DiagnosticSessionControl response (10 ms resolution).
pub const P2_STAR_SERVER_MAX_10MS: u16 = 500;

// ---------------------------------------------------------------- Configuration

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EcuConfig {
    pub vin: String,
    pub part_number: String,
    pub sw_version: String,
    /// Simulated response delay (ms). Used for testing the P2 timeout boundary.
    pub response_delay_ms: u32,
    /// Stop responding at the specified block number (injects an interruption, once: the field is
    /// cleared when it fires). Block numbers count TransferData blocks from 1 across the whole
    /// download.
    pub drop_at_block: Option<u32>,
    /// Whether to require security access.
    pub require_security_access: bool,
    /// Whether to require gateway authentication.
    pub require_gateway_auth: bool,
    /// Make checksum verification fail after the transfer completes.
    pub fail_checksum: bool,
    /// DTCs stored in the ECU at power-up.
    #[serde(default)]
    pub dtcs: Vec<DtcRecord>,
    /// tS3_Server in ms: how long a non-default session stays active without a request.
    /// [`DEFAULT_S3_SERVER_MS`] when unset.
    #[serde(default)]
    pub s3_server_ms: Option<u32>,
    /// How long the security delay timer runs, in ms. [`DEFAULT_SECURITY_DELAY_MS`] when unset.
    #[serde(default)]
    pub security_delay_ms: Option<u32>,
}

// ---------------------------------------------------------------- Clock

/// The time source of the ECU's timers (design 13.4, clock substitution). [`SimEcu::new`] uses
/// [`SystemClock`]; tests that need to control time use [`ManualClock`].
pub trait Clock: Send {
    /// Time elapsed since a fixed origin. Never goes backwards.
    fn now(&self) -> Duration;
}

/// Real time, measured from when the clock was created.
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that moves only when [`ManualClock::advance`] is called. Clones share the same time,
/// so a test keeps one clone and hands another to [`SimEcu::with_clock`].
#[derive(Clone, Default)]
pub struct ManualClock(Arc<Mutex<Duration>>);

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *now = now.saturating_add(by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DtcRecord {
    /// 3-byte DTC number (the top byte is ignored).
    pub dtc: u32,
    pub status: u8,
}

// ---------------------------------------------------------------- State

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Session {
    Default,
    Extended,
    Programming,
}

impl Session {
    /// diagnosticSessionType value (clause 9.2.2).
    pub fn code(self) -> u8 {
        match self {
            Session::Default => 0x01,
            Session::Programming => 0x02,
            Session::Extended => 0x03,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0x01 => Some(Session::Default),
            0x02 => Some(Session::Programming),
            0x03 => Some(Session::Extended),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlashPhase {
    Idle,
    Erased,
    Transferring {
        next_block: u32,
    },
    TransferComplete,
    Verified,
    /// Interrupted mid-transfer. Reported on reconnection (8.2.5 "resumption starts with a state check").
    Interrupted {
        last_block: u32,
    },
}

impl FlashPhase {
    /// Encoding used by [`DID_FLASH_STATE`]: phase code and block number.
    pub fn encode(self) -> [u8; 5] {
        let (code, block) = match self {
            FlashPhase::Idle => (0x00, 0),
            FlashPhase::Erased => (0x01, 0),
            FlashPhase::Transferring { next_block } => (0x02, next_block),
            FlashPhase::TransferComplete => (0x03, 0),
            FlashPhase::Verified => (0x04, 0),
            FlashPhase::Interrupted { last_block } => (0x05, last_block),
        };
        let b = block.to_be_bytes();
        [code, b[0], b[1], b[2], b[3]]
    }
}

/// An accepted RequestDownload. Survives a reconnection so that the transfer can resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Download {
    /// Start address and total size of the image, from the first RequestDownload.
    start: u32,
    size: u32,
    /// Bytes stored so far.
    received: u32,
    /// blockSequenceCounter the next new block must carry.
    expected_bsc: u8,
    /// blockSequenceCounter and length of the last stored block, for accepting a repeated request.
    last_bsc: Option<u8>,
    last_len: u32,
    /// Whether the transfer was opened or resumed with `require_security_access` set.
    secured: bool,
}

pub struct SimEcu {
    pub config: EcuConfig,
    pub session: Session,
    pub flash: FlashPhase,
    pub security_unlocked: bool,
    /// Set by the test to stand for a gateway authentication; the ECU never clears it.
    pub gateway_authenticated: bool,
    pub dtcs: Vec<DtcRecord>,
    seed_counter: u32,
    pending_seed: Option<u32>,
    failed_attempts: u8,
    /// When the running security delay ends, on [`Self::clock`]'s time line.
    security_delay_until: Option<Duration>,
    /// When the non-default session times out (tS3_Server), on [`Self::clock`]'s time line.
    /// `None` in the default session.
    s3_deadline: Option<Duration>,
    clock: Box<dyn Clock>,
    download: Option<Download>,
    image: Vec<u8>,
    /// Set by `drop_at_block` and [`Fault::PowerLoss`]: the ECU answers nothing until
    /// [`SimEcu::reconnect`].
    silent: bool,
    /// Faults armed by [`SimEcu::inject`] that have not fired yet, in injection order.
    armed: Vec<Fault>,
    /// Power cycles so far (power loss, ECU reset, reconnection).
    power_cycles: u64,
}

// ---------------------------------------------------------------- Responses

/// ISO 14229 NRCs (Annex A.1). Used as expected values in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Nrc {
    ServiceNotSupported = 0x11,
    SubFunctionNotSupported = 0x12,
    IncorrectMessageLengthOrInvalidFormat = 0x13,
    ResponseTooLong = 0x14,
    ConditionsNotCorrect = 0x22,
    RequestSequenceError = 0x24,
    RequestOutOfRange = 0x31,
    SecurityAccessDenied = 0x33,
    AuthenticationRequired = 0x34,
    InvalidKey = 0x35,
    ExceededNumberOfAttempts = 0x36,
    RequiredTimeDelayNotExpired = 0x37,
    TransferDataSuspended = 0x71,
    GeneralProgrammingFailure = 0x72,
    WrongBlockSequenceCounter = 0x73,
    ResponsePending = 0x78,
    SubFunctionNotSupportedInActiveSession = 0x7E,
    ServiceNotSupportedInActiveSession = 0x7F,
}

impl Nrc {
    /// NRCs that a server never sends for a functionally addressed request (clause 7.7.1).
    fn suppressed_on_functional(self) -> bool {
        matches!(
            self,
            Nrc::ServiceNotSupported
                | Nrc::ServiceNotSupportedInActiveSession
                | Nrc::SubFunctionNotSupported
                | Nrc::SubFunctionNotSupportedInActiveSession
                | Nrc::RequestOutOfRange
        )
    }
}

/// Addressing mode of a request (clause 7.7.3 / 7.7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    Physical,
    Functional,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SimResponse {
    /// Positive response message, response SID included.
    Positive(Vec<u8>),
    /// Negative response to the request with service identifier `sid`.
    Negative { sid: u8, nrc: Nrc },
    /// No response: suppressed by the request, suppressed for functional addressing,
    /// or a simulated timeout.
    NoResponse,
}

impl SimResponse {
    /// The bytes on the wire, or `None` when nothing is sent.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        match self {
            SimResponse::Positive(bytes) => Some(bytes.clone()),
            SimResponse::Negative { sid, nrc } => {
                Some(vec![NEGATIVE_RESPONSE_SID, *sid, *nrc as u8])
            }
            SimResponse::NoResponse => None,
        }
    }
}

/// A response and the delay before it is sent, from [`SimEcu::exchange`].
#[derive(Debug, PartialEq, Eq)]
pub struct Exchange {
    pub response: SimResponse,
    /// Milliseconds between the request and the response. Zero when there is no response.
    pub delay_ms: u32,
}

impl Exchange {
    fn none() -> Self {
        Self {
            response: SimResponse::NoResponse,
            delay_ms: 0,
        }
    }
}

/// Result of a service handler: `Ok(None)` means no message is sent.
type ServiceResult = Result<Option<Vec<u8>>, Nrc>;

impl SimEcu {
    /// An ECU whose timers run on real time ([`SystemClock`]).
    pub fn new(config: EcuConfig) -> Self {
        Self::with_clock(config, SystemClock::default())
    }

    /// An ECU whose timers run on `clock`.
    pub fn with_clock(config: EcuConfig, clock: impl Clock + 'static) -> Self {
        let dtcs = config.dtcs.clone();
        Self {
            config,
            session: Session::Default,
            flash: FlashPhase::Idle,
            security_unlocked: false,
            gateway_authenticated: false,
            dtcs,
            seed_counter: 0,
            pending_seed: None,
            failed_attempts: 0,
            security_delay_until: None,
            s3_deadline: None,
            clock: Box::new(clock),
            download: None,
            image: Vec::new(),
            silent: false,
            armed: Vec::new(),
            power_cycles: 0,
        }
    }

    /// Simulates reconnection after a power loss or communication loss.
    /// The session and unlock state are lost, but flash progress is retained. Gateway
    /// authentication is the gateway's state and is kept.
    pub fn reconnect(&mut self) {
        self.silent = false;
        self.power_cycle();
    }

    /// The key the simulator expects for `seed` (SecurityAccess, clause 9.4).
    pub fn key_for_seed(seed: u32) -> u32 {
        seed ^ SECURITY_KEY_MASK
    }

    /// Ends the security delay timer started by too many false keys at once, as if it had run
    /// out.
    pub fn expire_security_delay(&mut self) {
        self.security_delay_until = None;
        self.failed_attempts = 0;
    }

    /// Whether the security delay timer is running.
    pub fn security_delay_active(&mut self) -> bool {
        self.check_timers();
        self.security_delay_until.is_some()
    }

    /// Applies the timers that have run out by now: a non-default session whose tS3_Server
    /// expired falls back to the default session, and an expired security delay ends.
    /// [`SimEcu::exchange`] does this before it handles a request; a test that reads the state
    /// directly after advancing its clock calls it first.
    pub fn check_timers(&mut self) {
        let now = self.clock.now();
        if self.s3_deadline.is_some_and(|deadline| now >= deadline) {
            self.s3_deadline = None;
            if self.session != Session::Default {
                // The same as a change to the default session: security is relocked and a
                // running download stops (clause 9.2.1).
                self.lock_security();
                self.interrupt_transfer();
                self.session = Session::Default;
            }
        }
        if self.security_delay_until.is_some_and(|until| now >= until) {
            self.expire_security_delay();
        }
    }

    /// Starts the security delay timer for its full length.
    pub(crate) fn start_security_delay(&mut self) {
        let delay = self
            .config
            .security_delay_ms
            .unwrap_or(DEFAULT_SECURITY_DELAY_MS);
        self.security_delay_until = Some(
            self.clock
                .now()
                .saturating_add(Duration::from_millis(delay.into())),
        );
    }

    /// Restarts tS3_Server after a request was handled: it runs from when the response goes out
    /// (`delay_ms` from now), or from now when there is none, and only in a non-default session
    /// (ISO 14229-2 (2021) clause 9.5, Table 6; ADR-239).
    fn restart_s3(&mut self, delay_ms: u32) {
        self.s3_deadline = (self.session != Session::Default).then(|| {
            let s3 = self.config.s3_server_ms.unwrap_or(DEFAULT_S3_SERVER_MS);
            self.clock
                .now()
                .saturating_add(Duration::from_millis(u64::from(delay_ms) + u64::from(s3)))
        });
    }

    /// Data stored by TransferData since the last erase.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// Handles one physically addressed request. `message` starts with the SID.
    pub fn request(&mut self, message: &[u8]) -> SimResponse {
        self.request_with(Addressing::Physical, message)
    }

    /// Handles one request. `message` starts with the SID.
    pub fn request_with(&mut self, addressing: Addressing, message: &[u8]) -> SimResponse {
        self.exchange(addressing, message).response
    }

    /// Handles one request and reports how long after the request the response goes out, for the
    /// VCI side to apply. The delay is `EcuConfig::response_delay_ms` plus any
    /// [`Fault::DelayResponse`] that fires on this request.
    pub fn exchange(&mut self, addressing: Addressing, message: &[u8]) -> Exchange {
        self.check_timers();
        if self.silent {
            return Exchange::none();
        }
        if self.take_armed(|f| matches!(f, Fault::BusError)).is_some() {
            // The request never reaches the ECU.
            return Exchange::none();
        }
        let (exchange, sent_after_ms) = self.handle(addressing, message);
        // Any request that reaches the ECU restarts tS3_Server, supported or not.
        self.restart_s3(sent_after_ms);
        exchange
    }

    /// [`SimEcu::exchange`] for a request that has reached the ECU. Also returns when the
    /// response finished going out, in ms from now: tS3_Server restarts then, even for a
    /// response lost on the way ([`Fault::DropResponse`]), and at once when none is sent.
    fn handle(&mut self, addressing: Addressing, message: &[u8]) -> (Exchange, u32) {
        let mut delay_ms = self.config.response_delay_ms;
        while let Some(Fault::DelayResponse { ms }) =
            self.take_armed(|f| matches!(f, Fault::DelayResponse { .. }))
        {
            delay_ms = delay_ms.saturating_add(ms);
        }
        let response = self.respond(addressing, message);
        let dropped = self
            .take_armed(|f| matches!(f, Fault::DropResponse))
            .is_some();
        if response == SimResponse::NoResponse {
            return (Exchange::none(), 0);
        }
        if dropped {
            // The request was handled; only its response is lost.
            return (Exchange::none(), delay_ms);
        }
        (Exchange { response, delay_ms }, delay_ms)
    }

    /// Arms a fault (13.4). [`Fault::PowerLoss`] takes effect at once; the others fire on a later
    /// request, once each, as described on each variant.
    pub fn inject(&mut self, fault: Fault) {
        match fault {
            Fault::PowerLoss => {
                self.power_cycle();
                self.silent = true;
            }
            _ => self.armed.push(fault),
        }
    }

    /// Number of power cycles (power loss, ECU reset, [`SimEcu::reconnect`]) so far. A response
    /// the VCI side is still delaying when this changes was never sent.
    pub fn power_cycles(&self) -> u64 {
        self.power_cycles
    }

    /// Faults armed by [`SimEcu::inject`] that have not fired yet.
    pub fn armed_faults(&self) -> &[Fault] {
        &self.armed
    }

    /// Removes and returns the first armed fault matching `pred`.
    fn take_armed(&mut self, pred: impl Fn(&Fault) -> bool) -> Option<Fault> {
        let i = self.armed.iter().position(pred)?;
        Some(self.armed.remove(i))
    }

    fn respond(&mut self, addressing: Addressing, message: &[u8]) -> SimResponse {
        let Some(&sid) = message.first() else {
            return SimResponse::NoResponse;
        };
        match self.dispatch(sid, message) {
            Ok(Some(bytes)) => SimResponse::Positive(bytes),
            Ok(None) => SimResponse::NoResponse,
            Err(nrc) if addressing == Addressing::Functional && nrc.suppressed_on_functional() => {
                SimResponse::NoResponse
            }
            Err(nrc) => SimResponse::Negative { sid, nrc },
        }
    }

    /// Session, security and transfer state after a power cycle or ECU reset.
    /// The false-attempt counter starts again at zero (Annex I, transition 1); an active
    /// security delay starts again for its full length, as on power-up.
    fn power_cycle(&mut self) {
        // A delay that ran out before the power cycle stays over, whether or not a request
        // arrived in the meantime to notice it.
        self.check_timers();
        self.power_cycles += 1;
        self.session = Session::Default;
        self.s3_deadline = None;
        self.lock_security();
        self.failed_attempts = 0;
        if self.security_delay_until.is_some() {
            self.start_security_delay();
        }
        self.interrupt_transfer();
    }

    fn lock_security(&mut self) {
        self.security_unlocked = false;
        self.pending_seed = None;
    }

    /// A running transfer stops; the data received so far is kept so the transfer can resume.
    fn interrupt_transfer(&mut self) {
        if let FlashPhase::Transferring { next_block } = self.flash {
            self.flash = FlashPhase::Interrupted {
                last_block: next_block.saturating_sub(1),
            };
        }
    }
}

// ---------------------------------------------------------------- Fault injection

/// Events injected during tests with [`SimEcu::inject`]. Corresponds to 13.4 "injecting delays,
/// disconnects, crashes, and write failures". Serialized in snake case (`"power_loss"`,
/// `{"delay_response": {"ms": 500}}`), the form `sim-vci`'s control commands take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Fault {
    /// The response to the next request goes out `ms` later than configured.
    DelayResponse { ms: u32 },
    /// The next request is handled (its state changes apply) but its response is lost.
    DropResponse,
    /// The ECU loses power now: session, security and a running transfer are lost as in a power
    /// cycle, and the ECU answers nothing until [`SimEcu::reconnect`].
    PowerLoss,
    /// The next request is lost on the bus: the ECU neither handles nor answers it.
    BusError,
    /// Writing TransferData block `block` (counted from 1 over the whole download, as
    /// `EcuConfig::drop_at_block`) fails: the ECU answers NRC 72 and stores nothing, so the
    /// client may send the block again. Fires when that block arrives.
    CorruptBlock { block: u32 },
}
