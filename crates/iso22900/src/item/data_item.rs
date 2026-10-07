use super::*;

/// RAII wrapper for API-owned item pointers returned by D-PDU functions.
///
/// On drop, `PDUDestroyItem` is called for the underlying raw pointer.
pub struct ApiItem<'a, Item> {
    item: &'a Item,
    sys: &'a DPduApiSys,
    raw: NonNull<PDU_ITEM>,
    _marker: PhantomData<&'a ()>,
}

impl<'a, Item> ApiItem<'a, Item> {
    /// Wraps an item pointer that a D-PDU function returned through an output parameter.
    ///
    /// The null check comes before the pointer is turned into a reference, so a library that
    /// reports success without filling in the item yields `NullPointer` instead of undefined
    /// behaviour.
    ///
    /// # Safety
    ///
    /// `raw` must be null or point to a live item of type `Item` that stays valid until it is
    /// passed to `PDUDestroyItem`.
    pub(crate) unsafe fn from_raw(
        sys: &'a DPduApiSys,
        raw: *mut PDU_ITEM,
    ) -> Result<Self, DPduApiError> {
        let raw = NonNull::new(raw).ok_or(DPduApiError::NullPointer("PDU item"))?;
        // SAFETY: `raw` is non-null, and the caller guarantees it points to a live `Item`.
        let item = unsafe { raw.cast::<Item>().as_ref() };
        Ok(Self {
            item,
            sys,
            raw,
            _marker: PhantomData,
        })
    }

    /// Returns the borrowed typed item payload.
    pub fn borrowed(&self) -> &Item {
        self.item
    }
}

impl<'a, Item> std::ops::Deref for ApiItem<'a, Item> {
    type Target = Item;

    fn deref(&self) -> &Self::Target {
        self.item
    }
}

impl<'a, Item> Drop for ApiItem<'a, Item> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.sys.PDUDestroyItem(self.raw.as_ptr());
        }
    }
}

/// Borrowed wrapper around `PDU_DATA_ITEM`.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedDataItem(pub PDU_DATA_ITEM);

impl BorrowedDataItem {
    /// Returns raw item type code.
    pub fn item_type(&self) -> T_PDU_IT {
        self.0.ItemType
    }

    /// Interprets `pData` as `T` and returns an immutable reference.
    pub fn data<T>(&self) -> Result<&T, DPduApiError> {
        unsafe { (self.0.pData as *const T).as_ref() }
            .ok_or(DPduApiError::NullPointer("data item data"))
    }

    /// Interprets `pData` as `T` and returns a mutable reference.
    pub fn data_mut<T>(&mut self) -> Result<&mut T, DPduApiError> {
        unsafe { (self.0.pData as *mut T).as_mut() }
            .ok_or(DPduApiError::NullPointer("data item data"))
    }

    /// Dispatches immutable payload handling to a typed visitor.
    pub fn visit<V: DataItemVisitor>(&self, visitor: &mut V) -> Result<V::Output, DPduApiError> {
        match self.0.ItemType {
            E_PDU_IT::PDU_IT_IO_UNUM32 => {
                let data = self.data::<UNUM32>()?;
                let wrapped = transparent_ref::<UNUM32, BorrowedIoUnum32Data>(data);
                visitor.visit_io_unum32(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE => {
                let data = self.data::<PDU_IO_PROG_VOLTAGE_DATA>()?;
                let wrapped =
                    transparent_ref::<PDU_IO_PROG_VOLTAGE_DATA, BorrowedIoProgVoltageData>(data);
                visitor.visit_io_prog_voltage(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_BYTEARRAY => {
                let data = self.data::<PDU_IO_BYTEARRAY_DATA>()?;
                let wrapped =
                    transparent_ref::<PDU_IO_BYTEARRAY_DATA, BorrowedIoByteArrayData>(data);
                visitor.visit_io_bytearray(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_FILTER => {
                let data = self.data::<PDU_IO_FILTER_LIST>()?;
                let wrapped = transparent_ref::<PDU_IO_FILTER_LIST, BorrowedIoFilterListData>(data);
                visitor.visit_io_filter(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY => {
                let data = self.data::<PDU_IO_EVENT_QUEUE_PROPERTY_DATA>()?;
                let wrapped = transparent_ref::<
                    PDU_IO_EVENT_QUEUE_PROPERTY_DATA,
                    BorrowedIoEventQueuePropertyData,
                >(data);
                visitor.visit_io_event_queue_property(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST => {
                let data = self.data::<PDU_IO_VEHICLE_ID_REQUEST>()?;
                let wrapped = transparent_ref::<
                    PDU_IO_VEHICLE_ID_REQUEST,
                    BorrowedIoVehicleIdRequestData,
                >(data);
                visitor.visit_io_vehicle_id_request(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ETH_SWITCH_STATE => {
                let data = self.data::<PDU_IO_ETH_SWITCH_STATE>()?;
                let wrapped =
                    transparent_ref::<PDU_IO_ETH_SWITCH_STATE, BorrowedIoEthSwitchStateData>(data);
                visitor.visit_io_eth_switch_state(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_ADDRESS => {
                let data = self.data::<PDU_IO_ENTITY_ADDRESS_DATA>()?;
                let wrapped = transparent_ref::<
                    PDU_IO_ENTITY_ADDRESS_DATA,
                    BorrowedIoEntityAddressData,
                >(data);
                visitor.visit_io_entity_address(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_STATUS => {
                let data = self.data::<PDU_IO_ENTITY_STATUS_DATA>()?;
                let wrapped =
                    transparent_ref::<PDU_IO_ENTITY_STATUS_DATA, BorrowedIoEntityStatusData>(data);
                visitor.visit_io_entity_status(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_TLS_CERTIFICATE => {
                let data = self.data::<PDU_IO_TLS_CERTIFICATE>()?;
                let wrapped =
                    transparent_ref::<PDU_IO_TLS_CERTIFICATE, BorrowedIoTlsCertificateData>(data);
                visitor.visit_io_tls_certificate(wrapped)
            }
            _ => Err(DPduApiError::NullPointer("unknown data item type")),
        }
    }

    /// Dispatches mutable payload handling to a typed visitor.
    pub fn visit_mut<V: DataItemVisitorMut>(
        &mut self,
        visitor: &mut V,
    ) -> Result<V::Output, DPduApiError> {
        match self.0.ItemType {
            E_PDU_IT::PDU_IT_IO_UNUM32 => {
                let data = self.data_mut::<UNUM32>()?;
                let wrapped = transparent_mut::<UNUM32, BorrowedIoUnum32Data>(data);
                visitor.visit_io_unum32(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE => {
                let data = self.data_mut::<PDU_IO_PROG_VOLTAGE_DATA>()?;
                let wrapped =
                    transparent_mut::<PDU_IO_PROG_VOLTAGE_DATA, BorrowedIoProgVoltageData>(data);
                visitor.visit_io_prog_voltage(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_BYTEARRAY => {
                let data = self.data_mut::<PDU_IO_BYTEARRAY_DATA>()?;
                let wrapped =
                    transparent_mut::<PDU_IO_BYTEARRAY_DATA, BorrowedIoByteArrayData>(data);
                visitor.visit_io_bytearray(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_FILTER => {
                let data = self.data_mut::<PDU_IO_FILTER_LIST>()?;
                let wrapped = transparent_mut::<PDU_IO_FILTER_LIST, BorrowedIoFilterListData>(data);
                visitor.visit_io_filter(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY => {
                let data = self.data_mut::<PDU_IO_EVENT_QUEUE_PROPERTY_DATA>()?;
                let wrapped = transparent_mut::<
                    PDU_IO_EVENT_QUEUE_PROPERTY_DATA,
                    BorrowedIoEventQueuePropertyData,
                >(data);
                visitor.visit_io_event_queue_property(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST => {
                let data = self.data_mut::<PDU_IO_VEHICLE_ID_REQUEST>()?;
                let wrapped = transparent_mut::<
                    PDU_IO_VEHICLE_ID_REQUEST,
                    BorrowedIoVehicleIdRequestData,
                >(data);
                visitor.visit_io_vehicle_id_request(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ETH_SWITCH_STATE => {
                let data = self.data_mut::<PDU_IO_ETH_SWITCH_STATE>()?;
                let wrapped =
                    transparent_mut::<PDU_IO_ETH_SWITCH_STATE, BorrowedIoEthSwitchStateData>(data);
                visitor.visit_io_eth_switch_state(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_ADDRESS => {
                let data = self.data_mut::<PDU_IO_ENTITY_ADDRESS_DATA>()?;
                let wrapped = transparent_mut::<
                    PDU_IO_ENTITY_ADDRESS_DATA,
                    BorrowedIoEntityAddressData,
                >(data);
                visitor.visit_io_entity_address(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_STATUS => {
                let data = self.data_mut::<PDU_IO_ENTITY_STATUS_DATA>()?;
                let wrapped =
                    transparent_mut::<PDU_IO_ENTITY_STATUS_DATA, BorrowedIoEntityStatusData>(data);
                visitor.visit_io_entity_status(wrapped)
            }
            E_PDU_IT::PDU_IT_IO_TLS_CERTIFICATE => {
                let data = self.data_mut::<PDU_IO_TLS_CERTIFICATE>()?;
                let wrapped =
                    transparent_mut::<PDU_IO_TLS_CERTIFICATE, BorrowedIoTlsCertificateData>(data);
                visitor.visit_io_tls_certificate(wrapped)
            }
            _ => Err(DPduApiError::NullPointer("unknown data item type")),
        }
    }
}

#[derive(Debug)]
enum DataItemStorage {
    U32(Box<UNUM32>),
    ProgVoltage(Box<PDU_IO_PROG_VOLTAGE_DATA>),
    ByteArray {
        meta: Box<PDU_IO_BYTEARRAY_DATA>,
        bytes: Vec<u8>,
    },
    FilterList {
        meta: Box<PDU_IO_FILTER_LIST>,
        filters: Vec<PDU_IO_FILTER_DATA>,
    },
    EventQueueProperty(Box<PDU_IO_EVENT_QUEUE_PROPERTY_DATA>),
    VehicleIdRequest {
        meta: Box<PDU_IO_VEHICLE_ID_REQUEST>,
        preselection: Option<CString>,
        destination_bytes: Vec<Vec<u8>>,
        destinations: Vec<PDU_IP_ADDR_INFO>,
    },
    EthSwitchState(Box<PDU_IO_ETH_SWITCH_STATE>),
    EntityAddress(Box<PDU_IO_ENTITY_ADDRESS_DATA>),
    EntityStatus(Box<PDU_IO_ENTITY_STATUS_DATA>),
    TlsCertificate {
        meta: Box<PDU_IO_TLS_CERTIFICATE>,
        chains: Vec<PDU_IO_TLS_CERT_CHAIN_DATA>,
        certs: Vec<PDU_IO_TLS_CERT_DATA>,
        cert_bytes: Vec<Vec<u8>>,
    },
    Unsupported,
}

/// Owned data-item wrapper with backing storage for FFI pointers.
#[derive(Debug)]
pub struct OwnedDataItem {
    borrowed: BorrowedDataItem,
    storage: DataItemStorage,
}

impl OwnedDataItem {
    /// Builds an IO-UNUM32 data item.
    pub fn from_unum32(value: UNUM32) -> Self {
        let mut scalar = Box::new(value);
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_UNUM32,
            pData: (&mut *scalar as *mut UNUM32).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::U32(scalar),
        }
    }

    /// Builds an IO programming-voltage data item.
    pub fn from_prog_voltage(data: crate::IoProgVoltageData) -> Self {
        let mut payload = Box::new(PDU_IO_PROG_VOLTAGE_DATA {
            ProgVoltage_mv: data.prog_voltage_mv,
            PinOnDLC: data.pin_on_dlc,
        });
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE,
            pData: (&mut *payload as *mut PDU_IO_PROG_VOLTAGE_DATA).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::ProgVoltage(payload),
        }
    }

    /// Builds an IO byte-array data item.
    pub fn from_byte_array(mut bytes: Vec<u8>) -> Self {
        let mut meta = Box::new(PDU_IO_BYTEARRAY_DATA {
            DataSize: bytes.len() as UNUM32,
            pData: vec_ptr_or_null(&mut bytes).cast(),
        });
        meta.pData = vec_ptr_or_null(&mut bytes).cast();
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_BYTEARRAY,
            pData: (&mut *meta as *mut PDU_IO_BYTEARRAY_DATA).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::ByteArray { meta, bytes },
        }
    }

    /// Builds an IO filter-list data item.
    pub fn from_filter_list(filters: Vec<crate::IoFilterData>) -> Self {
        let mut raw_filters: Vec<PDU_IO_FILTER_DATA> = filters
            .into_iter()
            .map(|filter| {
                let mut mask = [0_u8; 12];
                for (idx, byte) in filter.filter_mask_message.into_iter().take(12).enumerate() {
                    mask[idx] = byte;
                }
                let mut pattern = [0_u8; 12];
                for (idx, byte) in filter
                    .filter_pattern_message
                    .into_iter()
                    .take(12)
                    .enumerate()
                {
                    pattern[idx] = byte;
                }
                PDU_IO_FILTER_DATA {
                    FilterType: filter.filter_type,
                    FilterNumber: filter.filter_number,
                    FilterCompareSize: filter.filter_compare_size,
                    FilterMaskMessage: mask,
                    FilterPatternMessage: pattern,
                }
            })
            .collect();

        let mut meta = Box::new(PDU_IO_FILTER_LIST {
            NumFilterEntries: raw_filters.len() as UNUM32,
            pFilterData: vec_ptr_or_null(&mut raw_filters),
        });
        meta.pFilterData = vec_ptr_or_null(&mut raw_filters);

        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_FILTER,
            pData: (&mut *meta as *mut PDU_IO_FILTER_LIST).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::FilterList {
                meta,
                filters: raw_filters,
            },
        }
    }

    /// Builds an IO event-queue-property data item.
    pub fn from_event_queue_property(data: crate::IoEventQueuePropertyData) -> Self {
        let mut payload = Box::new(PDU_IO_EVENT_QUEUE_PROPERTY_DATA {
            QueueSize: data.queue_size,
            QueueMode: data.queue_mode,
        });
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY,
            pData: (&mut *payload as *mut PDU_IO_EVENT_QUEUE_PROPERTY_DATA).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::EventQueueProperty(payload),
        }
    }

    /// Builds an IO vehicle-id-request data item.
    ///
    /// Returns an error when preselection text contains interior NUL bytes.
    pub fn from_vehicle_id_request(
        data: crate::IoVehicleIdRequestData,
    ) -> Result<Self, DPduApiError> {
        let preselection = match data.preselection_value {
            Some(value) => Some(CString::new(value).map_err(|_| DPduApiError::InvalidShortName)?),
            None => None,
        };

        let mut destination_bytes: Vec<Vec<u8>> = data
            .destination_addresses
            .iter()
            .map(|addr| addr.address.clone())
            .collect();
        let mut destinations: Vec<PDU_IP_ADDR_INFO> = data
            .destination_addresses
            .into_iter()
            .zip(destination_bytes.iter_mut())
            .map(|(addr, bytes)| PDU_IP_ADDR_INFO {
                IpVersion: addr.ip_version,
                pAddress: vec_ptr_or_null(bytes).cast(),
            })
            .collect();

        let mut meta = Box::new(PDU_IO_VEHICLE_ID_REQUEST {
            PreselectionMode: data.preselection_mode,
            PreselectionValue: preselection
                .as_ref()
                .map_or(std::ptr::null_mut(), |s| s.as_ptr().cast_mut()),
            CombinationMode: data.combination_mode,
            VehicleDiscoveryTime: data.vehicle_discovery_time,
            NumDestinationAddresses: destinations.len() as UNUM32,
            pDestinationAddresses: vec_ptr_or_null(&mut destinations),
        });
        meta.pDestinationAddresses = vec_ptr_or_null(&mut destinations);

        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST,
            pData: (&mut *meta as *mut PDU_IO_VEHICLE_ID_REQUEST).cast(),
        });
        Ok(Self {
            borrowed,
            storage: DataItemStorage::VehicleIdRequest {
                meta,
                preselection,
                destination_bytes,
                destinations,
            },
        })
    }

    /// Builds an IO Ethernet-switch-state data item.
    pub fn from_eth_switch_state(data: crate::IoEthSwitchStateData) -> Self {
        let mut payload = Box::new(PDU_IO_ETH_SWITCH_STATE {
            EthernetSenseState: data.ethernet_sense_state,
            EthernetActPinNumber: data.ethernet_act_pin_number,
        });
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_ETH_SWITCH_STATE,
            pData: (&mut *payload as *mut PDU_IO_ETH_SWITCH_STATE).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::EthSwitchState(payload),
        }
    }

    /// Builds an IO entity-address data item.
    pub fn from_entity_address(data: crate::IoEntityAddressData) -> Self {
        let mut payload = Box::new(PDU_IO_ENTITY_ADDRESS_DATA {
            LogicalAddress: data.logical_address,
            DoIPCtrlTimeout: data.doip_ctrl_timeout,
        });
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_ENTITY_ADDRESS,
            pData: (&mut *payload as *mut PDU_IO_ENTITY_ADDRESS_DATA).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::EntityAddress(payload),
        }
    }

    /// Builds an IO entity-status data item.
    pub fn from_entity_status(data: crate::IoEntityStatusData) -> Self {
        let mut payload = Box::new(PDU_IO_ENTITY_STATUS_DATA {
            EntityType: data.entity_type,
            TcpClientsMax: data.tcp_clients_max,
            TcpClients: data.tcp_clients,
            MaxDataSize: data.max_data_size,
        });
        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_ENTITY_STATUS,
            pData: (&mut *payload as *mut PDU_IO_ENTITY_STATUS_DATA).cast(),
        });
        Self {
            borrowed,
            storage: DataItemStorage::EntityStatus(payload),
        }
    }

    /// Builds an IO TLS-certificate data item.
    pub fn from_tls_certificate(data: crate::IoTlsCertificateData) -> Self {
        let mut cert_bytes: Vec<Vec<u8>> = data
            .cert_chains
            .into_iter()
            .map(|cert| cert.cert_buffer)
            .collect();
        let mut certs: Vec<PDU_IO_TLS_CERT_DATA> = cert_bytes
            .iter_mut()
            .map(|bytes| PDU_IO_TLS_CERT_DATA {
                CertLen: bytes.len() as UNUM32,
                CertBuffer: vec_ptr_or_null(bytes).cast(),
            })
            .collect();
        let mut chains = vec![PDU_IO_TLS_CERT_CHAIN_DATA {
            NumCertificates: certs.len() as UNUM32,
            pTlsCertData: vec_ptr_or_null(&mut certs),
        }];
        let mut meta = Box::new(PDU_IO_TLS_CERTIFICATE {
            NumCertChains: chains.len() as UNUM32,
            pTlsCertChainData: vec_ptr_or_null(&mut chains),
        });
        meta.pTlsCertChainData = vec_ptr_or_null(&mut chains);

        let borrowed = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_TLS_CERTIFICATE,
            pData: (&mut *meta as *mut PDU_IO_TLS_CERTIFICATE).cast(),
        });

        Self {
            borrowed,
            storage: DataItemStorage::TlsCertificate {
                meta,
                chains,
                certs,
                cert_bytes,
            },
        }
    }

    fn from_borrowed(item: &BorrowedDataItem) -> Result<Self, DPduApiError> {
        match item.item_type() {
            E_PDU_IT::PDU_IT_IO_UNUM32 => Ok(Self::from_unum32(*item.data::<UNUM32>()?)),
            E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE => {
                let data = item.data::<PDU_IO_PROG_VOLTAGE_DATA>()?;
                Ok(Self::from_prog_voltage(crate::IoProgVoltageData {
                    prog_voltage_mv: data.ProgVoltage_mv,
                    pin_on_dlc: data.PinOnDLC,
                }))
            }
            E_PDU_IT::PDU_IT_IO_BYTEARRAY => {
                let data = item.data::<PDU_IO_BYTEARRAY_DATA>()?;
                let bytes = unsafe {
                    ffi_slice(data.pData.cast::<u8>(), data.DataSize, "io bytearray data")
                }?
                .to_vec();
                Ok(Self::from_byte_array(bytes))
            }
            E_PDU_IT::PDU_IT_IO_FILTER => {
                let data = item.data::<PDU_IO_FILTER_LIST>()?;
                let entries = unsafe {
                    ffi_slice(data.pFilterData, data.NumFilterEntries, "io filter entries")
                }?;
                let filters = entries
                    .iter()
                    .map(|entry| crate::IoFilterData {
                        filter_type: entry.FilterType,
                        filter_number: entry.FilterNumber,
                        filter_compare_size: entry.FilterCompareSize,
                        filter_mask_message: entry.FilterMaskMessage.to_vec(),
                        filter_pattern_message: entry.FilterPatternMessage.to_vec(),
                    })
                    .collect();
                Ok(Self::from_filter_list(filters))
            }
            E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY => {
                let data = item.data::<PDU_IO_EVENT_QUEUE_PROPERTY_DATA>()?;
                Ok(Self::from_event_queue_property(
                    crate::IoEventQueuePropertyData {
                        queue_size: data.QueueSize,
                        queue_mode: data.QueueMode,
                    },
                ))
            }
            E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST => {
                let data = item.data::<PDU_IO_VEHICLE_ID_REQUEST>()?;
                let preselection_value = if data.PreselectionValue.is_null() {
                    None
                } else {
                    Some(
                        unsafe { CStr::from_ptr(data.PreselectionValue) }
                            .to_string_lossy()
                            .to_string(),
                    )
                };

                let destinations = unsafe {
                    ffi_slice(
                        data.pDestinationAddresses,
                        data.NumDestinationAddresses,
                        "vehicle id destination addresses",
                    )
                }?;

                let destination_addresses = destinations
                    .iter()
                    .map(|addr| {
                        let len = match addr.IpVersion {
                            4 => 4,
                            6 => 16,
                            _ => 0,
                        };
                        let bytes = if len == 0 {
                            Vec::new()
                        } else {
                            unsafe {
                                ffi_slice(addr.pAddress.cast::<u8>(), len, "ip address bytes")
                            }
                            .map(|s| s.to_vec())
                            .unwrap_or_default()
                        };
                        crate::IoIpAddrInfo {
                            ip_version: addr.IpVersion,
                            address: bytes,
                        }
                    })
                    .collect();

                Self::from_vehicle_id_request(crate::IoVehicleIdRequestData {
                    preselection_mode: data.PreselectionMode,
                    preselection_value,
                    combination_mode: data.CombinationMode,
                    vehicle_discovery_time: data.VehicleDiscoveryTime,
                    destination_addresses,
                })
            }
            E_PDU_IT::PDU_IT_IO_ETH_SWITCH_STATE => {
                let data = item.data::<PDU_IO_ETH_SWITCH_STATE>()?;
                Ok(Self::from_eth_switch_state(crate::IoEthSwitchStateData {
                    ethernet_sense_state: data.EthernetSenseState,
                    ethernet_act_pin_number: data.EthernetActPinNumber,
                }))
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_ADDRESS => {
                let data = item.data::<PDU_IO_ENTITY_ADDRESS_DATA>()?;
                Ok(Self::from_entity_address(crate::IoEntityAddressData {
                    logical_address: data.LogicalAddress,
                    doip_ctrl_timeout: data.DoIPCtrlTimeout,
                }))
            }
            E_PDU_IT::PDU_IT_IO_ENTITY_STATUS => {
                let data = item.data::<PDU_IO_ENTITY_STATUS_DATA>()?;
                Ok(Self::from_entity_status(crate::IoEntityStatusData {
                    entity_type: data.EntityType,
                    tcp_clients_max: data.TcpClientsMax,
                    tcp_clients: data.TcpClients,
                    max_data_size: data.MaxDataSize,
                }))
            }
            E_PDU_IT::PDU_IT_IO_TLS_CERTIFICATE => {
                let data = item.data::<PDU_IO_TLS_CERTIFICATE>()?;
                let chains = unsafe {
                    ffi_slice(
                        data.pTlsCertChainData,
                        data.NumCertChains,
                        "tls cert chain list",
                    )
                }?;
                let mut cert_chain_entries = Vec::new();
                for chain in chains {
                    let certs = unsafe {
                        ffi_slice(
                            chain.pTlsCertData,
                            chain.NumCertificates,
                            "tls cert chain data",
                        )
                    }?;
                    for cert in certs {
                        let bytes = unsafe {
                            ffi_slice(cert.CertBuffer.cast::<u8>(), cert.CertLen, "tls cert bytes")
                        }?
                        .to_vec();
                        cert_chain_entries.push(crate::IoTlsCertData { cert_buffer: bytes });
                    }
                }
                Ok(Self::from_tls_certificate(crate::IoTlsCertificateData {
                    cert_chains: cert_chain_entries,
                }))
            }
            _ => Ok(Self {
                borrowed: BorrowedDataItem(item.0),
                storage: DataItemStorage::Unsupported,
            }),
        }
    }
}

impl Borrow<BorrowedDataItem> for OwnedDataItem {
    fn borrow(&self) -> &BorrowedDataItem {
        match &self.storage {
            DataItemStorage::U32(value) => {
                std::hint::black_box(&**value as *const UNUM32);
            }
            DataItemStorage::ProgVoltage(value) => {
                std::hint::black_box(&**value as *const PDU_IO_PROG_VOLTAGE_DATA);
            }
            DataItemStorage::ByteArray { meta, bytes } => {
                std::hint::black_box(&**meta as *const PDU_IO_BYTEARRAY_DATA);
                std::hint::black_box(bytes.as_ptr());
            }
            DataItemStorage::FilterList { meta, filters } => {
                std::hint::black_box(&**meta as *const PDU_IO_FILTER_LIST);
                std::hint::black_box(filters.as_ptr());
            }
            DataItemStorage::EventQueueProperty(value) => {
                std::hint::black_box(&**value as *const PDU_IO_EVENT_QUEUE_PROPERTY_DATA);
            }
            DataItemStorage::VehicleIdRequest {
                meta,
                preselection,
                destination_bytes,
                destinations,
            } => {
                std::hint::black_box(&**meta as *const PDU_IO_VEHICLE_ID_REQUEST);
                std::hint::black_box(preselection.as_ref().map(|s| s.as_ptr()));
                std::hint::black_box(destination_bytes.as_ptr());
                std::hint::black_box(destinations.as_ptr());
            }
            DataItemStorage::EthSwitchState(value) => {
                std::hint::black_box(&**value as *const PDU_IO_ETH_SWITCH_STATE);
            }
            DataItemStorage::EntityAddress(value) => {
                std::hint::black_box(&**value as *const PDU_IO_ENTITY_ADDRESS_DATA);
            }
            DataItemStorage::EntityStatus(value) => {
                std::hint::black_box(&**value as *const PDU_IO_ENTITY_STATUS_DATA);
            }
            DataItemStorage::TlsCertificate {
                meta,
                chains,
                certs,
                cert_bytes,
            } => {
                std::hint::black_box(&**meta as *const PDU_IO_TLS_CERTIFICATE);
                std::hint::black_box(chains.as_ptr());
                std::hint::black_box(certs.as_ptr());
                std::hint::black_box(cert_bytes.as_ptr());
            }
            DataItemStorage::Unsupported => {}
        }
        &self.borrowed
    }
}

impl ToOwned for BorrowedDataItem {
    type Owned = OwnedDataItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedDataItem::from_borrowed(self).expect("data item is not cloneable")
    }
}

impl AsRef<BorrowedDataItem> for OwnedDataItem {
    fn as_ref(&self) -> &BorrowedDataItem {
        self.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_data_item_from_unum32_exposes_value() {
        let owned = OwnedDataItem::from_unum32(42);
        let borrowed = owned.as_ref();

        assert_eq!(borrowed.item_type(), E_PDU_IT::PDU_IT_IO_UNUM32);
        assert_eq!(*borrowed.data::<UNUM32>().expect("unum32 payload"), 42);
    }

    #[test]
    fn borrowed_bytearray_to_owned_is_deep_copy() {
        let mut source = vec![1_u8, 2, 3, 4];
        let raw = PDU_IO_BYTEARRAY_DATA {
            DataSize: source.len() as UNUM32,
            pData: source.as_mut_ptr().cast(),
        };
        let item = BorrowedDataItem(PDU_DATA_ITEM {
            ItemType: E_PDU_IT::PDU_IT_IO_BYTEARRAY,
            pData: (&raw as *const PDU_IO_BYTEARRAY_DATA).cast_mut().cast(),
        });

        let owned = item.to_owned();
        source[0] = 99;

        let owned_meta = owned
            .as_ref()
            .data::<PDU_IO_BYTEARRAY_DATA>()
            .expect("owned bytearray meta");
        let owned_bytes = unsafe {
            std::slice::from_raw_parts(owned_meta.pData.cast::<u8>(), owned_meta.DataSize as usize)
        };

        assert_eq!(owned_bytes, &[1, 2, 3, 4]);
    }

    #[test]
    fn vehicle_id_constructor_rejects_nul_preselection() {
        let result = OwnedDataItem::from_vehicle_id_request(crate::IoVehicleIdRequestData {
            preselection_mode: 1,
            preselection_value: Some("bad\0input".to_string()),
            combination_mode: 2,
            vehicle_discovery_time: 3,
            destination_addresses: vec![],
        });

        assert!(matches!(result, Err(DPduApiError::InvalidShortName)));
    }

    #[test]
    fn tls_certificate_constructor_sets_chain_and_cert_counts() {
        let owned = OwnedDataItem::from_tls_certificate(crate::IoTlsCertificateData {
            cert_chains: vec![
                crate::IoTlsCertData {
                    cert_buffer: vec![0xAA, 0xBB],
                },
                crate::IoTlsCertData {
                    cert_buffer: vec![0xCC],
                },
            ],
        });

        let tls = owned
            .as_ref()
            .data::<PDU_IO_TLS_CERTIFICATE>()
            .expect("tls payload");
        assert_eq!(tls.NumCertChains, 1);

        let chains = unsafe {
            std::slice::from_raw_parts(tls.pTlsCertChainData, tls.NumCertChains as usize)
        };
        assert_eq!(chains[0].NumCertificates, 2);
    }
}
