use super::*;

/// Visitor for immutable `BorrowedDataItem` payloads.
pub trait DataItemVisitor {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_io_unum32(
        &mut self,
        data: &BorrowedIoUnum32Data,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_prog_voltage(
        &mut self,
        data: &BorrowedIoProgVoltageData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_bytearray(
        &mut self,
        data: &BorrowedIoByteArrayData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_filter(
        &mut self,
        data: &BorrowedIoFilterListData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_event_queue_property(
        &mut self,
        data: &BorrowedIoEventQueuePropertyData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_vehicle_id_request(
        &mut self,
        data: &BorrowedIoVehicleIdRequestData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_eth_switch_state(
        &mut self,
        data: &BorrowedIoEthSwitchStateData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_entity_address(
        &mut self,
        data: &BorrowedIoEntityAddressData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_entity_status(
        &mut self,
        data: &BorrowedIoEntityStatusData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_tls_certificate(
        &mut self,
        data: &BorrowedIoTlsCertificateData,
    ) -> Result<Self::Output, DPduApiError>;
}

/// Visitor for mutable `BorrowedDataItem` payloads.
pub trait DataItemVisitorMut {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_io_unum32(
        &mut self,
        data: &mut BorrowedIoUnum32Data,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_prog_voltage(
        &mut self,
        data: &mut BorrowedIoProgVoltageData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_bytearray(
        &mut self,
        data: &mut BorrowedIoByteArrayData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_filter(
        &mut self,
        data: &mut BorrowedIoFilterListData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_event_queue_property(
        &mut self,
        data: &mut BorrowedIoEventQueuePropertyData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_vehicle_id_request(
        &mut self,
        data: &mut BorrowedIoVehicleIdRequestData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_eth_switch_state(
        &mut self,
        data: &mut BorrowedIoEthSwitchStateData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_entity_address(
        &mut self,
        data: &mut BorrowedIoEntityAddressData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_entity_status(
        &mut self,
        data: &mut BorrowedIoEntityStatusData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_io_tls_certificate(
        &mut self,
        data: &mut BorrowedIoTlsCertificateData,
    ) -> Result<Self::Output, DPduApiError>;
}

/// Visitor for immutable event-item payloads.
pub trait EventItemVisitor {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_result(&mut self, data: &BorrowedResultData) -> Result<Self::Output, DPduApiError>;
    fn visit_status(&mut self, data: &BorrowedStatusData) -> Result<Self::Output, DPduApiError>;
    fn visit_error(&mut self, data: &BorrowedErrorData) -> Result<Self::Output, DPduApiError>;
    fn visit_info(&mut self, data: &BorrowedInfoData) -> Result<Self::Output, DPduApiError>;
}

/// Visitor for mutable event-item payloads.
pub trait EventItemVisitorMut {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_result(&mut self, data: &mut BorrowedResultData)
    -> Result<Self::Output, DPduApiError>;
    fn visit_status(&mut self, data: &mut BorrowedStatusData)
    -> Result<Self::Output, DPduApiError>;
    fn visit_error(&mut self, data: &mut BorrowedErrorData) -> Result<Self::Output, DPduApiError>;
    fn visit_info(&mut self, data: &mut BorrowedInfoData) -> Result<Self::Output, DPduApiError>;
}

/// Visitor for immutable parameter-item payloads.
pub trait ParamItemVisitor {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_unum8(&mut self, data: &u8) -> Result<Self::Output, DPduApiError>;
    fn visit_snum8(&mut self, data: &i8) -> Result<Self::Output, DPduApiError>;
    fn visit_unum16(&mut self, data: &u16) -> Result<Self::Output, DPduApiError>;
    fn visit_snum16(&mut self, data: &i16) -> Result<Self::Output, DPduApiError>;
    fn visit_unum32(&mut self, data: &u32) -> Result<Self::Output, DPduApiError>;
    fn visit_snum32(&mut self, data: &i32) -> Result<Self::Output, DPduApiError>;
    fn visit_bytefield(
        &mut self,
        data: &BorrowedBytefieldData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_structfield(
        &mut self,
        data: &BorrowedStructfieldData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_longfield(
        &mut self,
        data: &BorrowedLongfieldData,
    ) -> Result<Self::Output, DPduApiError>;
}

/// Visitor for mutable parameter-item payloads.
pub trait ParamItemVisitorMut {
    /// Return type produced by visitor methods.
    type Output;

    fn visit_unum8(&mut self, data: &mut u8) -> Result<Self::Output, DPduApiError>;
    fn visit_snum8(&mut self, data: &mut i8) -> Result<Self::Output, DPduApiError>;
    fn visit_unum16(&mut self, data: &mut u16) -> Result<Self::Output, DPduApiError>;
    fn visit_snum16(&mut self, data: &mut i16) -> Result<Self::Output, DPduApiError>;
    fn visit_unum32(&mut self, data: &mut u32) -> Result<Self::Output, DPduApiError>;
    fn visit_snum32(&mut self, data: &mut i32) -> Result<Self::Output, DPduApiError>;
    fn visit_bytefield(
        &mut self,
        data: &mut BorrowedBytefieldData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_structfield(
        &mut self,
        data: &mut BorrowedStructfieldData,
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_longfield(
        &mut self,
        data: &mut BorrowedLongfieldData,
    ) -> Result<Self::Output, DPduApiError>;
}
