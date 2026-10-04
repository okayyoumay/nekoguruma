use tonic::Status;

use iso22900::{
    BorrowedDataItem, BorrowedIoByteArrayData, BorrowedIoEntityAddressData,
    BorrowedIoEntityStatusData, BorrowedIoEthSwitchStateData, BorrowedIoEventQueuePropertyData,
    BorrowedIoFilterListData, BorrowedIoProgVoltageData, BorrowedIoTlsCertificateData,
    BorrowedIoUnum32Data, BorrowedIoVehicleIdRequestData, DPduApiError, E_PDU_FILTER,
    E_PDU_QUEUE_MODE, OwnedDataItem,
};
use vci_service_interface::{
    DataItem, IoBytearray, IoEntityAddress, IoEntityStatus, IoEthSwitchState, IoEventQueueProperty,
    IoFilter, IoFilterList, IoProgVoltage, IoTlsCertChainData, IoTlsCertData, IoTlsCertificate,
    IoVehicleIdRequest, IpAddrInfo, data_item::Data as DataItemData,
};

fn map_runtime_error(error: DPduApiError) -> Status {
    Status::internal(format!("ISO22900 call failed: {error}"))
}

pub(super) fn ioctl_input_to_owned(input_data: DataItem) -> Result<OwnedDataItem, Status> {
    match input_data.data {
        Some(DataItemData::Unum32Value(value)) => Ok(OwnedDataItem::from_unum32(value)),
        Some(DataItemData::ProgVoltage(value)) => Ok(OwnedDataItem::from_prog_voltage(
            iso22900::IoProgVoltageData {
                prog_voltage_mv: value.prog_voltage_mv,
                pin_on_dlc: value.pin_on_dlc,
            },
        )),
        Some(DataItemData::BytearrayData(value)) => Ok(OwnedDataItem::from_byte_array(value.data)),
        Some(DataItemData::FilterData(value)) => Ok(OwnedDataItem::from_filter_list(
            value
                .filters
                .into_iter()
                .map(|f| {
                    let filter_type = match vci_service_interface::PduFilter::try_from(
                        f.filter_type,
                    )
                    .map_err(|_| Status::invalid_argument("filter.filter_type is invalid"))?
                    {
                        vci_service_interface::PduFilter::PduFltPass => E_PDU_FILTER::PDU_FLT_PASS,
                        vci_service_interface::PduFilter::PduFltBlock => {
                            E_PDU_FILTER::PDU_FLT_BLOCK
                        }
                        vci_service_interface::PduFilter::PduFltPassUudt => {
                            E_PDU_FILTER::PDU_FLT_PASS_UUDT
                        }
                        vci_service_interface::PduFilter::PduFltBlockUudt => {
                            E_PDU_FILTER::PDU_FLT_BLOCK_UUDT
                        }
                        vci_service_interface::PduFilter::PduFltUnspecified => {
                            return Err(Status::invalid_argument(
                                "filter.filter_type must be a concrete filter type",
                            ));
                        }
                    };

                    Ok(iso22900::IoFilterData {
                        filter_type,
                        filter_number: f.filter_number,
                        filter_compare_size: 0,
                        filter_mask_message: f.filter_mask_message,
                        filter_pattern_message: f.filter_pattern_message,
                    })
                })
                .collect::<Result<Vec<_>, Status>>()?,
        )),
        Some(DataItemData::EventQueueProperty(value)) => {
            let queue_mode = match vci_service_interface::PduQueueMode::try_from(value.queue_mode)
                .map_err(|_| {
                Status::invalid_argument("event_queue_property.queue_mode is invalid")
            })? {
                vci_service_interface::PduQueueMode::PduQueUnlimited => {
                    E_PDU_QUEUE_MODE::PDU_QUE_UNLIMITED
                }
                vci_service_interface::PduQueueMode::PduQueLimited => {
                    E_PDU_QUEUE_MODE::PDU_QUE_LIMITED
                }
                vci_service_interface::PduQueueMode::PduQueCircular => {
                    E_PDU_QUEUE_MODE::PDU_QUE_CIRCULAR
                }
            };

            Ok(OwnedDataItem::from_event_queue_property(
                iso22900::IoEventQueuePropertyData {
                    queue_size: value.queue_size,
                    queue_mode,
                },
            ))
        }
        Some(DataItemData::VehicleIdRequest(value)) => {
            OwnedDataItem::from_vehicle_id_request(iso22900::IoVehicleIdRequestData {
                preselection_mode: value.preselection_mode,
                preselection_value: Some(value.preselection_value),
                combination_mode: value.combination_mode,
                vehicle_discovery_time: value.vehicle_discovery_time,
                destination_addresses: value
                    .destination_addresses
                    .into_iter()
                    .map(|addr| iso22900::IoIpAddrInfo {
                        ip_version: addr.ip_version,
                        address: addr.address,
                    })
                    .collect(),
            })
            .map_err(map_runtime_error)
        }
        Some(DataItemData::EthSwitchState(value)) => Ok(OwnedDataItem::from_eth_switch_state(
            iso22900::IoEthSwitchStateData {
                ethernet_sense_state: value.ethernet_sense_state,
                ethernet_act_pin_number: value.ethernet_act_pin_number,
            },
        )),
        Some(DataItemData::EntityAddress(value)) => Ok(OwnedDataItem::from_entity_address(
            iso22900::IoEntityAddressData {
                logical_address: value.logical_address,
                doip_ctrl_timeout: value.doip_ctrl_timeout,
            },
        )),
        Some(DataItemData::EntityStatus(value)) => Ok(OwnedDataItem::from_entity_status(
            iso22900::IoEntityStatusData {
                entity_type: value.entity_type,
                tcp_clients_max: value.tcp_clients_max,
                tcp_clients: value.tcp_clients,
                max_data_size: value.max_data_size,
            },
        )),
        Some(DataItemData::TlsCertificate(value)) => Ok(OwnedDataItem::from_tls_certificate(
            iso22900::IoTlsCertificateData {
                cert_chains: value
                    .cert_chains
                    .into_iter()
                    .flat_map(|chain| {
                        chain.certs.into_iter().map(|cert| iso22900::IoTlsCertData {
                            cert_buffer: cert.cert_buffer,
                        })
                    })
                    .collect(),
            },
        )),
        None => Err(Status::invalid_argument(
            "input_data.data is required for IOCTL",
        )),
    }
}

pub(super) fn ioctl_output_to_data_item(output: &BorrowedDataItem) -> Result<DataItem, Status> {
    struct DataItemVisitorImpl;

    impl iso22900::DataItemVisitor for DataItemVisitorImpl {
        type Output = Result<DataItem, Status>;

        fn visit_io_unum32(
            &mut self,
            data: &BorrowedIoUnum32Data,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::Unum32Value(data.value())),
            }))
        }

        fn visit_io_prog_voltage(
            &mut self,
            data: &BorrowedIoProgVoltageData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::ProgVoltage(IoProgVoltage {
                    prog_voltage_mv: data.prog_voltage_mv(),
                    pin_on_dlc: data.pin_on_dlc(),
                })),
            }))
        }

        fn visit_io_bytearray(
            &mut self,
            data: &BorrowedIoByteArrayData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::BytearrayData(IoBytearray {
                    data: data.data_bytes()?.to_vec(),
                })),
            }))
        }

        fn visit_io_filter(
            &mut self,
            data: &BorrowedIoFilterListData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::FilterData(IoFilterList {
                    filters: data
                        .entries()?
                        .iter()
                        .map(|filter| IoFilter {
                            filter_type: filter.filter_type() as i32,
                            filter_number: filter.filter_number(),
                            filter_mask_message: filter.filter_mask_message().to_vec(),
                            filter_pattern_message: filter.filter_pattern_message().to_vec(),
                        })
                        .collect::<Vec<IoFilter>>(),
                })),
            }))
        }

        fn visit_io_event_queue_property(
            &mut self,
            data: &BorrowedIoEventQueuePropertyData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::EventQueueProperty(IoEventQueueProperty {
                    queue_size: data.queue_size(),
                    queue_mode: data.queue_mode().0 as i32,
                })),
            }))
        }

        fn visit_io_vehicle_id_request(
            &mut self,
            data: &BorrowedIoVehicleIdRequestData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::VehicleIdRequest(IoVehicleIdRequest {
                    preselection_mode: data.preselection_mode(),
                    preselection_value: data.preselection_value().unwrap_or("").to_owned(),
                    combination_mode: data.combination_mode(),
                    vehicle_discovery_time: data.vehicle_discovery_time(),
                    destination_addresses: data
                        .destination_addresses()?
                        .iter()
                        .map(|addr| {
                            Ok(IpAddrInfo {
                                ip_version: addr.ip_version(),
                                address: addr.address_bytes()?.to_vec(),
                            })
                        })
                        .collect::<Result<Vec<IpAddrInfo>, DPduApiError>>()?,
                })),
            }))
        }

        fn visit_io_eth_switch_state(
            &mut self,
            data: &BorrowedIoEthSwitchStateData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::EthSwitchState(IoEthSwitchState {
                    ethernet_sense_state: data.ethernet_sense_state(),
                    ethernet_act_pin_number: data.ethernet_act_pin_number(),
                })),
            }))
        }

        fn visit_io_entity_address(
            &mut self,
            data: &BorrowedIoEntityAddressData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::EntityAddress(IoEntityAddress {
                    logical_address: data.logical_address(),
                    doip_ctrl_timeout: data.doip_ctrl_timeout(),
                })),
            }))
        }

        fn visit_io_entity_status(
            &mut self,
            data: &BorrowedIoEntityStatusData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::EntityStatus(IoEntityStatus {
                    entity_type: data.entity_type(),
                    tcp_clients_max: data.tcp_clients_max(),
                    tcp_clients: data.tcp_clients(),
                    max_data_size: data.max_data_size(),
                })),
            }))
        }

        fn visit_io_tls_certificate(
            &mut self,
            data: &BorrowedIoTlsCertificateData,
        ) -> Result<Self::Output, DPduApiError> {
            Ok(Ok(DataItem {
                data: Some(DataItemData::TlsCertificate(IoTlsCertificate {
                    cert_chains: data
                        .cert_chains()?
                        .iter()
                        .map(|cert| {
                            Ok(IoTlsCertChainData {
                                certs: cert
                                    .certificates()?
                                    .iter()
                                    .map(|cert_buffer| {
                                        Ok(IoTlsCertData {
                                            cert_buffer: cert_buffer.cert_bytes()?.to_vec(),
                                        })
                                    })
                                    .collect::<Result<Vec<IoTlsCertData>, DPduApiError>>()?,
                            })
                        })
                        .collect::<Result<Vec<IoTlsCertChainData>, DPduApiError>>()?,
                })),
            }))
        }
    }

    let mut visitor = DataItemVisitorImpl;
    output.visit(&mut visitor).map_err(map_runtime_error)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;
    use vci_service_interface::{IoVehicleIdRequest, IpAddrInfo};

    #[test]
    fn ioctl_input_requires_data_variant() {
        let result = ioctl_input_to_owned(DataItem { data: None });
        assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
    }

    #[test]
    fn ioctl_input_vehicle_id_rejects_nul_preselection() {
        let result = ioctl_input_to_owned(DataItem {
            data: Some(DataItemData::VehicleIdRequest(IoVehicleIdRequest {
                preselection_mode: 1,
                preselection_value: "BAD\0VALUE".to_owned(),
                combination_mode: 0,
                vehicle_discovery_time: 50,
                destination_addresses: vec![IpAddrInfo {
                    ip_version: 4,
                    address: vec![192, 168, 0, 10],
                }],
            })),
        });
        assert_eq!(result.unwrap_err().code(), Code::Internal);
    }

    #[test]
    fn ioctl_input_accepts_unum32_variant() {
        let result = ioctl_input_to_owned(DataItem {
            data: Some(DataItemData::Unum32Value(42)),
        });
        assert!(result.is_ok());
    }

    #[test]
    fn ioctl_input_accepts_bytearray_variant() {
        let result = ioctl_input_to_owned(DataItem {
            data: Some(DataItemData::BytearrayData(
                vci_service_interface::IoBytearray {
                    data: vec![9, 8, 7],
                },
            )),
        });
        assert!(result.is_ok());
    }
}
