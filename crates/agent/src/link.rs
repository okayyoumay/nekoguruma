//! The communication link of a job (ADR-235 item 4).

use std::time::Duration;

use diag_ir::declaration::{EcuVariant, Protocol};
use tonic::Streaming;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EcuUniqueRespData,
    EventNotification, GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ObjectType, ParamItem,
    PduParamClass, PinData, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    SubscribeEventRequest, UniqueRespIdTableItem, create_com_logical_link_request, param_item,
    pin_data, resource_data, subscribe_event_request,
};
use worker_host::client::WorkerClient;

use crate::host::{HostError, unary};
use crate::policy::Permission;

/// What a job's link needs: from the protocol a job's document declares
/// ([`LinkConfig::declared`], ADR-324), or built by the caller for a program without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    /// ISO 22900-2 (2022) protocol short name (Annex B.1), e.g. [`PROTOCOL_UDS_ON_CAN`].
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
    /// Further ComParams by short name, set on the link as the document declares them, after
    /// the link's own (ADR-324 item 3).
    pub params: Vec<(String, u32)>,
}

impl LinkConfig {
    /// UDS on CAN at 500 kbit/s with the given request and response IDs (for example the
    /// common 7E0 / 7E8 pair).
    pub fn iso15765(tx_id: u32, rx_id: u32) -> Self {
        Self {
            protocol_short_name: PROTOCOL_UDS_ON_CAN.to_owned(),
            baud_rate: 500_000,
            tx_id,
            rx_id,
            p2_max_ms: 1_000,
            p2_star_ms: 5_000,
            rc78_completion_ms: 25_000,
            params: Vec::new(),
        }
    }

    /// The link `variant` declares (ADR-324): its one protocol, which must be UDS on CAN, with
    /// its identifiers and baud rate. P2, P2* and the 0x78 completion timeout come from its
    /// ComParams `CP_P2Max`, `CP_P2Star` and `CP_RC78CompletionTimeout` in microseconds, rounded
    /// up to whole milliseconds, or from [`LinkConfig::iso15765`] when it declares none; its
    /// other ComParams are set on the link as they are. The result is validated.
    pub fn declared(variant: &EcuVariant<'_>) -> Result<Self, HostError> {
        let refuse = |why: String| HostError::DeclaredLink(why);
        let protocols: Vec<Protocol<'_>> = variant.protocols().into_iter().flatten().collect();
        let protocol = match protocols.as_slice() {
            [only] => *only,
            [] => return Err(refuse("the variant declares no protocol".to_owned())),
            _ => {
                return Err(refuse(
                    "the variant declares several protocols; choosing one is not supported yet"
                        .to_owned(),
                ));
            }
        };
        let name = protocol.short_name().unwrap_or_default();
        if !UDS_ON_CAN_NAMES.contains(&name) || protocol.application_layer() != Some("UDS") {
            return Err(refuse(format!(
                "protocol {name:?} carrying {:?} is not UDS on CAN",
                protocol.application_layer().unwrap_or_default()
            )));
        }
        if protocol.extended_addressing() {
            return Err(refuse(
                "extended addressing is not supported: the link does not set it".to_owned(),
            ));
        }
        let mut config = Self {
            baud_rate: protocol.baud_rate(),
            params: Vec::new(),
            ..Self::iso15765(protocol.tx_id(), protocol.rx_id())
        };
        let mut seen: Vec<&str> = Vec::new();
        for param in protocol.params().into_iter().flatten() {
            let name = param.short_name().unwrap_or_default();
            if seen.contains(&name) {
                return Err(refuse(format!("ComParam {name} is declared twice")));
            }
            seen.push(name);
            let value = u32::try_from(param.value()).map_err(|_| {
                refuse(format!(
                    "ComParam {name} is {}, not a 32-bit unsigned value",
                    param.value()
                ))
            })?;
            let timing = match name {
                "CP_P2Max" => Some(&mut config.p2_max_ms),
                "CP_P2Star" => Some(&mut config.p2_star_ms),
                "CP_RC78CompletionTimeout" => Some(&mut config.rc78_completion_ms),
                _ => None,
            };
            match timing {
                Some(ms) => *ms = value.div_ceil(1_000),
                None => config.params.push((name.to_owned(), value)),
            }
        }
        config
            .validate()
            .map_err(|error| refuse(error.to_string()))?;
        Ok(config)
    }

    /// Refuses values the worker would read differently from the host: the worker replaces a
    /// zero P2 or P2* with its own default and reads a zero 0x78 completion timeout as no
    /// limit, while the host would compute its deadline from zero; and a timing above
    /// [`MAX_TIMING_MS`] does not fit the worker's microsecond ComParams.
    pub fn validate(&self) -> Result<(), HostError> {
        // The read-only policy judges UDS service IDs, and the link set-up (flow-control table,
        // response pending handling) is for UDS on CAN. On any other protocol, raw CAN for
        // example, the same bytes would not be UDS requests.
        if self.protocol_short_name != PROTOCOL_UDS_ON_CAN {
            return Err(HostError::Setup(
                "only the ISO_14229_3_on_ISO_15765_2 protocol (UDS on CAN) is supported",
            ));
        }
        for timing in [self.p2_max_ms, self.p2_star_ms, self.rc78_completion_ms] {
            if timing == 0 || timing > MAX_TIMING_MS {
                return Err(HostError::Setup(
                    "P2, P2* and the 0x78 completion timeout must be 1 ms to 4294967 ms",
                ));
            }
        }
        if self.tx_id > MAX_CAN_ID || self.rx_id > MAX_CAN_ID {
            return Err(HostError::Setup(
                "only 11-bit CAN IDs are supported: the link does not set the ID format yet",
            ));
        }
        if self.baud_rate == 0 {
            return Err(HostError::Setup("the baud rate must not be zero"));
        }
        // The link sets these itself, from the fields above or for its response pending
        // handling, and a further one would override it.
        if self
            .params
            .iter()
            .any(|(name, _)| SET_BY_LINK.contains(&name.as_str()))
        {
            return Err(HostError::Setup(
                "a further ComParam names one the link sets itself (ADR-324 item 3)",
            ));
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

/// The one protocol the runner supports: UDS on CAN, by its ISO 22900-2 (2022) short name
/// (Annex B.1). Both worker kinds resolve the link's resource from the standard names (ADR-319).
pub const PROTOCOL_UDS_ON_CAN: &str = "ISO_14229_3_on_ISO_15765_2";
/// The protocol names a document may declare UDS on CAN by (ADR-324 item 2): the transport's
/// name the CSV tables use, or either ISO 22900-2 (2022) short name of the stack (Annex B.1).
/// The link names its resource [`PROTOCOL_UDS_ON_CAN`] whichever it is.
const UDS_ON_CAN_NAMES: [&str; 3] = [
    "ISO15765",
    PROTOCOL_UDS_ON_CAN,
    "ISO_15765_3_on_ISO_15765_2",
];
/// The ComParams the link sets itself ([`set_up_link`]).
const SET_BY_LINK: [&str; 8] = [
    "CP_Baudrate",
    "CP_P2Max",
    "CP_P2Star",
    "CP_RC78Handling",
    "CP_RCByteOffset",
    "CP_RC78CompletionTimeout",
    "CP_CanPhysReqId",
    "CP_CanRespUSDTId",
];
/// The bus type of UDS on CAN: dual-wire high-speed CAN (ISO 22900-2 (2022) Annex B.1).
pub const BUS_TYPE_DWCAN: &str = "ISO_11898_2_DWCAN";
/// The DLC pins of dual-wire CAN and their pin types (ISO 22900-2 (2022) Annex B.2): CAN high
/// on pin 6, CAN low on pin 14.
pub const DWCAN_PINS: [(u32, &str); 2] = [(6, "HI"), (14, "LOW")];

/// Longest timing the worker's microsecond ComParams can hold.
pub const MAX_TIMING_MS: u32 = u32::MAX / 1_000;
/// 11-bit IDs only: a 29-bit ID needs `CP_CanPhysReqFormat` / `CP_CanRespUSDTFormat`, which
/// `open` does not set yet.
const MAX_CAN_ID: u32 = 0x7FF;

/// Where the response code sits in a negative response: `7F`, SID, code.
const RC_BYTE_OFFSET: u32 = 2;

/// An open link: the logical link on the worker and the event stream its primitives report on.
pub struct Link {
    pub module_handle: ModuleHandle,
    pub cll_handle: ComLogicalLinkHandle,
    pub events: Streaming<EventNotification>,
    /// What the job may send on this link: decided from the VCI the module reports (ADR-247).
    /// Not public: only [`open`] may decide it, so a library caller cannot grant a link to a
    /// real VCI the simulator permission.
    pub(crate) permission: Permission,
}

impl Link {
    /// What the job may send on this link.
    pub fn permission(&self) -> Permission {
        self.permission
    }
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

/// Opens the worker's only module (several are refused), creates a logical link for `config`, sets its ComParams and the
/// response ID table, connects it and subscribes to its events (ADR-235 item 4). The worker
/// answers 0x78 itself (`CP_RC78Handling`), so the procedure only sees final responses. If a
/// step fails, what was already opened is closed again.
pub async fn open(
    client: &mut WorkerClient,
    config: &LinkConfig,
    deadline: Duration,
) -> Result<Link, HostError> {
    open_tracked(client, config, deadline)
        .await
        .map_err(|failure| failure.error)
}

/// A link that failed to open.
#[derive(Debug)]
pub(crate) struct OpenFailure {
    pub(crate) error: HostError,
    /// Whether what the open had already opened on the worker was closed again, or nothing was
    /// opened. When it is false the worker may still hold the module or the link (ADR-258).
    pub(crate) cleaned_up: bool,
}

impl OpenFailure {
    /// A failure before anything was opened on the worker.
    fn clean(error: HostError) -> Self {
        Self {
            error,
            cleaned_up: true,
        }
    }
}

/// [`open`], also telling whether a failed open left anything open on the worker.
pub(crate) async fn open_tracked(
    client: &mut WorkerClient,
    config: &LinkConfig,
    deadline: Duration,
) -> Result<Link, OpenFailure> {
    config.validate().map_err(OpenFailure::clean)?;
    let modules = unary(
        deadline,
        "GetModuleIds",
        client.get_module_ids(GetModuleIdsRequest {}),
    )
    .await
    .map_err(OpenFailure::clean)?
    .module_id_list
    .map(|list| list.module_data)
    .unwrap_or_default();
    // Choosing among several VCIs on one worker is not supported yet; refuse rather than
    // connect to whichever the worker lists first.
    let module_handle = match modules.as_slice() {
        [only] => only
            .module_handle
            .ok_or(OpenFailure::clean(HostError::Setup(
                "the worker's module has no handle",
            )))?,
        [] => {
            return Err(OpenFailure::clean(HostError::Setup(
                "the worker reports no module",
            )));
        }
        _ => {
            return Err(OpenFailure::clean(HostError::Setup(
                "the worker reports several modules; choosing one is not supported yet",
            )));
        }
    };
    let connected = unary(
        deadline,
        "ModuleConnect",
        client.module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    if let Err(error) = connected {
        // The connect may have completed on the worker after the deadline. Disconnecting a
        // module that is not connected changes nothing; when that disconnect fails too, the
        // module is not known to be disconnected.
        let cleaned_up = disconnect_module(client, module_handle, deadline)
            .await
            .is_ok();
        return Err(OpenFailure { error, cleaned_up });
    }

    let permission = vci_permission(client, deadline, module_handle).await;

    let cll_handle = match create_link(client, config, deadline, module_handle).await {
        Ok(cll_handle) => cll_handle,
        Err(error) => {
            let cleaned_up = disconnect_module(client, module_handle, deadline)
                .await
                .is_ok();
            return Err(OpenFailure { error, cleaned_up });
        }
    };
    match set_up_link(client, config, deadline, cll_handle).await {
        Ok(events) => Ok(Link {
            module_handle,
            cll_handle,
            events,
            permission,
        }),
        Err(error) => {
            let cleaned_up = teardown(client, module_handle, cll_handle, deadline)
                .await
                .is_ok();
            Err(OpenFailure { error, cleaned_up })
        }
    }
}

/// What a job may send through this module (ADR-247). A debug build asks the worker which VCI
/// it fronts, since only the simulator may be written to. A VCI whose version cannot be read
/// is not the simulator: the job stays read-only, as it would in a release build.
#[cfg(debug_assertions)]
async fn vci_permission(
    client: &mut WorkerClient,
    deadline: Duration,
    module_handle: ModuleHandle,
) -> Permission {
    let version = unary(
        deadline,
        "GetVersion",
        client.get_version(vci_service_interface::GetVersionRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    match version {
        Ok(response) => response
            .version_data
            .as_ref()
            .map_or(Permission::ReadOnly, crate::policy::identify),
        Err(error) => {
            tracing::warn!(%error, "could not read the VCI's version; the job stays read-only");
            Permission::ReadOnly
        }
    }
}

/// A release build never asks: its links are read-only whatever the VCI (ADR-247).
#[cfg(not(debug_assertions))]
async fn vci_permission(
    _client: &mut WorkerClient,
    _deadline: Duration,
    _module_handle: ModuleHandle,
) -> Permission {
    Permission::ReadOnly
}

/// The resource a link is created on, by the ISO 22900-2 (2022) short names of its protocol,
/// bus type and DLC pins (10.1.4.8), which both worker kinds resolve (ADR-319).
pub fn resource_data(config: &LinkConfig) -> ResourceData {
    ResourceData {
        bus_type: Some(resource_data::BusType::BusTypeName(
            BUS_TYPE_DWCAN.to_owned(),
        )),
        protocol: Some(resource_data::Protocol::ProtocolName(
            config.protocol_short_name.clone(),
        )),
        dlc_pin_data: DWCAN_PINS
            .iter()
            .map(|&(dlc_pin_number, pin_type)| PinData {
                dlc_pin_number,
                dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName(pin_type.to_owned())),
            })
            .collect(),
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
            resource_data: Some(resource_data(config)),
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
    let own = [
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
    ];
    let declared = config
        .params
        .iter()
        .map(|(name, value)| (name.as_str(), *value));
    for (name, value) in own.into_iter().chain(declared) {
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
        permission: _,
    } = link;
    drop(events);
    teardown(client, module_handle, cll_handle, deadline).await
}

/// Runs every step even after one fails, and returns the first failure.
pub(crate) async fn teardown(
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
    let module_disconnected = disconnect_module(client, module_handle, deadline).await;
    // The first failure, after every step was tried.
    disconnected.and(destroyed).and(module_disconnected)
}

async fn disconnect_module(
    client: &mut WorkerClient,
    module_handle: ModuleHandle,
    deadline: Duration,
) -> Result<(), HostError> {
    let result = unary(
        deadline,
        "ModuleDisconnect",
        client.module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await
    .map(drop);
    if let Err(error) = &result {
        tracing::warn!(%error, "could not disconnect the module");
    }
    result
}

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;
    use diag_ir::declaration::{
        ComParamT, EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document,
    };

    use super::*;
    use crate::JobDocument;

    /// The example ECU's protocol: UDS over ISO 15765 at 500 kbit/s on 7E0 / 7E8.
    fn protocol(params: &[(&str, i64)]) -> ProtocolT {
        let mut protocol = ProtocolT::default();
        protocol.short_name = Some("ISO15765".into());
        protocol.application_layer = Some("UDS".into());
        protocol.baud_rate = 500_000;
        protocol.tx_id = 0x7E0;
        protocol.rx_id = 0x7E8;
        protocol.functional_id = 0x7DF;
        protocol.params = Some(
            params
                .iter()
                .map(|&(name, value)| {
                    let mut param = ComParamT::default();
                    param.short_name = Some(name.into());
                    param.value = value;
                    param
                })
                .collect(),
        );
        protocol
    }

    fn declared(protocols: Vec<ProtocolT>) -> Result<LinkConfig, HostError> {
        let mut variant = EcuVariantT::default();
        variant.id = 1;
        variant.short_name = Some("v".into());
        variant.protocols = Some(protocols);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        let document =
            JobDocument::new(encode_ecu_document(&document)).expect("the document loads");
        LinkConfig::declared(&document.variant())
    }

    #[test]
    fn a_declared_protocol_gives_the_link() {
        assert_eq!(
            declared(vec![protocol(&[("CP_BlockSize", 0), ("CP_StMin", 0)])]).expect("usable"),
            LinkConfig {
                params: vec![("CP_BlockSize".to_owned(), 0), ("CP_StMin".to_owned(), 0)],
                ..LinkConfig::iso15765(0x7E0, 0x7E8)
            }
        );
        // The timings are in microseconds, rounded up to whole milliseconds; the identifiers,
        // baud rate and either ISO 22900-2 name come from the protocol.
        let mut other = protocol(&[
            ("CP_P2Max", 50_000),
            ("CP_P2Star", 2_000_001),
            ("CP_RC78CompletionTimeout", 1),
        ]);
        other.short_name = Some("ISO_15765_3_on_ISO_15765_2".into());
        other.baud_rate = 250_000;
        other.tx_id = 0x710;
        other.rx_id = 0x718;
        assert_eq!(
            declared(vec![other]).expect("usable"),
            LinkConfig {
                baud_rate: 250_000,
                p2_max_ms: 50,
                p2_star_ms: 2_001,
                rc78_completion_ms: 1,
                ..LinkConfig::iso15765(0x710, 0x718)
            }
        );
    }

    #[test]
    fn a_link_the_agent_cannot_open_is_refused() {
        let with = |change: fn(&mut ProtocolT)| {
            let mut protocol = protocol(&[]);
            change(&mut protocol);
            protocol
        };
        for protocols in [
            Vec::new(),
            vec![protocol(&[]), protocol(&[])],
            // Not UDS on CAN.
            vec![with(|p| p.short_name = Some("CAN".into()))],
            vec![with(|p| p.short_name = None)],
            vec![with(|p| p.application_layer = Some("KWP2000".into()))],
            vec![with(|p| p.application_layer = None)],
            vec![with(|p| p.extended_addressing = true)],
            // What `validate` refuses.
            vec![with(|p| p.tx_id = 0x800)],
            vec![with(|p| p.baud_rate = 0)],
            vec![protocol(&[("CP_P2Max", 0)])],
            vec![protocol(&[("CP_P2Star", 4_294_967_296)])],
            // A ComParam the link sets itself, or one that does not fit a 32-bit value.
            vec![protocol(&[("CP_Baudrate", 500_000)])],
            vec![protocol(&[("CP_CanPhysReqId", 0x7E0)])],
            vec![protocol(&[("CP_RC78Handling", 0)])],
            vec![protocol(&[("CP_StMin", -1)])],
            vec![protocol(&[("CP_StMin", 0), ("CP_StMin", 1)])],
            vec![protocol(&[("CP_P2Max", 50_000), ("CP_P2Max", 60_000)])],
        ] {
            let error = declared(protocols.clone()).expect_err("refused");
            assert!(
                matches!(error, HostError::DeclaredLink(_)),
                "{protocols:?}: {error}"
            );
        }
    }
}
