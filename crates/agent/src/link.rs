//! The communication link of a job (ADR-235 item 4).

use std::time::Duration;

use tonic::Streaming;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EcuUniqueRespData,
    EventNotification, GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ObjectType, ParamItem,
    PduParamClass, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    SubscribeEventRequest, UniqueRespIdTableItem, create_com_logical_link_request, param_item,
    resource_data, subscribe_event_request,
};
use worker_host::client::WorkerClient;

use crate::host::{HostError, unary};

/// What a job's link needs. Until procedures declare their timings, the caller fills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    /// D-PDU protocol name, e.g. `ISO15765`.
    pub protocol_short_name: String,
    pub baud_rate: u32,
    /// Physical request CAN ID (tester to ECU).
    pub tx_id: u32,
    /// Response CAN ID (ECU to tester).
    pub rx_id: u32,
    /// P2: time to the first response, in milliseconds.
    pub p2_max_ms: u32,
    /// P2*: time to the next response after a 0x78 (response pending), in milliseconds.
    pub p2_star_ms: u32,
    /// Upper bound of a whole 0x78 chain, in milliseconds.
    pub rc78_completion_ms: u32,
}

impl LinkConfig {
    /// UDS on CAN with the legacy ISO 15765-4 IDs of the first ECU (7E0 / 7E8) at 500 kbit/s.
    pub fn iso15765(tx_id: u32, rx_id: u32) -> Self {
        Self {
            protocol_short_name: "ISO15765".to_owned(),
            baud_rate: 500_000,
            tx_id,
            rx_id,
            p2_max_ms: 1_000,
            p2_star_ms: 5_000,
            rc78_completion_ms: 25_000,
        }
    }

    /// Refuses values the worker would read differently from the host: a zero timing turns a
    /// limit off or makes every response pending expire at once, and a timing above
    /// [`MAX_TIMING_MS`] does not fit the worker's microsecond ComParams.
    pub fn validate(&self) -> Result<(), HostError> {
        for timing in [self.p2_max_ms, self.p2_star_ms, self.rc78_completion_ms] {
            if timing == 0 || timing > MAX_TIMING_MS {
                return Err(HostError::Setup(
                    "P2, P2* and the 0x78 completion timeout must be 1 ms to 4294967 ms",
                ));
            }
        }
        if self.tx_id > MAX_CAN_ID || self.rx_id > MAX_CAN_ID {
            return Err(HostError::Setup("a CAN ID does not fit 29 bits"));
        }
        Ok(())
    }

    /// How long a send-receive may take before the host gives up: the first response window,
    /// the whole 0x78 chain, one more P2*, and a margin for the worker and the transmission of
    /// the request (ADR-235 item 6).
    pub fn send_recv_ceiling(&self) -> Duration {
        Duration::from_millis(
            u64::from(self.p2_max_ms)
                + u64::from(self.rc78_completion_ms)
                + u64::from(self.p2_star_ms),
        ) + Duration::from_secs(2)
    }
}

/// Longest timing the worker's microsecond ComParams can hold.
pub const MAX_TIMING_MS: u32 = u32::MAX / 1_000;
const MAX_CAN_ID: u32 = 0x1FFF_FFFF;

/// Where the response code sits in a negative response: `7F`, SID, code.
const RC_BYTE_OFFSET: u32 = 2;

/// An open link: the logical link on the worker and the event stream its primitives report on.
pub struct Link {
    pub module_handle: ModuleHandle,
    pub cll_handle: ComLogicalLinkHandle,
    pub events: Streaming<EventNotification>,
}

fn param(id: u32, class: PduParamClass, value: u32) -> ParamItem {
    ParamItem {
        id: Some(param_item::Id::ParamId(id)),
        com_param_class: class as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    }
}

/// `ms` is at most [`MAX_TIMING_MS`] after [`LinkConfig::validate`].
fn micros(ms: u32) -> u32 {
    ms.saturating_mul(1_000)
}

async fn comparam_id(
    client: &mut WorkerClient,
    deadline: Duration,
    shortname: &str,
) -> Result<u32, HostError> {
    let request = GetObjectIdRequest {
        object_type: ObjectType::ObjtComparam as i32,
        shortname: shortname.to_owned(),
    };
    Ok(
        unary(deadline, "GetObjectId", client.get_object_id(request))
            .await?
            .pdu_object_id,
    )
}

/// Opens the first module, creates a logical link for `config`, sets its ComParams and the
/// response ID table, connects it and subscribes to its events (ADR-235 item 4). The worker
/// answers 0x78 itself (`CP_RC78Handling`), so the procedure only sees final responses. If a
/// step fails, what was already opened is closed again.
pub async fn open(
    client: &mut WorkerClient,
    config: &LinkConfig,
    deadline: Duration,
) -> Result<Link, HostError> {
    config.validate()?;
    let module_handle = unary(
        deadline,
        "GetModuleIds",
        client.get_module_ids(GetModuleIdsRequest {}),
    )
    .await?
    .module_id_list
    .and_then(|list| list.module_data.into_iter().next())
    .and_then(|data| data.module_handle)
    .ok_or(HostError::Setup("the worker reports no module"))?;
    unary(
        deadline,
        "ModuleConnect",
        client.module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await?;

    let cll_handle = match create_link(client, config, deadline, module_handle).await {
        Ok(cll_handle) => cll_handle,
        Err(error) => {
            disconnect_module(client, module_handle, deadline).await;
            return Err(error);
        }
    };
    match set_up_link(client, config, deadline, cll_handle).await {
        Ok(events) => Ok(Link {
            module_handle,
            cll_handle,
            events,
        }),
        Err(error) => {
            let _ = teardown(client, module_handle, cll_handle, deadline).await;
            Err(error)
        }
    }
}

async fn create_link(
    client: &mut WorkerClient,
    config: &LinkConfig,
    deadline: Duration,
    module_handle: ModuleHandle,
) -> Result<ComLogicalLinkHandle, HostError> {
    let resource_id = unary(
        deadline,
        "GetResourceIds",
        client.get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolName(
                    config.protocol_short_name.clone(),
                )),
            }),
        }),
    )
    .await?
    .resource_id_list
    .and_then(|list| list.resource_id_data_array.into_iter().next())
    .and_then(|data| data.resource_id_array.into_iter().next())
    .ok_or(HostError::Setup("no resource for the protocol"))?;

    unary(
        deadline,
        "CreateComLogicalLink",
        client.create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        }),
    )
    .await?
    .cll_handle
    .ok_or(HostError::Setup("no logical link handle"))
}

async fn set_up_link(
    client: &mut WorkerClient,
    config: &LinkConfig,
    deadline: Duration,
    cll_handle: ComLogicalLinkHandle,
) -> Result<Streaming<EventNotification>, HostError> {
    for (name, value) in [
        ("CP_Baudrate", config.baud_rate),
        ("CP_P2Max", micros(config.p2_max_ms)),
        ("CP_P2Star", micros(config.p2_star_ms)),
        ("CP_RC78Handling", 1),
        // The worker finds a response pending only at this offset.
        ("CP_RCByteOffset", RC_BYTE_OFFSET),
        (
            "CP_RC78CompletionTimeout",
            micros(config.rc78_completion_ms),
        ),
    ] {
        let id = comparam_id(client, deadline, name).await?;
        unary(
            deadline,
            "SetComParam",
            client.set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(param(id, PduParamClass::PduPcCom, value)),
            }),
        )
        .await?;
    }

    // The table makes the worker install the flow-control filter for the ECU's IDs.
    let phys_req = comparam_id(client, deadline, "CP_CanPhysReqId").await?;
    let resp = comparam_id(client, deadline, "CP_CanRespUSDTId").await?;
    unary(
        deadline,
        "SetUniqueRespIdTable",
        client.set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![
                        param(phys_req, PduParamClass::PduPcUniqueId, config.tx_id),
                        param(resp, PduParamClass::PduPcUniqueId, config.rx_id),
                    ],
                }],
            }),
        }),
    )
    .await?;
    unary(
        deadline,
        "ConnectComLogicalLink",
        client.connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        }),
    )
    .await?;

    unary(
        deadline,
        "SubscribeEvent",
        client.subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        }),
    )
    .await
}

/// Disconnects and destroys the logical link and disconnects the module.
pub async fn close(
    client: &mut WorkerClient,
    link: Link,
    deadline: Duration,
) -> Result<(), HostError> {
    let Link {
        module_handle,
        cll_handle,
        events,
    } = link;
    drop(events);
    teardown(client, module_handle, cll_handle, deadline).await
}

/// Runs every step even after one fails, and returns the first failure.
async fn teardown(
    client: &mut WorkerClient,
    module_handle: ModuleHandle,
    cll_handle: ComLogicalLinkHandle,
    deadline: Duration,
) -> Result<(), HostError> {
    // A link that was never connected refuses the disconnect; destroying it still works.
    let disconnected = unary(
        deadline,
        "DisconnectComLogicalLink",
        client.disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        }),
    )
    .await
    .map(drop);
    let destroyed = unary(
        deadline,
        "DestroyComLogicalLink",
        client.destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        }),
    )
    .await
    .map(drop);
    disconnect_module(client, module_handle, deadline).await;
    disconnected.or(destroyed)
}

async fn disconnect_module(
    client: &mut WorkerClient,
    module_handle: ModuleHandle,
    deadline: Duration,
) {
    let result = unary(
        deadline,
        "ModuleDisconnect",
        client.module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not disconnect the module");
    }
}
