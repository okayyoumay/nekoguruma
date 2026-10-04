//! UDS service handlers. Each handler checks in the order of the NRC evaluation figure of its
//! clause in ISO 14229-1 (2026 edition) and returns the first NRC that applies.

use crate::{
    DID_ACTIVE_DIAGNOSTIC_SESSION, DID_FLASH_STATE, DID_SOFTWARE_VERSION, DID_SPARE_PART_NUMBER,
    DID_VIN, DTC_STATUS_AFTER_CLEAR, DTC_STATUS_AVAILABILITY_MASK, Download, FLASH_SIZE,
    FLASH_START, FlashPhase, MAX_BLOCK_LENGTH, MAX_DIDS_PER_READ, MAX_RESPONSE_LENGTH,
    MAX_SECURITY_ATTEMPTS, Nrc, P2_SERVER_MAX_MS, P2_STAR_SERVER_MAX_10MS,
    POSITIVE_RESPONSE_OFFSET, RID_CHECK_PROGRAMMING_DEPENDENCIES, RID_ERASE_MEMORY,
    SUPPRESS_POS_RSP, ServiceResult, Session, SimEcu,
};

const SID_DIAGNOSTIC_SESSION_CONTROL: u8 = 0x10;
const SID_ECU_RESET: u8 = 0x11;
const SID_CLEAR_DIAGNOSTIC_INFORMATION: u8 = 0x14;
const SID_READ_DTC_INFORMATION: u8 = 0x19;
const SID_READ_DATA_BY_IDENTIFIER: u8 = 0x22;
const SID_SECURITY_ACCESS: u8 = 0x27;
const SID_WRITE_DATA_BY_IDENTIFIER: u8 = 0x2E;
const SID_ROUTINE_CONTROL: u8 = 0x31;
const SID_REQUEST_DOWNLOAD: u8 = 0x34;
const SID_TRANSFER_DATA: u8 = 0x36;
const SID_REQUEST_TRANSFER_EXIT: u8 = 0x37;
const SID_TESTER_PRESENT: u8 = 0x3E;

const ALL_DTC_GROUPS: u32 = 0x00FF_FFFF;
const DTC_FORMAT_ISO_14229_1: u8 = 0x01;

/// A SubFunction byte split into its value (bits 6-0) and the suppressPosRspMsgIndicationBit.
struct SubFunction {
    value: u8,
    suppress: bool,
}

fn positive(sid: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + data.len());
    out.push(sid + POSITIVE_RESPONSE_OFFSET);
    out.extend_from_slice(data);
    out
}

/// Common checks for a request with a SubFunction parameter (clause 7.7.3.1): minimum length and
/// SubFunction supported. Every supported SubFunction is available in every session the service
/// is, so NRC 7E is never produced.
fn sub_function(message: &[u8], supported: impl Fn(u8) -> bool) -> Result<SubFunction, Nrc> {
    if message.len() < 2 {
        return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
    }
    let value = message[1] & !SUPPRESS_POS_RSP;
    if !supported(value) {
        return Err(Nrc::SubFunctionNotSupported);
    }
    Ok(SubFunction {
        value,
        suppress: message[1] & SUPPRESS_POS_RSP != 0,
    })
}

/// A positive response, or none when the client set the suppressPosRspMsgIndicationBit.
fn reply(sf: &SubFunction, response: Vec<u8>) -> ServiceResult {
    Ok((!sf.suppress).then_some(response))
}

fn be_uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b))
}

impl SimEcu {
    /// General server response behaviour (clause 7.7.2, Figure 5): service supported,
    /// authentication, service supported in the active session; then the service itself.
    /// Gateway authentication is a service-level check at the Figure 5 position, so it comes
    /// before the length and range checks of the individual services.
    pub(crate) fn dispatch(&mut self, sid: u8, message: &[u8]) -> ServiceResult {
        let in_session = match sid {
            SID_DIAGNOSTIC_SESSION_CONTROL
            | SID_ECU_RESET
            | SID_CLEAR_DIAGNOSTIC_INFORMATION
            | SID_READ_DTC_INFORMATION
            | SID_READ_DATA_BY_IDENTIFIER
            | SID_WRITE_DATA_BY_IDENTIFIER
            | SID_ROUTINE_CONTROL
            | SID_TESTER_PRESENT => true,
            SID_SECURITY_ACCESS => self.session != Session::Default,
            SID_REQUEST_DOWNLOAD | SID_TRANSFER_DATA | SID_REQUEST_TRANSFER_EXIT => {
                self.session == Session::Programming
            }
            _ => return Err(Nrc::ServiceNotSupported),
        };
        let needs_authentication = matches!(
            sid,
            SID_WRITE_DATA_BY_IDENTIFIER
                | SID_ROUTINE_CONTROL
                | SID_REQUEST_DOWNLOAD
                | SID_TRANSFER_DATA
                | SID_REQUEST_TRANSFER_EXIT
        );
        if needs_authentication && self.config.require_gateway_auth && !self.gateway_authenticated {
            return Err(Nrc::AuthenticationRequired);
        }
        if !in_session {
            return Err(Nrc::ServiceNotSupportedInActiveSession);
        }
        match sid {
            SID_DIAGNOSTIC_SESSION_CONTROL => self.diagnostic_session_control(message),
            SID_ECU_RESET => self.ecu_reset(message),
            SID_CLEAR_DIAGNOSTIC_INFORMATION => self.clear_diagnostic_information(message),
            SID_READ_DTC_INFORMATION => self.read_dtc_information(message),
            SID_READ_DATA_BY_IDENTIFIER => self.read_data_by_identifier(message),
            SID_SECURITY_ACCESS => self.security_access(message),
            SID_WRITE_DATA_BY_IDENTIFIER => self.write_data_by_identifier(message),
            SID_ROUTINE_CONTROL => self.routine_control(message),
            SID_REQUEST_DOWNLOAD => self.request_download(message),
            SID_TRANSFER_DATA => self.transfer_data(message),
            SID_REQUEST_TRANSFER_EXIT => self.request_transfer_exit(message),
            SID_TESTER_PRESENT => self.tester_present(message),
            _ => unreachable!("filtered above"),
        }
    }

    fn security_satisfied(&self) -> bool {
        !self.config.require_security_access || self.security_unlocked
    }

    // ------------------------------------------------------------ 0x10 (clause 9.2)

    fn diagnostic_session_control(&mut self, message: &[u8]) -> ServiceResult {
        let sf = sub_function(message, |v| Session::from_code(v).is_some())?;
        if message.len() != 2 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let target = Session::from_code(sf.value).expect("checked by sub_function");
        // Every session start, including a restart of the active one, relocks security
        // (clause 9.2.1, transitions c and d). A running download depends on the unlocked state,
        // so any session start interrupts it.
        self.lock_security();
        self.interrupt_transfer();
        self.session = target;
        let p2 = P2_SERVER_MAX_MS.to_be_bytes();
        let p2_star = P2_STAR_SERVER_MAX_10MS.to_be_bytes();
        reply(
            &sf,
            positive(
                SID_DIAGNOSTIC_SESSION_CONTROL,
                &[sf.value, p2[0], p2[1], p2_star[0], p2_star[1]],
            ),
        )
    }

    // ------------------------------------------------------------ 0x11 (clause 9.3)

    fn ecu_reset(&mut self, message: &[u8]) -> ServiceResult {
        // hardReset, keyOffOnReset, softReset. Rapid power shutdown is not simulated.
        let sf = sub_function(message, |v| matches!(v, 0x01..=0x03))?;
        if message.len() != 2 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        // The response goes out before the reset takes effect (recommended by clause 9.3.1).
        let response = positive(SID_ECU_RESET, &[sf.value]);
        self.power_cycle();
        reply(&sf, response)
    }

    // ------------------------------------------------------------ 0x27 (clause 9.4)

    fn security_access(&mut self, message: &[u8]) -> ServiceResult {
        // Any SecurityAccess request ends the wait for a key: only a positive requestSeed issues
        // a new seed, and every other outcome discards the pending one (Annex I, transition 9).
        let pending_seed = self.pending_seed.take();
        // One security level: requestSeed 0x01 / sendKey 0x02.
        let sf = sub_function(message, |v| matches!(v, 0x01 | 0x02))?;
        if sf.value == 0x01 {
            // No securityAccessDataRecord is supported.
            if message.len() != 2 {
                return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
            }
            if self.security_delay_active {
                return Err(Nrc::RequiredTimeDelayNotExpired);
            }
            // An already unlocked level answers with an all-zero seed; a locked level never does.
            let seed = if self.security_unlocked {
                0
            } else {
                let seed = self.next_seed();
                self.pending_seed = Some(seed);
                seed
            };
            let s = seed.to_be_bytes();
            return reply(
                &sf,
                positive(SID_SECURITY_ACCESS, &[sf.value, s[0], s[1], s[2], s[3]]),
            );
        }

        // A seed answers exactly one sendKey, right, wrong or malformed. Without a seed, any
        // sendKey is out of sequence (Annex I, transitions 4, 7 and 9).
        let Some(seed) = pending_seed else {
            return Err(Nrc::RequestSequenceError);
        };
        if message.len() != 6 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let key = u32::from_be_bytes([message[2], message[3], message[4], message[5]]);
        if key != Self::key_for_seed(seed) {
            self.failed_attempts = self.failed_attempts.saturating_add(1);
            if self.failed_attempts >= MAX_SECURITY_ATTEMPTS {
                self.security_delay_active = true;
                return Err(Nrc::ExceededNumberOfAttempts);
            }
            return Err(Nrc::InvalidKey);
        }
        self.failed_attempts = 0;
        self.security_unlocked = true;
        reply(&sf, positive(SID_SECURITY_ACCESS, &[sf.value]))
    }

    /// Deterministic, never-zero seed sequence.
    fn next_seed(&mut self) -> u32 {
        self.seed_counter = self.seed_counter.wrapping_add(1);
        match self.seed_counter.wrapping_mul(0x9E37_79B9) ^ 0x1357_9BDF {
            0 => 1,
            seed => seed,
        }
    }

    // ------------------------------------------------------------ 0x3E (clause 9.7)

    fn tester_present(&mut self, message: &[u8]) -> ServiceResult {
        let sf = sub_function(message, |v| v == 0x00)?;
        if message.len() != 2 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        reply(&sf, positive(SID_TESTER_PRESENT, &[sf.value]))
    }

    // ------------------------------------------------------------ 0x22 (clause 10.2)

    fn read_data_by_identifier(&mut self, message: &[u8]) -> ServiceResult {
        if message.len() < 3 || !(message.len() - 1).is_multiple_of(2) {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        if (message.len() - 1) / 2 > MAX_DIDS_PER_READ {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        // Unsupported DIDs are left out; the request fails only if none is supported.
        let mut data = Vec::new();
        for pair in message[1..].chunks_exact(2) {
            let did = u16::from_be_bytes([pair[0], pair[1]]);
            if let Some(record) = self.read_did(did) {
                data.extend_from_slice(pair);
                data.extend_from_slice(&record);
            }
        }
        if data.is_empty() {
            return Err(Nrc::RequestOutOfRange);
        }
        if 1 + data.len() > MAX_RESPONSE_LENGTH {
            return Err(Nrc::ResponseTooLong);
        }
        Ok(Some(positive(SID_READ_DATA_BY_IDENTIFIER, &data)))
    }

    fn read_did(&self, did: u16) -> Option<Vec<u8>> {
        match did {
            DID_ACTIVE_DIAGNOSTIC_SESSION => Some(vec![self.session.code()]),
            DID_SPARE_PART_NUMBER => Some(self.config.part_number.as_bytes().to_vec()),
            DID_SOFTWARE_VERSION => Some(self.config.sw_version.as_bytes().to_vec()),
            DID_VIN => Some(self.config.vin.as_bytes().to_vec()),
            DID_FLASH_STATE => {
                let received = self.download.map_or(0, |dl| dl.received);
                let mut record = self.flash.encode().to_vec();
                record.extend_from_slice(&received.to_be_bytes());
                Some(record)
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------ 0x2E (clause 10.7)

    fn write_data_by_identifier(&mut self, message: &[u8]) -> ServiceResult {
        if message.len() < 4 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let did = u16::from_be_bytes([message[1], message[2]]);
        // Only the VIN is writable, and only outside the default session.
        if did != DID_VIN || self.session == Session::Default {
            return Err(Nrc::RequestOutOfRange);
        }
        let record = &message[3..];
        if record.len() != 17 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        if !self.security_satisfied() {
            return Err(Nrc::SecurityAccessDenied);
        }
        if !record.iter().all(u8::is_ascii_alphanumeric) {
            return Err(Nrc::RequestOutOfRange);
        }
        self.config.vin = String::from_utf8(record.to_vec()).expect("ASCII checked above");
        Ok(Some(positive(SID_WRITE_DATA_BY_IDENTIFIER, &message[1..3])))
    }

    // ------------------------------------------------------------ 0x14 (clause 11.2)

    fn clear_diagnostic_information(&mut self, message: &[u8]) -> ServiceResult {
        // No user-defined DTC memory, so a MemorySelection byte makes the length wrong.
        if message.len() != 4 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let group = be_uint(&message[1..4]) as u32;
        let mut matched = false;
        for record in &mut self.dtcs {
            if group == ALL_DTC_GROUPS || record.dtc & 0x00FF_FFFF == group {
                record.status = DTC_STATUS_AFTER_CLEAR;
                matched = true;
            }
        }
        // All groups is always supported, even with no DTCs stored.
        if !matched && group != ALL_DTC_GROUPS {
            return Err(Nrc::RequestOutOfRange);
        }
        Ok(Some(positive(SID_CLEAR_DIAGNOSTIC_INFORMATION, &[])))
    }

    // ------------------------------------------------------------ 0x19 (clause 11.3)

    fn read_dtc_information(&mut self, message: &[u8]) -> ServiceResult {
        // reportNumberOfDTCByStatusMask, reportDTCByStatusMask, reportSupportedDTCs.
        let sf = sub_function(message, |v| matches!(v, 0x01 | 0x02 | 0x0A))?;
        let expected_len = if sf.value == 0x0A { 2 } else { 3 };
        if message.len() != expected_len {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        // A DTC matches when its status shares at least one bit with the client's mask.
        let mask = if sf.value == 0x0A {
            None
        } else {
            Some(message[2] & DTC_STATUS_AVAILABILITY_MASK)
        };
        let matching = self
            .dtcs
            .iter()
            .filter(|r| mask.is_none_or(|m| r.status & m != 0));
        let mut data = vec![sf.value, DTC_STATUS_AVAILABILITY_MASK];
        if sf.value == 0x01 {
            let count = u16::try_from(matching.count()).unwrap_or(u16::MAX);
            data.push(DTC_FORMAT_ISO_14229_1);
            data.extend_from_slice(&count.to_be_bytes());
        } else {
            for record in matching {
                data.extend_from_slice(&record.dtc.to_be_bytes()[1..]);
                data.push(record.status);
            }
        }
        if 1 + data.len() > MAX_RESPONSE_LENGTH {
            return Err(Nrc::ResponseTooLong);
        }
        reply(&sf, positive(SID_READ_DTC_INFORMATION, &data))
    }

    // ------------------------------------------------------------ 0x31 (clause 13.2)

    fn routine_control(&mut self, message: &[u8]) -> ServiceResult {
        // RoutineControl checks its routineIdentifier before its SubFunction (clause 7.7.2,
        // Figure 5, and clause 13.2.4, Figure 31), so the generic SubFunction checks do not apply.
        if message.len() < 4 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let sf = SubFunction {
            value: message[1] & !SUPPRESS_POS_RSP,
            suppress: message[1] & SUPPRESS_POS_RSP != 0,
        };
        let rid = u16::from_be_bytes([message[2], message[3]]);
        // Both routines exist only in the programming session.
        let known = matches!(rid, RID_ERASE_MEMORY | RID_CHECK_PROGRAMMING_DEPENDENCIES);
        if !known || self.session != Session::Programming {
            return Err(Nrc::RequestOutOfRange);
        }
        if rid == RID_ERASE_MEMORY && !self.security_satisfied() {
            return Err(Nrc::SecurityAccessDenied);
        }
        // Only startRoutine; both routines run to completion within the request.
        if sf.value != 0x01 {
            return Err(Nrc::SubFunctionNotSupported);
        }
        let rid_bytes = rid.to_be_bytes();
        match rid {
            RID_ERASE_MEMORY => {
                // The routineControlOptionRecord (erase range) is accepted and ignored:
                // the whole simulated flash is erased.
                if matches!(self.flash, FlashPhase::Transferring { .. }) {
                    return Err(Nrc::ConditionsNotCorrect);
                }
                self.flash = FlashPhase::Erased;
                self.download = None;
                self.image.clear();
                reply(
                    &sf,
                    positive(SID_ROUTINE_CONTROL, &[sf.value, rid_bytes[0], rid_bytes[1]]),
                )
            }
            _ => {
                if message.len() != 4 {
                    return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
                }
                if !matches!(
                    self.flash,
                    FlashPhase::TransferComplete | FlashPhase::Verified
                ) {
                    return Err(Nrc::RequestSequenceError);
                }
                // routineStatusRecord: 0x00 = image correct, 0x01 = image incorrect.
                let status = if self.config.fail_checksum {
                    self.flash = FlashPhase::TransferComplete;
                    0x01
                } else {
                    self.flash = FlashPhase::Verified;
                    0x00
                };
                reply(
                    &sf,
                    positive(
                        SID_ROUTINE_CONTROL,
                        &[sf.value, rid_bytes[0], rid_bytes[1], status],
                    ),
                )
            }
        }
    }

    // ------------------------------------------------------------ 0x34 (clause 14.2)

    fn request_download(&mut self, message: &[u8]) -> ServiceResult {
        if message.len() < 5 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let data_format = message[1];
        let size_len = usize::from(message[2] >> 4);
        let address_len = usize::from(message[2] & 0x0F);
        // No compression or encryption; address and size of 1 to 4 bytes.
        if data_format != 0x00 || !(1..=4).contains(&size_len) || !(1..=4).contains(&address_len) {
            return Err(Nrc::RequestOutOfRange);
        }
        if message.len() != 3 + address_len + size_len {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let address = be_uint(&message[3..3 + address_len]);
        let size = be_uint(&message[3 + address_len..]);
        let flash_end = u64::from(FLASH_START) + u64::from(FLASH_SIZE);
        if size == 0 || address < u64::from(FLASH_START) || address + size > flash_end {
            return Err(Nrc::RequestOutOfRange);
        }
        let (address, size) = (address as u32, size as u32);
        if !self.security_satisfied() {
            return Err(Nrc::SecurityAccessDenied);
        }
        let next_block = match (self.flash, self.download) {
            (FlashPhase::Erased, _) => {
                self.download = Some(Download {
                    start: address,
                    size,
                    received: 0,
                    expected_bsc: 1,
                    last_bsc: None,
                    last_len: 0,
                });
                1
            }
            // Resume: the request must cover exactly the part not yet received.
            (FlashPhase::Interrupted { last_block }, Some(dl))
                if address == dl.start + dl.received && size == dl.size - dl.received =>
            {
                self.download = Some(Download {
                    expected_bsc: 1,
                    last_bsc: None,
                    last_len: 0,
                    ..dl
                });
                last_block + 1
            }
            // A transfer is running, or the flash has not been erased.
            _ => return Err(Nrc::ConditionsNotCorrect),
        };
        self.flash = FlashPhase::Transferring { next_block };
        let max = MAX_BLOCK_LENGTH.to_be_bytes();
        Ok(Some(positive(
            SID_REQUEST_DOWNLOAD,
            &[0x20, max[0], max[1]],
        )))
    }

    // ------------------------------------------------------------ 0x36 (clause 14.4)

    fn transfer_data(&mut self, message: &[u8]) -> ServiceResult {
        if message.len() < 2 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let (FlashPhase::Transferring { next_block }, Some(mut dl)) = (self.flash, self.download)
        else {
            return Err(Nrc::RequestSequenceError);
        };
        // A download needs at least one data byte and must fit maxNumberOfBlockLength.
        if message.len() < 3 || message.len() > usize::from(MAX_BLOCK_LENGTH) {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        let bsc = message[1];
        let data = &message[2..];
        // A repeated request (its response was lost) is answered again without storing twice.
        // A block that reuses the previous counter with different data is a sequence error.
        if dl.last_bsc == Some(bsc) {
            let last = &self.image[self.image.len() - dl.last_len as usize..];
            if last != data {
                return Err(Nrc::WrongBlockSequenceCounter);
            }
            return Ok(Some(positive(SID_TRANSFER_DATA, &[bsc])));
        }
        if dl.received == dl.size {
            return Err(Nrc::RequestSequenceError);
        }
        if u64::from(dl.received) + data.len() as u64 > u64::from(dl.size) {
            return Err(Nrc::TransferDataSuspended);
        }
        if bsc != dl.expected_bsc {
            return Err(Nrc::WrongBlockSequenceCounter);
        }
        if self.config.drop_at_block == Some(next_block) {
            // Injected interruption (once): the block is lost and the ECU goes quiet.
            self.config.drop_at_block = None;
            self.silent = true;
            return Ok(None);
        }
        self.image.extend_from_slice(data);
        dl.received += data.len() as u32;
        dl.last_bsc = Some(bsc);
        dl.last_len = data.len() as u32;
        dl.expected_bsc = bsc.wrapping_add(1);
        self.download = Some(dl);
        self.flash = FlashPhase::Transferring {
            next_block: next_block + 1,
        };
        Ok(Some(positive(SID_TRANSFER_DATA, &[bsc])))
    }

    // ------------------------------------------------------------ 0x37 (clause 14.5)

    fn request_transfer_exit(&mut self, message: &[u8]) -> ServiceResult {
        // A download interrupted after its last block can still be closed: all data is stored.
        let (FlashPhase::Transferring { .. } | FlashPhase::Interrupted { .. }, Some(dl)) =
            (self.flash, self.download)
        else {
            return Err(Nrc::RequestSequenceError);
        };
        if dl.received < dl.size {
            return Err(Nrc::RequestSequenceError);
        }
        // No requestTransferExitRequestParameterRecord is supported.
        if message.len() != 1 {
            return Err(Nrc::IncorrectMessageLengthOrInvalidFormat);
        }
        self.download = None;
        self.flash = FlashPhase::TransferComplete;
        Ok(Some(positive(SID_REQUEST_TRANSFER_EXIT, &[])))
    }
}
