use super::*;

/// Event/result flag payload wrapper.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedFlagData(pub PDU_FLAG_DATA);

impl BorrowedFlagData {
    /// Returns number of flag bytes.
    pub fn num_flag_bytes(&self) -> UNUM32 {
        self.0.NumFlagBytes
    }

    /// Returns immutable flag bytes.
    pub fn bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pFlagData.cast::<u8>(),
                self.0.NumFlagBytes,
                "event flag bytes",
            )
        }
    }

    /// Returns mutable flag bytes.
    pub fn bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pFlagData.cast::<u8>(),
                self.0.NumFlagBytes,
                "event flag bytes",
            )
        }
    }
}

/// Extra-info payload for result events.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedExtraInfoData(pub PDU_EXTRA_INFO);

impl BorrowedExtraInfoData {
    /// Returns number of header bytes.
    pub fn num_header_bytes(&self) -> UNUM32 {
        self.0.NumHeaderBytes
    }

    /// Returns number of footer bytes.
    pub fn num_footer_bytes(&self) -> UNUM32 {
        self.0.NumFooterBytes
    }

    /// Returns immutable header bytes.
    pub fn header_bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pHeaderBytes.cast::<u8>(),
                self.0.NumHeaderBytes,
                "event header bytes",
            )
        }
    }

    /// Returns mutable header bytes.
    pub fn header_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pHeaderBytes.cast::<u8>(),
                self.0.NumHeaderBytes,
                "event header bytes",
            )
        }
    }

    /// Returns immutable footer bytes.
    pub fn footer_bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pFooterBytes.cast::<u8>(),
                self.0.NumFooterBytes,
                "event footer bytes",
            )
        }
    }

    /// Returns mutable footer bytes.
    pub fn footer_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pFooterBytes.cast::<u8>(),
                self.0.NumFooterBytes,
                "event footer bytes",
            )
        }
    }
}

/// Result-event payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedResultData(pub PDU_RESULT_DATA);

impl BorrowedResultData {
    /// Returns received flag data.
    pub fn rx_flag(&self) -> &BorrowedFlagData {
        unsafe { &*(&self.0.RxFlag as *const PDU_FLAG_DATA as *const BorrowedFlagData) }
    }

    /// Returns mutable received flag data.
    pub fn rx_flag_mut(&mut self) -> &mut BorrowedFlagData {
        unsafe { &mut *(&mut self.0.RxFlag as *mut PDU_FLAG_DATA as *mut BorrowedFlagData) }
    }

    /// Returns unique response identifier.
    pub fn unique_resp_identifier(&self) -> UNUM32 {
        self.0.UniqueRespIdentifier
    }

    /// Returns acceptance id.
    pub fn acceptance_id(&self) -> UNUM32 {
        self.0.AcceptanceId
    }

    /// Returns timestamp flag data.
    pub fn timestamp_flags(&self) -> &BorrowedFlagData {
        unsafe { &*(&self.0.TimestampFlags as *const PDU_FLAG_DATA as *const BorrowedFlagData) }
    }

    /// Returns mutable timestamp flag data.
    pub fn timestamp_flags_mut(&mut self) -> &mut BorrowedFlagData {
        unsafe { &mut *(&mut self.0.TimestampFlags as *mut PDU_FLAG_DATA as *mut BorrowedFlagData) }
    }

    /// Returns tx-done timestamp.
    pub fn tx_msg_done_timestamp(&self) -> UNUM32 {
        self.0.TxMsgDoneTimestamp
    }

    /// Returns start-message timestamp.
    pub fn start_msg_timestamp(&self) -> UNUM32 {
        self.0.StartMsgTimestamp
    }

    /// Returns optional extra-info payload.
    pub fn extra_info(&self) -> Option<&BorrowedExtraInfoData> {
        unsafe { self.0.pExtraInfo.as_ref() }.map(|data| unsafe {
            &*(data as *const PDU_EXTRA_INFO as *const BorrowedExtraInfoData)
        })
    }

    /// Returns optional mutable extra-info payload.
    pub fn extra_info_mut(&mut self) -> Option<&mut BorrowedExtraInfoData> {
        unsafe { self.0.pExtraInfo.as_mut() }.map(|data| unsafe {
            &mut *(data as *mut PDU_EXTRA_INFO as *mut BorrowedExtraInfoData)
        })
    }

    /// Returns number of payload data bytes.
    pub fn num_data_bytes(&self) -> UNUM32 {
        self.0.NumDataBytes
    }

    /// Returns immutable result payload bytes.
    pub fn data_bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pDataBytes.cast::<u8>(),
                self.0.NumDataBytes,
                "result payload bytes",
            )
        }
    }

    /// Returns mutable result payload bytes.
    pub fn data_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pDataBytes.cast::<u8>(),
                self.0.NumDataBytes,
                "result payload bytes",
            )
        }
    }
}

/// Status-event payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedStatusData(pub PDU_STATUS_DATA);

impl BorrowedStatusData {
    /// Returns status code.
    pub fn status(&self) -> UNUM32 {
        self.0.0 as UNUM32
    }

    /// Sets status code.
    pub fn set_status(&mut self, value: T_PDU_STATUS) {
        self.0 = value;
    }
}

/// Error-event payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedErrorData(pub PDU_ERROR_DATA);

impl BorrowedErrorData {
    /// Returns error-code identifier.
    pub fn error_code_id(&self) -> T_PDU_ERR_EVT {
        self.0.ErrorCodeId
    }

    /// Sets error-code identifier.
    pub fn set_error_code_id(&mut self, value: T_PDU_ERR_EVT) {
        self.0.ErrorCodeId = value;
    }

    /// Returns extra error-info identifier.
    pub fn extra_error_info_id(&self) -> UNUM32 {
        self.0.ExtraErrorInfoId
    }

    /// Sets extra error-info identifier.
    pub fn set_extra_error_info_id(&mut self, value: UNUM32) {
        self.0.ExtraErrorInfoId = value;
    }
}

/// Info-event payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedInfoData(pub PDU_INFO_DATA);

impl BorrowedInfoData {
    /// Returns info code.
    pub fn info_code(&self) -> T_PDU_INFO {
        self.0.InfoCode
    }

    /// Sets info code.
    pub fn set_info_code(&mut self, value: T_PDU_INFO) {
        self.0.InfoCode = value;
    }

    /// Returns extra info data value.
    pub fn extra_info_data(&self) -> UNUM32 {
        self.0.ExtraInfoData
    }

    /// Sets extra info data value.
    pub fn set_extra_info_data(&mut self, value: UNUM32) {
        self.0.ExtraInfoData = value;
    }
}
