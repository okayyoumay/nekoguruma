//! Vehicle simulator (ECU side). For testing per 13.4.
//!
//! Simulates responses to UDS services, negative response codes (NRC), flash session state transitions,
//! response delay and no-response, and security access seed generation.
//! Used for interruption / resumption tests (8.2.5).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Configuration

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EcuConfig {
    pub vin: String,
    pub part_number: String,
    pub sw_version: String,
    /// Simulated response delay (ms). Used for testing the P2 timeout boundary.
    pub response_delay_ms: u32,
    /// Stop responding at the specified block number (injects an interruption).
    pub drop_at_block: Option<u32>,
    /// Whether to require security access.
    pub require_security_access: bool,
    /// Whether to require gateway authentication.
    pub require_gateway_auth: bool,
    /// Make checksum verification fail after the transfer completes.
    pub fail_checksum: bool,
}

// ---------------------------------------------------------------- State

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Session {
    Default,
    Extended,
    Programming,
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

pub struct SimEcu {
    pub config: EcuConfig,
    pub session: Session,
    pub flash: FlashPhase,
    pub security_unlocked: bool,
    pub gateway_authenticated: bool,
    seed_counter: u32,
}

// ---------------------------------------------------------------- Negative responses

/// ISO 14229 NRCs. Used as expected values in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nrc {
    ServiceNotSupported = 0x11,
    ConditionsNotCorrect = 0x22,
    RequestSequenceError = 0x24,
    SecurityAccessDenied = 0x33,
    InvalidKey = 0x35,
    GeneralProgrammingFailure = 0x72,
    ResponsePending = 0x78,
}

#[derive(Debug)]
pub enum SimResponse {
    Positive(Vec<u8>),
    Negative(Nrc),
    /// No response (simulated timeout).
    NoResponse,
}

impl SimEcu {
    pub fn new(config: EcuConfig) -> Self {
        Self {
            config,
            session: Session::Default,
            flash: FlashPhase::Idle,
            security_unlocked: false,
            gateway_authenticated: false,
            seed_counter: 0,
        }
    }

    /// Simulates reconnection after a power loss or communication loss.
    /// The session and unlock state are lost, but flash progress is retained.
    pub fn reconnect(&mut self) {
        self.session = Session::Default;
        self.security_unlocked = false;
        if let FlashPhase::Transferring { next_block } = self.flash {
            self.flash = FlashPhase::Interrupted {
                last_block: next_block.saturating_sub(1),
            };
        }
    }

    pub fn request(&mut self, _sid: u8, _payload: &[u8]) -> SimResponse {
        // TODO: dispatch per service.
        // - 0x10 DiagnosticSessionControl
        // - 0x22 ReadDataByIdentifier (VIN, part number, SW version)
        // - 0x19 ReadDTCInformation
        // - 0x27 SecurityAccess (generate seeds deterministically from seed_counter)
        // - 0x31 RoutineControl (erase)
        // - 0x34/0x36/0x37 RequestDownload / TransferData / TransferExit
        todo!("implement UDS services")
    }
}

// ---------------------------------------------------------------- Fault injection

/// Events injected during tests. Corresponds to 13.4 "injecting delays, disconnects, crashes, and write failures".
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    DelayResponse { ms: u32 },
    DropResponse,
    PowerLoss,
    BusError,
    CorruptBlock { block: u32 },
}
