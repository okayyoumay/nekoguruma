use super::*;

/// IO-control payload for plain `UNUM32` values.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoUnum32Data(pub UNUM32);

impl BorrowedIoUnum32Data {
    /// Returns scalar value.
    pub fn value(&self) -> UNUM32 {
        self.0
    }

    /// Sets scalar value.
    pub fn set_value(&mut self, value: UNUM32) {
        self.0 = value;
    }
}

/// IO-control payload for programming voltage.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoProgVoltageData(pub PDU_IO_PROG_VOLTAGE_DATA);

impl BorrowedIoProgVoltageData {
    /// Returns programming voltage in mV.
    pub fn prog_voltage_mv(&self) -> UNUM32 {
        self.0.ProgVoltage_mv
    }

    /// Sets programming voltage in mV.
    pub fn set_prog_voltage_mv(&mut self, value: UNUM32) {
        self.0.ProgVoltage_mv = value;
    }

    /// Returns target DLC pin.
    pub fn pin_on_dlc(&self) -> UNUM32 {
        self.0.PinOnDLC
    }

    /// Sets target DLC pin.
    pub fn set_pin_on_dlc(&mut self, value: UNUM32) {
        self.0.PinOnDLC = value;
    }
}

/// IO-control payload for byte arrays.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoByteArrayData(pub PDU_IO_BYTEARRAY_DATA);

impl BorrowedIoByteArrayData {
    /// Returns byte-array length.
    pub fn data_size(&self) -> UNUM32 {
        self.0.DataSize
    }

    /// Returns immutable byte-array payload.
    pub fn data_bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pData.cast::<u8>(),
                self.0.DataSize,
                "io bytearray data",
            )
        }
    }

    /// Returns mutable byte-array payload.
    pub fn data_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pData.cast::<u8>(),
                self.0.DataSize,
                "io bytearray data",
            )
        }
    }
}

/// One IO filter configuration entry.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoFilterData(pub PDU_IO_FILTER_DATA);

impl BorrowedIoFilterData {
    /// Returns filter type code.
    pub fn filter_type(&self) -> UNUM32 {
        self.0.FilterType.0 as UNUM32
    }

    /// Returns filter index/number.
    pub fn filter_number(&self) -> UNUM32 {
        self.0.FilterNumber
    }

    /// Returns compare-size setting.
    pub fn filter_compare_size(&self) -> UNUM32 {
        self.0.FilterCompareSize
    }

    /// Returns filter mask message bytes.
    pub fn filter_mask_message(&self) -> &[u8; 12] {
        &self.0.FilterMaskMessage
    }

    /// Returns mutable filter mask message bytes.
    pub fn filter_mask_message_mut(&mut self) -> &mut [u8; 12] {
        &mut self.0.FilterMaskMessage
    }

    /// Returns filter pattern message bytes.
    pub fn filter_pattern_message(&self) -> &[u8; 12] {
        &self.0.FilterPatternMessage
    }

    /// Returns mutable filter pattern message bytes.
    pub fn filter_pattern_message_mut(&mut self) -> &mut [u8; 12] {
        &mut self.0.FilterPatternMessage
    }
}

/// IO-control payload containing filter list.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoFilterListData(pub PDU_IO_FILTER_LIST);

impl BorrowedIoFilterListData {
    /// Returns number of filter entries.
    pub fn num_filter_entries(&self) -> UNUM32 {
        self.0.NumFilterEntries
    }

    /// Returns immutable filter entries.
    pub fn entries(&self) -> Result<&[BorrowedIoFilterData], DPduApiError> {
        let raw = unsafe {
            ffi_slice(
                self.0.pFilterData,
                self.0.NumFilterEntries,
                "io filter entries",
            )
        }?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedIoFilterData, raw.len())
        })
    }

    /// Returns mutable filter entries.
    pub fn entries_mut(&mut self) -> Result<&mut [BorrowedIoFilterData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pFilterData as *mut BorrowedIoFilterData,
                self.0.NumFilterEntries,
                "io filter entries",
            )
        }
    }
}

/// IO-control payload for event queue properties.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoEventQueuePropertyData(pub PDU_IO_EVENT_QUEUE_PROPERTY_DATA);

impl BorrowedIoEventQueuePropertyData {
    /// Returns queue size.
    pub fn queue_size(&self) -> UNUM32 {
        self.0.QueueSize
    }

    /// Sets queue size.
    pub fn set_queue_size(&mut self, value: UNUM32) {
        self.0.QueueSize = value;
    }

    /// Returns queue mode code.
    pub fn queue_mode(&self) -> T_PDU_QUEUE_MODE {
        self.0.QueueMode
    }

    /// Sets queue mode.
    pub fn set_queue_mode(&mut self, value: T_PDU_QUEUE_MODE) {
        self.0.QueueMode = value;
    }
}

/// IP address entry used by IO vehicle-id request payloads.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoIpAddrInfo(pub PDU_IP_ADDR_INFO);

impl BorrowedIoIpAddrInfo {
    /// Returns IP version (`4` or `6`).
    pub fn ip_version(&self) -> UNUM32 {
        self.0.IpVersion
    }

    fn address_size(&self) -> Result<UNUM32, DPduApiError> {
        match self.0.IpVersion {
            4 => Ok(4),
            6 => Ok(16),
            _ => Err(DPduApiError::Unsupported("ip address version")),
        }
    }

    /// Returns immutable address bytes.
    pub fn address_bytes(&self) -> Result<&[u8], DPduApiError> {
        let len = self.address_size()?;
        unsafe { ffi_slice(self.0.pAddress.cast::<u8>(), len, "ip address bytes") }
    }

    /// Returns mutable address bytes.
    pub fn address_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        let len = self.address_size()?;
        unsafe { ffi_slice_mut(self.0.pAddress.cast::<u8>(), len, "ip address bytes") }
    }
}

/// IO-control payload for vehicle-id request settings.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoVehicleIdRequestData(pub PDU_IO_VEHICLE_ID_REQUEST);

impl BorrowedIoVehicleIdRequestData {
    /// Returns preselection mode.
    pub fn preselection_mode(&self) -> UNUM32 {
        self.0.PreselectionMode
    }

    /// Sets preselection mode.
    pub fn set_preselection_mode(&mut self, value: UNUM32) {
        self.0.PreselectionMode = value;
    }

    /// Returns optional preselection value if valid UTF-8.
    pub fn preselection_value(&self) -> Option<&str> {
        if self.0.PreselectionValue.is_null() {
            None
        } else {
            unsafe {
                std::ffi::CStr::from_ptr(self.0.PreselectionValue)
                    .to_str()
                    .ok()
            }
        }
    }

    /// Returns combination mode.
    pub fn combination_mode(&self) -> UNUM32 {
        self.0.CombinationMode
    }

    /// Sets combination mode.
    pub fn set_combination_mode(&mut self, value: UNUM32) {
        self.0.CombinationMode = value;
    }

    /// Returns vehicle discovery time.
    pub fn vehicle_discovery_time(&self) -> UNUM32 {
        self.0.VehicleDiscoveryTime
    }

    /// Sets vehicle discovery time.
    pub fn set_vehicle_discovery_time(&mut self, value: UNUM32) {
        self.0.VehicleDiscoveryTime = value;
    }

    /// Returns number of destination addresses.
    pub fn num_destination_addresses(&self) -> UNUM32 {
        self.0.NumDestinationAddresses
    }

    /// Returns immutable destination address entries.
    pub fn destination_addresses(&self) -> Result<&[BorrowedIoIpAddrInfo], DPduApiError> {
        let raw = unsafe {
            ffi_slice(
                self.0.pDestinationAddresses,
                self.0.NumDestinationAddresses,
                "vehicle id destination addresses",
            )
        }?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedIoIpAddrInfo, raw.len())
        })
    }

    /// Returns mutable destination address entries.
    pub fn destination_addresses_mut(
        &mut self,
    ) -> Result<&mut [BorrowedIoIpAddrInfo], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pDestinationAddresses as *mut BorrowedIoIpAddrInfo,
                self.0.NumDestinationAddresses,
                "vehicle id destination addresses",
            )
        }
    }
}

/// IO-control payload for Ethernet switch state.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoEthSwitchStateData(pub PDU_IO_ETH_SWITCH_STATE);

impl BorrowedIoEthSwitchStateData {
    /// Returns Ethernet sense-state value.
    pub fn ethernet_sense_state(&self) -> UNUM32 {
        self.0.EthernetSenseState
    }

    /// Sets Ethernet sense-state value.
    pub fn set_ethernet_sense_state(&mut self, value: UNUM32) {
        self.0.EthernetSenseState = value;
    }

    /// Returns Ethernet activation pin number.
    pub fn ethernet_act_pin_number(&self) -> UNUM32 {
        self.0.EthernetActPinNumber
    }

    /// Sets Ethernet activation pin number.
    pub fn set_ethernet_act_pin_number(&mut self, value: UNUM32) {
        self.0.EthernetActPinNumber = value;
    }
}

/// IO-control payload for entity address.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoEntityAddressData(pub PDU_IO_ENTITY_ADDRESS_DATA);

impl BorrowedIoEntityAddressData {
    /// Returns logical address.
    pub fn logical_address(&self) -> UNUM32 {
        self.0.LogicalAddress
    }

    /// Sets logical address.
    pub fn set_logical_address(&mut self, value: UNUM32) {
        self.0.LogicalAddress = value;
    }

    /// Returns DoIP control timeout.
    pub fn doip_ctrl_timeout(&self) -> UNUM32 {
        self.0.DoIPCtrlTimeout
    }

    /// Sets DoIP control timeout.
    pub fn set_doip_ctrl_timeout(&mut self, value: UNUM32) {
        self.0.DoIPCtrlTimeout = value;
    }
}

/// IO-control payload for entity status.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoEntityStatusData(pub PDU_IO_ENTITY_STATUS_DATA);

impl BorrowedIoEntityStatusData {
    /// Returns entity type.
    pub fn entity_type(&self) -> UNUM32 {
        self.0.EntityType
    }

    /// Sets entity type.
    pub fn set_entity_type(&mut self, value: UNUM32) {
        self.0.EntityType = value;
    }

    /// Returns max TCP clients.
    pub fn tcp_clients_max(&self) -> UNUM32 {
        self.0.TcpClientsMax
    }

    /// Sets max TCP clients.
    pub fn set_tcp_clients_max(&mut self, value: UNUM32) {
        self.0.TcpClientsMax = value;
    }

    /// Returns current TCP clients.
    pub fn tcp_clients(&self) -> UNUM32 {
        self.0.TcpClients
    }

    /// Sets current TCP clients.
    pub fn set_tcp_clients(&mut self, value: UNUM32) {
        self.0.TcpClients = value;
    }

    /// Returns maximum payload size.
    pub fn max_data_size(&self) -> UNUM32 {
        self.0.MaxDataSize
    }

    /// Sets maximum payload size.
    pub fn set_max_data_size(&mut self, value: UNUM32) {
        self.0.MaxDataSize = value;
    }
}

/// IO-control payload for a single TLS certificate.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoTlsCertData(pub PDU_IO_TLS_CERT_DATA);

impl BorrowedIoTlsCertData {
    /// Returns certificate length.
    pub fn cert_len(&self) -> UNUM32 {
        self.0.CertLen
    }

    /// Returns immutable certificate bytes.
    pub fn cert_bytes(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.CertBuffer.cast::<u8>(),
                self.0.CertLen,
                "tls cert bytes",
            )
        }
    }

    /// Returns mutable certificate bytes.
    pub fn cert_bytes_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.CertBuffer.cast::<u8>(),
                self.0.CertLen,
                "tls cert bytes",
            )
        }
    }
}

/// IO-control payload for one TLS certificate chain.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoTlsCertChainData(pub PDU_IO_TLS_CERT_CHAIN_DATA);

impl BorrowedIoTlsCertChainData {
    /// Returns number of certificates in chain.
    pub fn num_certificates(&self) -> UNUM32 {
        self.0.NumCertificates
    }

    /// Returns immutable certificate entries.
    pub fn certificates(&self) -> Result<&[BorrowedIoTlsCertData], DPduApiError> {
        let raw = unsafe {
            ffi_slice(
                self.0.pTlsCertData,
                self.0.NumCertificates,
                "tls cert chain data",
            )
        }?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedIoTlsCertData, raw.len())
        })
    }

    /// Returns mutable certificate entries.
    pub fn certificates_mut(&mut self) -> Result<&mut [BorrowedIoTlsCertData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pTlsCertData as *mut BorrowedIoTlsCertData,
                self.0.NumCertificates,
                "tls cert chain data",
            )
        }
    }
}

/// IO-control payload containing certificate chains.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedIoTlsCertificateData(pub PDU_IO_TLS_CERTIFICATE);

impl BorrowedIoTlsCertificateData {
    /// Returns number of certificate chains.
    pub fn num_cert_chains(&self) -> UNUM32 {
        self.0.NumCertChains
    }

    /// Returns immutable certificate chain entries.
    pub fn cert_chains(&self) -> Result<&[BorrowedIoTlsCertChainData], DPduApiError> {
        let raw = unsafe {
            ffi_slice(
                self.0.pTlsCertChainData,
                self.0.NumCertChains,
                "tls cert chain list",
            )
        }?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedIoTlsCertChainData, raw.len())
        })
    }

    /// Returns mutable certificate chain entries.
    pub fn cert_chains_mut(&mut self) -> Result<&mut [BorrowedIoTlsCertChainData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pTlsCertChainData as *mut BorrowedIoTlsCertChainData,
                self.0.NumCertChains,
                "tls cert chain list",
            )
        }
    }
}
