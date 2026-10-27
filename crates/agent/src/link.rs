//! The communication link of a job (ADR-235 item 4).

use std::time::Duration;

use crate::client::WorkerApi;
use diag_ir::declaration::{EcuVariant, Protocol};
use tonic::Streaming;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EcuUniqueRespData,
    EventNotification, GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ObjectType, ParamItem,
    PduParamClass, PendingForwarding, PinData, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, SubscribeEventRequest, UniqueRespIdTableItem, VersionData,
    create_com_logical_link_request, param_item, pin_data, resource_data, subscribe_event_request,
};

use crate::host::{HostError, unary};
use crate::oplog::LogSafe;
use crate::policy::{ApplicationLayer, Permission};

/// The longest a classic ISO-TP message carries (a 12-bit length), the response the job-end
/// wait allows for ([`LinkConfig::job_end_wait`]).
pub const JOB_END_RESPONSE_BYTES: u64 = 4_095;
/// An upper bound of one classic CAN frame's bits on the bus, stuffing included.
pub const CAN_FRAME_BITS: u64 = 160;
/// The longest message one classic CAN frame carries as a single frame whatever the addressing
/// format: seven bytes only with normal addressing, six with extended or mixed (ISO
/// 15765-2:2024 9.6.2.1).
pub const CAN_SINGLE_FRAME_BYTES: usize = 6;
/// A message a link sends as one frame ([`LinkConfig::single_frame`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleFrame {
    /// The longest message one frame surely carries.
    pub max_bytes: usize,
    /// The longest that frame's transfer takes, in microseconds.
    pub transfer_us: u64,
}

/// The `CP_Cr` the job-end wait takes when the link leaves it to the library: the timeout
/// ISO 15765-2:2024 9.8.1 gives the receiver's wait for the next consecutive frame, 1 s.
pub const JOB_END_DEFAULT_CR_US: u64 = 1_000_000;
/// The job-end wait's cap (ADR-358).
pub const JOB_END_WAIT_MAX: Duration = Duration::from_secs(10);

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
    /// other ComParams are set on the link as they are. ComParam names are compared ignoring
    /// case, as the workers resolve them, and a ComParam with a unit is refused, since the agent
    /// cannot convert units. The result is validated.
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
            if seen.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
                return Err(refuse(format!("ComParam {name} is declared twice")));
            }
            seen.push(name);
            // A unit is an id of the server's unit table, which the agent does not have: a
            // value is taken in the ComParam's own unit (microseconds for a time).
            if param.unit_id() != 0 {
                return Err(refuse(format!(
                    "ComParam {name} has a unit; give it in the ComParam's own unit, without one"
                )));
            }
            let value = u32::try_from(param.value()).map_err(|_| {
                refuse(format!(
                    "ComParam {name} is {}, not a 32-bit unsigned value",
                    param.value()
                ))
            })?;
            let is = |standard: &str| name.eq_ignore_ascii_case(standard);
            let timing = if is("CP_P2Max") {
                Some(&mut config.p2_max_ms)
            } else if is("CP_P2Star") {
                Some(&mut config.p2_star_ms)
            } else if is("CP_RC78CompletionTimeout") {
                Some(&mut config.rc78_completion_ms)
            } else {
                None
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

    /// The application layer the link carries: UDS on the one protocol [`LinkConfig::validate`]
    /// accepts, and none on any other, so that a read-only job could send nothing on it.
    pub fn application_layer(&self) -> Option<ApplicationLayer> {
        (self.protocol_short_name == PROTOCOL_UDS_ON_CAN).then_some(ApplicationLayer::Uds)
    }

    /// The longest message the link surely sends as one frame, and the longest that frame's
    /// transfer takes at the baud rate (design 8.11 "Timing judged", ADR-364): on UDS on CAN,
    /// [`CAN_SINGLE_FRAME_BYTES`] in one frame of at most [`CAN_FRAME_BITS`]. `None` on any
    /// other protocol, which [`LinkConfig::validate`] refuses.
    pub fn single_frame(&self) -> Option<SingleFrame> {
        (self.protocol_short_name == PROTOCOL_UDS_ON_CAN && self.baud_rate != 0).then(|| {
            SingleFrame {
                max_bytes: CAN_SINGLE_FRAME_BYTES,
                transfer_us: (CAN_FRAME_BITS * 1_000_000).div_ceil(u64::from(self.baud_rate)),
            }
        })
    }

    /// How long the agent keeps the link open at job end for a primitive past its deadline
    /// before it cancels it (design 8.11 "Settlement", ADR-358): the link's worst case for a
    /// response already under way, one wait for a consecutive frame (`CP_Cr`) plus the transfer
    /// of the longest response a classic ISO-TP message carries, received at the separation
    /// time the tester's flow control requests (`CP_STmin`, ISO 15765-2:2024 9.6.5, 9.8.1), each
    /// frame taking at most [`CAN_FRAME_BITS`] at the baud rate; at most
    /// [`JOB_END_WAIT_MAX`]. A `CP_Cr` the link leaves to the library, or declares 0, counts as
    /// [`JOB_END_DEFAULT_CR_US`], an unset `CP_STmin` as none. By the agent's deadline every
    /// limit in force has passed, so none is added; an ECU that sends slower than `CP_STmin`
    /// is cut off by the cap.
    pub fn job_end_wait(&self) -> Duration {
        let param = |name: &str| {
            self.params
                .iter()
                .find(|(given, _)| given.eq_ignore_ascii_case(name))
                .map(|(_, value)| u64::from(*value))
        };
        // A zero `CP_Cr` is taken as the library's default, as a zero P2 is (`validate`).
        let cr_us = param("CP_Cr")
            .filter(|&cr| cr != 0)
            .unwrap_or(JOB_END_DEFAULT_CR_US);
        let st_min_us = param("CP_STmin").unwrap_or(0);
        // A first frame carries 6 bytes and each consecutive frame 7.
        let frames = 1 + (JOB_END_RESPONSE_BYTES - 6).div_ceil(7);
        let frame_us = (CAN_FRAME_BITS * 1_000_000).div_ceil(u64::from(self.baud_rate.max(1)));
        let wait = Duration::from_micros(cr_us + frames * (frame_us + st_min_us));
        wait.min(JOB_END_WAIT_MAX)
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
        // handling, and a further one would override it. The others would let a primitive run
        // past the host's deadline: the worker would go on sending after the host gave up.
        let named = |list: &[&str], name: &str| list.iter().any(|n| n.eq_ignore_ascii_case(name));
        if self
            .params
            .iter()
            .any(|(name, _)| named(&SET_BY_LINK, name) || named(&OUTLAST_THE_HOST, name))
        {
            return Err(HostError::Setup(
                "a further ComParam is one the link sets itself, or one that lets a primitive \
                 outlast the host's deadline (ADR-324 item 3)",
            ));
        }
        // Only the standard's ComParams: the J2534 worker also resolves names of its own, which
        // could name what the link sets under another name.
        if self
            .params
            .iter()
            .any(|(name, _)| !name.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("CP_")))
        {
            return Err(HostError::Setup(
                "a further ComParam must have a standard name, starting with CP_",
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
/// name the CSV tables use, or the ISO 22900-2 (2022) short name the link names its resource by
/// (Annex B.1, ADR-319).
const UDS_ON_CAN_NAMES: [&str; 2] = ["ISO15765", PROTOCOL_UDS_ON_CAN];
/// The ComParams the link sets itself ([`set_up_link`]).
const SET_BY_LINK: [&str; 12] = [
    "CP_Baudrate",
    "CP_P2Max",
    "CP_P2Star",
    "CP_RC78Handling",
    "CP_RCByteOffset",
    "CP_RC78CompletionTimeout",
    "CP_RC21Handling",
    "CP_RC23Handling",
    "CP_TransmitIndEnable",
    "CP_StartMsgIndEnable",
    "CP_CanPhysReqId",
    "CP_CanRespUSDTId",
];
/// The indications the link turns on for the communication log (design 8.11 "Link settings
/// for the log"): transmit indications and start-of-message indications, which the J2534 worker
/// passes on only under them (ADR-151). A library that refuses one opens the link without it.
pub(crate) const INDICATION_COMPARAMS: [&str; 2] = ["CP_TransmitIndEnable", "CP_StartMsgIndEnable"];
/// ComParams that let a primitive run longer than [`LinkConfig::send_recv_ceiling`] allows:
/// the limits and request times of repeating the request on a busy (0x21) or not-yet-complete
/// (0x23) answer, which the link turns off (`CP_RC21Handling`, `CP_RC23Handling`), repeating it
/// at the application or transport layer, and taking P2 and P2* from the ECU's session response.
const OUTLAST_THE_HOST: [&str; 7] = [
    "CP_RC21CompletionTimeout",
    "CP_RC21RequestTime",
    "CP_RC23CompletionTimeout",
    "CP_RC23RequestTime",
    "CP_RepeatReqCountApp",
    "CP_RepeatReqCountTrans",
    "CP_ModifyTiming",
];
/// The ID GetObjectId gives a name the implementation does not know (`PDU_ID_UNDEF`, ISO
/// 22900-2 (2022) 8.4.23).
const PDU_ID_UNDEF: u32 = 0xFFFF_FFFE;
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
    /// The application layer whose read-only list the policy applies to the link's requests
    /// (ADR-342), from [`LinkConfig::application_layer`].
    pub(crate) application_layer: Option<ApplicationLayer>,
    /// Which of [`INDICATION_COMPARAMS`] the library accepted.
    pub(crate) indications: Indications,
    /// The module's version data, for the communication log's `vci_connected` (design 8.11);
    /// `None` when the worker did not give it.
    pub(crate) version: Option<VersionData>,
    /// Whether the worker passes on every response pending the link sees, as its answer to
    /// `CreateComLogicalLink` says (ADR-355); `None` when it does not say. The agent takes it
    /// from the worker's answer alone (design principle 6).
    pub(crate) forwards_pendings: Option<bool>,
}

/// Which indications the link has on: those of [`INDICATION_COMPARAMS`] the library accepted
/// (design 8.11 "Link settings for the log").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Indications {
    /// `CP_TransmitIndEnable`.
    pub(crate) transmit: bool,
    /// `CP_StartMsgIndEnable`.
    pub(crate) start_of_message: bool,
}

impl Indications {
    /// Whether the library accepted the ComParam `name` of [`INDICATION_COMPARAMS`].
    pub(crate) fn accepted(&self, name: &str) -> bool {
        if name == INDICATION_COMPARAMS[0] {
            self.transmit
        } else {
            self.start_of_message
        }
    }
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
/// Which of [`own_comparams`] take their value from `variant`'s document rather than the
/// agent's default, by the names `own_comparams` gives: the baud rate always, and the timing
/// ComParams the document declares ([`LinkConfig::declared`]). Empty for a variant that
/// declares no usable protocol, whose link [`LinkConfig::declared`] refuses anyway.
pub(crate) fn valued_by_document(variant: &EcuVariant<'_>) -> Vec<&'static str> {
    let protocols: Vec<Protocol<'_>> = variant.protocols().into_iter().flatten().collect();
    let [protocol] = protocols.as_slice() else {
        return Vec::new();
    };
    let mut named = vec!["CP_Baudrate"];
    for param in protocol.params().into_iter().flatten() {
        let name = param.short_name().unwrap_or_default();
        for timing in ["CP_P2Max", "CP_P2Star", "CP_RC78CompletionTimeout"] {
            if name.eq_ignore_ascii_case(timing) {
                named.push(timing);
            }
        }
    }
    named
}

/// The ComParams the link sets itself, by name, before those the document declares; the
/// identifiers go into the response ID table instead, and the indications are set apart, since
/// a library may refuse them ([`INDICATION_COMPARAMS`]). The communication log's `link_opened`
/// names the same (design 8.11).
pub(crate) fn own_comparams(config: &LinkConfig) -> [(&'static str, u32); 8] {
    [
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
        // The API repeats no request on a busy (0x21) or not-yet-complete (0x23) answer, which
        // the log could not see (design 8.11 "Link settings for the log"): the one profile, UDS,
        // declares neither a pending code (ADR-309 item 5).
        ("CP_RC21Handling", 0),
        ("CP_RC23Handling", 0),
    ]
}

fn micros(ms: u32) -> u32 {
    ms.saturating_mul(1_000)
}

async fn comparam_id<C: WorkerApi>(
    client: &mut C,
    deadline: Duration,
    shortname: &str,
) -> Result<u32, HostError> {
    let request = GetObjectIdRequest {
        object_type: ObjectType::ObjtComparam as i32,
        shortname: shortname.to_owned(),
    };
    let id = unary(deadline, "GetObjectId", client.get_object_id(request))
        .await?
        .pdu_object_id;
    if id == PDU_ID_UNDEF {
        return Err(HostError::DeclaredLink(format!(
            "the worker does not know ComParam {shortname}"
        )));
    }
    Ok(id)
}

/// Opens the worker's only module (several are refused), creates a logical link for `config`, sets its ComParams and the
/// response ID table, connects it and subscribes to its events (ADR-235 item 4). The worker
/// answers 0x78 itself (`CP_RC78Handling`), so the procedure only sees final responses. If a
/// step fails, what was already opened is closed again.
pub async fn open<C: WorkerApi>(
    client: &mut C,
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
pub(crate) async fn open_tracked<C: WorkerApi>(
    client: &mut C,
    config: &LinkConfig,
    deadline: Duration,
) -> Result<Link, OpenFailure> {
    config.validate().map_err(OpenFailure::clean)?;
    // A client that holds the module (the hardware harness's guarded client) has it opened and
    // closes it itself: the job neither lists, connects nor disconnects it.
    let module_handle = match client.held_module() {
        Some(held) => held,
        None => open_module(client, deadline).await?,
    };

    let version = vci_version(client, deadline, module_handle).await;
    let permission = vci_permission(version.as_ref());

    let (cll_handle, forwards_pendings) =
        match create_link(client, config, deadline, module_handle).await {
            Ok(created) => created,
            Err(error) => {
                // A module the client holds is not disconnected here, so that proves nothing: the
                // failed create left nothing behind only when the worker refused it. A create that
                // timed out or failed otherwise may have made a link nobody holds a handle for
                // (ADR-258).
                let cleaned_up = if client.held_module().is_some() {
                    nothing_created(&error)
                } else {
                    disconnect_module(client, module_handle, deadline)
                        .await
                        .is_ok()
                };
                return Err(OpenFailure { error, cleaned_up });
            }
        };
    match set_up_link(client, config, deadline, cll_handle).await {
        Ok((events, indications)) => Ok(Link {
            module_handle,
            cll_handle,
            events,
            permission,
            application_layer: config.application_layer(),
            indications,
            version,
            forwards_pendings,
        }),
        Err(error) => {
            let cleaned_up = teardown(client, module_handle, cll_handle, deadline)
                .await
                .is_ok();
            Err(OpenFailure { error, cleaned_up })
        }
    }
}

/// Opens the worker's only module (several are refused) and returns its handle. A module that
/// fails to connect is disconnected again.
async fn open_module<C: WorkerApi>(
    client: &mut C,
    deadline: Duration,
) -> Result<ModuleHandle, OpenFailure> {
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
    Ok(module_handle)
}

/// The setup error `create_link` returns when `GetResourceIds` finds no resource, before any
/// `CreateComLogicalLink` is sent.
const NO_RESOURCE: &str = "no resource for the protocol";

/// Whether a failed `create_link` created nothing: it failed in `GetResourceIds`, before any
/// `CreateComLogicalLink` was sent, or the worker (or the guard in front of it, as
/// `PermissionDenied`) refused the create. A timeout or any other status of the create, or a
/// create that answered without a link handle, does not say.
fn nothing_created(error: &HostError) -> bool {
    use tonic::Code::{
        AlreadyExists, FailedPrecondition, InvalidArgument, NotFound, PermissionDenied,
        ResourceExhausted, Unauthenticated, Unimplemented,
    };
    match error {
        HostError::Rpc {
            rpc: "GetResourceIds",
            ..
        }
        | HostError::WorkerUnresponsive("GetResourceIds") => true,
        HostError::Rpc { status, .. } => matches!(
            status.code(),
            PermissionDenied
                | InvalidArgument
                | FailedPrecondition
                | NotFound
                | ResourceExhausted
                | Unimplemented
                | AlreadyExists
                | Unauthenticated
        ),
        HostError::Setup(message) => *message == NO_RESOURCE,
        _ => false,
    }
}

/// The module's version data (`GetVersion`), read in every build for the communication log's
/// `vci_connected` (design 8.11). A worker that does not give it leaves `None`, with a warning.
async fn vci_version<C: WorkerApi>(
    client: &mut C,
    deadline: Duration,
    module_handle: ModuleHandle,
) -> Option<VersionData> {
    let version = unary(
        deadline,
        "GetVersion",
        client.get_version(vci_service_interface::GetVersionRequest {
            module_handle: Some(module_handle),
        }),
    )
    .await;
    match version {
        Ok(response) => response.version_data,
        Err(error) => {
            tracing::warn!(
                error = %error.log_text(),
                external = error.log_external(),
                "could not read the VCI's version; the job stays read-only"
            );
            None
        }
    }
}

/// What a job may send through this module (ADR-247). A debug build decides from the VCI the
/// version data names, since only the simulator may be written to. A VCI whose version could
/// not be read is not the simulator: the job stays read-only, as it would in a release build.
#[cfg(debug_assertions)]
fn vci_permission(version: Option<&VersionData>) -> Permission {
    version.map_or(Permission::ReadOnly, crate::policy::identify)
}

/// A release build's links are read-only whatever the VCI (ADR-247).
#[cfg(not(debug_assertions))]
fn vci_permission(_version: Option<&VersionData>) -> Permission {
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

async fn create_link<C: WorkerApi>(
    client: &mut C,
    config: &LinkConfig,
    deadline: Duration,
    module_handle: ModuleHandle,
) -> Result<(ComLogicalLinkHandle, Option<bool>), HostError> {
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
    .ok_or(HostError::Setup(NO_RESOURCE))?;

    let created = unary(
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
    .await?;
    let cll_handle = created
        .cll_handle
        .ok_or(HostError::Setup("no logical link handle"))?;
    Ok((cll_handle, forwards_pendings(created.pending_forwarding)))
}

/// Whether the worker said the link forwards every pending (ADR-355): what it says, and only
/// that; an unspecified or unknown value is not a guess.
fn forwards_pendings(pending_forwarding: i32) -> Option<bool> {
    match PendingForwarding::try_from(pending_forwarding) {
        Ok(PendingForwarding::All) => Some(true),
        Ok(PendingForwarding::NotAll) => Some(false),
        Ok(PendingForwarding::Unspecified) | Err(_) => None,
    }
}

async fn set_up_link<C: WorkerApi>(
    client: &mut C,
    config: &LinkConfig,
    deadline: Duration,
    cll_handle: ComLogicalLinkHandle,
) -> Result<(Streaming<EventNotification>, Indications), HostError> {
    let own = own_comparams(config);
    let declared = config
        .params
        .iter()
        .map(|(name, value)| (name.as_str(), *value, true));
    // The identifiers go into the response ID table below; resolved first, so a declared alias
    // of either is caught as well.
    let phys_req = comparam_id(client, deadline, "CP_CanPhysReqId").await?;
    let resp = comparam_id(client, deadline, "CP_CanRespUSDTId").await?;
    let mut set = vec![phys_req, resp];
    for (name, value, is_declared) in own
        .into_iter()
        .map(|(name, value)| (name, value, false))
        .chain(declared)
    {
        let id = comparam_id(client, deadline, name).await?;
        // A declared ComParam the worker resolves to one already set, under an alias the
        // name checks did not catch, would override it (ADR-324 item 3).
        if is_declared && set.contains(&id) {
            return Err(HostError::DeclaredLink(format!(
                "ComParam {name} sets what the link or another ComParam already sets"
            )));
        }
        set.push(id);
        // The worker takes a declared ComParam's class from the ComParam itself, so a timing
        // or bus-type one is set as such (ISO 22900-2 (2022) 10.1.4.5).
        let class = if is_declared {
            PduParamClass::PduPcSpecified
        } else {
            PduParamClass::PduPcCom
        };
        unary(
            deadline,
            "SetComParam",
            client.set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(param(id, class, value)),
            }),
        )
        .await?;
    }
    let mut indications = Indications::default();
    for name in INDICATION_COMPARAMS {
        let accepted = set_indication(client, deadline, cll_handle, name, &mut set).await?;
        if name == INDICATION_COMPARAMS[0] {
            indications.transmit = accepted;
        } else {
            indications.start_of_message = accepted;
        }
    }

    // The table makes the worker install the flow-control filter for the ECU's IDs.
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

    let events = unary(
        deadline,
        "SubscribeEvent",
        client.subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        }),
    )
    .await?;
    Ok((events, indications))
}

/// Turns the indication `name` of [`INDICATION_COMPARAMS`] on, and returns whether the library
/// accepted it. A library that does not know the ComParam, or refuses it, leaves the link
/// without the indication (design 8.11 "Link settings for the log"); a worker that does not
/// answer, or whose connection is gone, fails the link as for any other ComParam. `set` holds
/// the ids already set, so a ComParam the worker resolves to one of them is not set again.
async fn set_indication<C: WorkerApi>(
    client: &mut C,
    deadline: Duration,
    cll_handle: ComLogicalLinkHandle,
    name: &'static str,
    set: &mut Vec<u32>,
) -> Result<bool, HostError> {
    // A D-PDU library may answer an unknown name with an error rather than `PDU_ID_UNDEF`.
    let id = match comparam_id(client, deadline, name).await {
        Ok(id) => id,
        Err(HostError::DeclaredLink(_)) => return Ok(false),
        Err(HostError::Rpc { status, .. }) if refused(&status) => return Ok(false),
        Err(error) => return Err(error),
    };
    if set.contains(&id) {
        return Ok(false);
    }
    set.push(id);
    match unary(
        deadline,
        "SetComParam",
        client.set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(param(id, PduParamClass::PduPcCom, 1)),
        }),
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(HostError::Rpc { status, .. }) if refused(&status) => {
            tracing::info!(
                comparam = name,
                "the library refused an indication ComParam"
            );
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Whether `status` is the worker refusing a request, rather than the connection to it failing.
fn refused(status: &tonic::Status) -> bool {
    !matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Cancelled | tonic::Code::Unknown
    )
}

/// Disconnects and destroys the logical link and disconnects the module.
pub async fn close<C: WorkerApi>(
    client: &mut C,
    link: Link,
    deadline: Duration,
) -> Result<(), HostError> {
    let Link {
        module_handle,
        cll_handle,
        events,
        permission: _,
        application_layer: _,
        indications: _,
        version: _,
        forwards_pendings: _,
    } = link;
    drop(events);
    teardown(client, module_handle, cll_handle, deadline).await
}

/// Runs every step even after one fails, and returns the first failure.
pub(crate) async fn teardown<C: WorkerApi>(
    client: &mut C,
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

async fn disconnect_module<C: WorkerApi>(
    client: &mut C,
    module_handle: ModuleHandle,
    deadline: Duration,
) -> Result<(), HostError> {
    // A module the client holds is closed by its owner.
    if client.held_module().is_some() {
        return Ok(());
    }
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
        tracing::warn!(
            error = %error.log_text(),
            external = error.log_external(),
            "could not disconnect the module"
        );
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

    /// ADR-364: on UDS on CAN a message of up to six bytes is one frame of at most 160 bits;
    /// another protocol has no bound.
    #[test]
    fn a_single_frame_is_bounded_on_uds_on_can_only() {
        let mut config = LinkConfig::iso15765(0x7E0, 0x7E8);
        assert_eq!(
            config.single_frame(),
            Some(SingleFrame {
                max_bytes: 6,
                transfer_us: 320,
            })
        );
        config.baud_rate = 125_000;
        assert_eq!(
            config.single_frame().map(|frame| frame.transfer_us),
            Some(1_280)
        );
        config.protocol_short_name = "ISO_14230_3_on_ISO_14230_2".to_owned();
        assert_eq!(config.single_frame(), None);
    }

    /// ADR-358: the job-end wait is one `CP_Cr` wait plus a 4095-byte response at `CP_STmin`,
    /// a frame taking at most 160 bits, capped at 10 s; unset ComParams count as 1 s and none.
    #[test]
    fn the_job_end_wait_is_the_link_s_worst_case_for_a_response() {
        // 586 frames of 320 µs at 500 kbit/s, after the 1 s default.
        let plain = LinkConfig::iso15765(0x7E0, 0x7E8);
        assert_eq!(
            plain.job_end_wait(),
            Duration::from_micros(1_000_000 + 586 * 320)
        );
        let declared = LinkConfig {
            params: vec![
                ("cp_cr".to_owned(), 150_000),
                ("CP_STmin".to_owned(), 1_000),
            ],
            ..LinkConfig::iso15765(0x7E0, 0x7E8)
        };
        assert_eq!(
            declared.job_end_wait(),
            Duration::from_micros(150_000 + 586 * (320 + 1_000))
        );
        let slow = LinkConfig {
            baud_rate: 10_000,
            ..LinkConfig::iso15765(0x7E0, 0x7E8)
        };
        assert_eq!(slow.job_end_wait(), JOB_END_WAIT_MAX);
    }

    /// ADR-355: the worker's answer, and only it, says whether the link forwards every pending;
    /// an unspecified or unknown value is unknown.
    #[test]
    fn forwards_pendings_is_what_the_worker_answered() {
        assert_eq!(forwards_pendings(PendingForwarding::All as i32), Some(true));
        assert_eq!(
            forwards_pendings(PendingForwarding::NotAll as i32),
            Some(false)
        );
        assert_eq!(
            forwards_pendings(PendingForwarding::Unspecified as i32),
            None
        );
        assert_eq!(forwards_pendings(7), None);
    }

    /// Only a refusal or a missing resource proves a failed create left no link: a create that
    /// answered without a handle, or a timeout, may have created one the runner cannot name.
    #[test]
    fn a_failed_create_is_clean_only_when_nothing_can_have_been_created() {
        assert!(nothing_created(&HostError::Setup(NO_RESOURCE)));
        assert!(!nothing_created(&HostError::Setup(
            "no logical link handle"
        )));
        assert!(nothing_created(&HostError::Rpc {
            rpc: "CreateComLogicalLink",
            status: Box::new(tonic::Status::permission_denied("refused")),
        }));
        assert!(!nothing_created(&HostError::Rpc {
            rpc: "CreateComLogicalLink",
            status: Box::new(tonic::Status::unavailable("gone")),
        }));
        assert!(!nothing_created(&HostError::WorkerUnresponsive(
            "CreateComLogicalLink"
        )));
        // The resource lookup comes before the create, so any failure of it left no link.
        assert!(nothing_created(&HostError::Rpc {
            rpc: "GetResourceIds",
            status: Box::new(tonic::Status::unavailable("gone")),
        }));
        assert!(nothing_created(&HostError::WorkerUnresponsive(
            "GetResourceIds"
        )));
    }

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

    #[test]
    fn a_comparam_with_a_unit_is_refused() {
        let mut protocol = protocol(&[("CP_P2Max", 50)]);
        protocol.params.as_mut().expect("params")[0].unit_id = 2;
        let error = declared(vec![protocol]).expect_err("refused");
        assert!(
            matches!(&error, HostError::DeclaredLink(why) if why.contains("unit")),
            "{error}"
        );
    }

    /// The link's own ComParams whose value the document gives: the baud rate, and a timing
    /// ComParam only when it is declared, under the link's spelling of its name.
    #[test]
    fn the_document_values_the_baud_rate_and_the_timings_it_declares() {
        assert_eq!(
            valued_by(vec![protocol(&[("cp_p2max", 50_000), ("CP_StMin", 0)])]),
            ["CP_Baudrate", "CP_P2Max"]
        );
        assert_eq!(valued_by(vec![protocol(&[])]), ["CP_Baudrate"]);
        assert!(valued_by(Vec::new()).is_empty());
    }

    fn valued_by(protocols: Vec<ProtocolT>) -> Vec<&'static str> {
        valued_by_document(&document_of(protocols).variant())
    }

    fn document_of(protocols: Vec<ProtocolT>) -> JobDocument {
        let mut variant = EcuVariantT::default();
        variant.id = 1;
        variant.short_name = Some("v".into());
        variant.protocols = Some(protocols);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        JobDocument::new(encode_ecu_document(&document)).expect("the document loads")
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
        // The timings are in microseconds, rounded up to whole milliseconds, and named in any
        // case; the identifiers and baud rate come from the protocol, which may also be named
        // by the stack's ISO 22900-2 name.
        let mut other = protocol(&[
            ("cp_p2max", 50_000),
            ("CP_P2Star", 2_000_001),
            ("CP_RC78CompletionTimeout", 1),
        ]);
        other.short_name = Some(PROTOCOL_UDS_ON_CAN.into());
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

    /// The read-only list the host applies is the link's layer's (ADR-342): UDS on the one
    /// protocol `validate` accepts, none on any other.
    #[test]
    fn a_link_carries_uds_only_on_uds_on_can() {
        assert_eq!(
            LinkConfig::iso15765(0x7E0, 0x7E8).application_layer(),
            Some(ApplicationLayer::Uds)
        );
        for name in ["ISO_11898_RAW", "ISO15765", ""] {
            let config = LinkConfig {
                protocol_short_name: name.to_owned(),
                ..LinkConfig::iso15765(0x7E0, 0x7E8)
            };
            assert_eq!(config.application_layer(), None, "{name:?}");
        }
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
            // Names compare ignoring case, as the workers resolve them.
            vec![protocol(&[("CP_StMin", 0), ("cp_stmin", 1)])],
            vec![protocol(&[("cp_baudrate", 250_000)])],
            vec![protocol(&[("CP_RC78HANDLING", 0)])],
            vec![protocol(&[("CP_TransmitIndEnable", 0)])],
            vec![protocol(&[("CP_RC23Handling", 1)])],
            // A name that is not the standard's, which the J2534 worker may alias.
            vec![protocol(&[("DATA_RATE", 250_000)])],
            // ComParams that let a primitive outlast the host's deadline.
            vec![protocol(&[("CP_RC21Handling", 1)])],
            vec![protocol(&[("CP_ModifyTiming", 1)])],
            vec![protocol(&[("CP_RepeatReqCountApp", 3)])],
            // Only the transport's name or the one the link names its resource by.
            vec![with(|p| {
                p.short_name = Some("ISO_15765_3_on_ISO_15765_2".into())
            })],
        ] {
            let error = declared(protocols.clone()).expect_err("refused");
            assert!(
                matches!(error, HostError::DeclaredLink(_)),
                "{protocols:?}: {error}"
            );
        }
    }
}
