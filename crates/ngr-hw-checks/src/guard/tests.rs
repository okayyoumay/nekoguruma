use vci_service_interface::{
    CancelComPrimitiveRequest, CllCreateFlagBit, ComLogicalLinkHandle, ComOperationType,
    ComPrimitiveCtrlData, ComPrimitiveHandle, ConnectComLogicalLinkRequest, DataItem,
    EcuUniqueRespData, GetStatusRequest, IoCtlRequest, ModuleAndResourceId, ModuleHandle,
    ObjectType, ParamItem, PduParamClass, PinData, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, TxFlag, TxFlagBit,
    UniqueRespIdTableItem, com_primitive_ctrl_data, create_com_logical_link_request as create,
    get_status_request, io_ctl_request, param_item, pin_data, resource_data,
};

use super::*;
use crate::profile::{Setup, parse};

const MODULE: u32 = 1;
const CLL: u32 = 10;
const PROTOCOL: &str = "ISO_14229_3_on_ISO_15765_2";

/// The profile's link, with what a test changes.
struct L {
    id_bits: u8,
    request: u32,
    response: u32,
    functional: Option<u32>,
    addressing: &'static str,
    address: Option<u8>,
    padding: Option<u8>,
    comparams: &'static str,
}

impl Default for L {
    fn default() -> Self {
        L {
            id_bits: 11,
            request: 0x7E0,
            response: 0x7E8,
            functional: Some(0x7DF),
            addressing: "normal",
            address: None,
            padding: None,
            comparams: "",
        }
    }
}

fn link_toml(l: &L) -> String {
    let mut text = format!(
        "protocol = \"{PROTOCOL}\"\nbaud_rate = 500000\npins = [6, 14]\nrequest_id = {}\n\
         response_id = {}\nid_bits = {}\naddressing = \"{}\"\n",
        l.request, l.response, l.id_bits, l.addressing
    );
    if let Some(f) = l.functional {
        text += &format!("functional_id = {f}\n");
    }
    if let Some(a) = l.address {
        text += &format!("address = {a}\n");
    }
    if let Some(p) = l.padding {
        text += &format!("padding = {p}\n");
    }
    if !l.comparams.is_empty() {
        text += &format!("[link.comparams]\n{}\n", l.comparams);
    }
    text
}

const VCI: &str = "api = \"j2534-0404\"\nlibrary = \"Vendor VCI\"\nmodule = \"VCI-1\"\n\
                   label = \"bench\"\npname = \"Vendor VCI 1\"\nfirmware_version = \"1.2.3\"\n\
                   library_version = \"4.5\"\n";

fn profile_with(class: &str, l: &L) -> Profile {
    parse(&format!(
        "class = \"{class}\"\n[vci]\n{VCI}\n[link]\n{}",
        link_toml(l)
    ))
    .unwrap()
}

fn profile(class: &str) -> Profile {
    profile_with(class, &L::default())
}

/// One end of a T0 loopback on a channel of the profile's VCI.
fn t0_end(channel: &str, request: u32, response: u32, padding: Option<u8>) -> String {
    format!(
        "[[endpoint]]\nchannel = \"{channel}\"\n[endpoint.vci]\n{VCI}\n[endpoint.link]\n{}",
        link_toml(&L {
            request,
            response,
            functional: None,
            padding,
            ..L::default()
        })
    )
}

/// A T0 loopback of two channels of one VCI, each end's request identifier the other's response.
fn loopback() -> Profile {
    parse(&format!(
        "class = \"T0\"\n{}{}",
        t0_end("CAN_CH1", 0x123, 0x124, None),
        t0_end("CAN_CH2", 0x124, 0x123, None)
    ))
    .unwrap()
}

struct Env {
    profile: Profile,
    vci: crate::profile::Vci,
    links: LinkTable,
    learned: Learned,
}

impl Env {
    fn new(profile: Profile) -> Self {
        let vci = match &profile.setup {
            Setup::Link { vci, .. } => vci.clone(),
            Setup::Loopback(ends) => ends.iter().find_map(|e| e.vci.clone()).unwrap(),
        };
        Self::for_vci(profile, vci)
    }

    /// The context of the worker that fronts `vci`.
    fn for_vci(profile: Profile, vci: crate::profile::Vci) -> Self {
        Env {
            profile,
            vci,
            links: LinkTable::new(),
            learned: Learned::default(),
        }
    }

    /// With the link `CLL` bound to endpoint 0.
    fn linked(profile: Profile, raw_mode: bool) -> Self {
        let mut env = Self::new(profile);
        env.links.insert(CLL, BoundLink::new(0, raw_mode));
        env
    }

    /// With the link `CLL` bound and every profile-pinned ComParam recorded as set.
    fn ready(profile: Profile) -> Self {
        let mut env = Self::linked(profile, false);
        let required = link::required(env.endpoint_link(0), env.profile.class);
        env.links.get_mut(&CLL).unwrap().set.extend(required);
        env
    }

    fn endpoint_link(&self, index: usize) -> &crate::profile::Link {
        match &self.profile.setup {
            crate::profile::Setup::Link { link, .. } => link,
            crate::profile::Setup::Loopback(ends) => &ends[index].link,
        }
    }

    fn decide(&self, request: Request<'_>) -> Result<Grant, GuardRefusal> {
        decide(
            &Context {
                profile: &self.profile,
                module: MODULE,
                vci: &self.vci,
                links: &self.links,
                learned: &self.learned,
            },
            &request,
        )
    }
}

fn module() -> Option<ModuleHandle> {
    Some(ModuleHandle {
        module_handle: MODULE,
    })
}

fn cll() -> Option<ComLogicalLinkHandle> {
    Some(ComLogicalLinkHandle {
        module_handle: MODULE,
        cll_handle: CLL,
    })
}

fn pins(list: &[(u32, &str)]) -> Vec<PinData> {
    list.iter()
        .map(|&(number, kind)| PinData {
            dlc_pin_number: number,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName(kind.to_owned())),
        })
        .collect()
}

fn resource(protocol: &str, bus: &str, pin_list: &[(u32, &str)]) -> ResourceData {
    ResourceData {
        bus_type: Some(resource_data::BusType::BusTypeName(bus.to_owned())),
        protocol: Some(resource_data::Protocol::ProtocolName(protocol.to_owned())),
        dlc_pin_data: pins(pin_list),
    }
}

fn profile_resource() -> ResourceData {
    resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(6, "HI"), (14, "LOW")])
}

fn create_request(resource: create::Resource) -> CreateComLogicalLinkRequest {
    CreateComLogicalLinkRequest {
        module_handle: module(),
        resource: Some(resource),
        cll_create_flag: None,
    }
}

/// Creates the link for `resource`: in RawMode on T0, which creates no other kind.
fn create_with(env: &Env, resource: &ResourceData) -> Result<Grant, GuardRefusal> {
    let mut request = create_request(create::Resource::RscData(resource.clone()));
    if env.profile.class == Class::T0 {
        request.cll_create_flag = Some(create::CllCreateFlag::CllCreateFlagBits(
            vci_service_interface::CllCreateFlag {
                bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
            },
        ));
    }
    env.decide(Request::CreateComLogicalLink(&request))
}

fn param_by_name(name: &str, data: param_item::ParamData) -> ParamItem {
    ParamItem {
        id: Some(param_item::Id::ParamName(name.to_owned())),
        com_param_class: PduParamClass::PduPcCom as i32,
        param_data: Some(data),
    }
}

fn unum(name: &str, value: u32) -> ParamItem {
    param_by_name(name, param_item::ParamData::Unum32(value))
}

fn set(env: &Env, item: ParamItem) -> Result<Grant, GuardRefusal> {
    env.decide(Request::SetComParam(&SetComParamRequest {
        cll_handle: cll(),
        param_item: Some(item),
    }))
}

/// The response table entries of the default 11-bit normal link: the identifiers and the
/// formats (0x05: segmented, with flow control).
fn table_items(phys: u32, resp: u32) -> Vec<ParamItem> {
    vec![
        unum("CP_CanPhysReqId", phys),
        unum("CP_CanRespUSDTId", resp),
        unum("CP_CanPhysReqFormat", 0x05),
        unum("CP_CanRespUSDTFormat", 0x05),
    ]
}

fn set_table(env: &Env, items: Vec<ParamItem>) -> Result<Grant, GuardRefusal> {
    env.decide(Request::SetUniqueRespIdTable(
        &SetUniqueRespIdTableRequest {
            cll_handle: cll(),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: items,
                }],
            }),
        },
    ))
}

fn start(cop_type: ComOperationType, data: &[u8], cycles: i32) -> StartComPrimitiveRequest {
    StartComPrimitiveRequest {
        cll_handle: cll(),
        cop_type: cop_type as i32,
        cop_data: data.to_vec(),
        cop_ctrl_data: Some(ComPrimitiveCtrlData {
            num_send_cycles: cycles,
            ..ComPrimitiveCtrlData::default()
        }),
        cop_tag: None,
    }
}

fn start_flags(data: &[u8], flag: com_primitive_ctrl_data::TxFlag) -> StartComPrimitiveRequest {
    let mut request = start(ComOperationType::CoptSendrecv, data, 1);
    request.cop_ctrl_data.as_mut().unwrap().tx_flag = Some(flag);
    request
}

fn flag_bits(bits: &[TxFlagBit]) -> com_primitive_ctrl_data::TxFlag {
    com_primitive_ctrl_data::TxFlag::TxFlagBits(TxFlag {
        bits: bits.iter().map(|&b| b as i32).collect(),
    })
}

fn send(env: &Env, data: &[u8], cycles: i32) -> Result<Grant, GuardRefusal> {
    env.decide(Request::StartComPrimitive(&start(
        ComOperationType::CoptSendrecv,
        data,
        cycles,
    )))
}

fn ioctl(env: &Env, command: io_ctl_request::IoCtrlCommand) -> Result<Grant, GuardRefusal> {
    env.decide(Request::IoCtl(&IoCtlRequest {
        handle: Some(io_ctl_request::Handle::CllHandle(cll().unwrap())),
        io_ctrl_command: Some(command),
        input_data: None::<DataItem>,
        has_output: false,
    }))
}

fn ioctl_name(env: &Env, name: &str) -> Result<Grant, GuardRefusal> {
    ioctl(
        env,
        io_ctl_request::IoCtrlCommand::IoCtrlCommandName(name.to_owned()),
    )
}

// ---------------------------------------------------------------------------------------------
// Link setup

#[test]
fn the_profiles_resource_is_created_by_data_and_by_learned_id() {
    let mut env = Env::new(profile("T3"));
    assert_eq!(
        create_with(&env, &profile_resource()),
        Ok(Grant::CreatesLink {
            endpoint: 0,
            raw_mode: false
        })
    );
    // The protocol name is compared ignoring ASCII case.
    let lower = resource(
        &PROTOCOL.to_ascii_lowercase(),
        "iso_11898_2_dwcan",
        &[(14, "low"), (6, "hi")],
    );
    assert!(create_with(&env, &lower).is_ok());

    env.learned.resources.insert(7, profile_resource());
    assert_eq!(
        env.decide(Request::CreateComLogicalLink(&create_request(
            create::Resource::ResourceId(7)
        ))),
        Ok(Grant::CreatesLink {
            endpoint: 0,
            raw_mode: false
        })
    );
}

#[test]
fn a_resource_that_is_not_the_profiles_is_refused() {
    let env = Env::new(profile("T3"));
    let wrong_id = env.decide(Request::CreateComLogicalLink(&create_request(
        create::Resource::ResourceId(9),
    )));
    assert_eq!(
        wrong_id,
        Err(GuardRefusal::UnknownId {
            kind: "resource",
            id: 9
        })
    );
    let by_name = env.decide(Request::CreateComLogicalLink(&create_request(
        create::Resource::ResourceName("ISO_14229_3_on_ISO_15765_2".to_owned()),
    )));
    assert!(matches!(by_name, Err(GuardRefusal::LinkSetup(_))));

    let hi_low = [(6, "HI"), (14, "LOW")];
    let cases = [
        resource("ISO_15765_3_on_ISO_15765_2", "ISO_11898_2_DWCAN", &hi_low),
        resource("ISO_11898_RAW", "ISO_11898_2_DWCAN", &hi_low),
        resource(PROTOCOL, "ISO_11898_3_DWFTCAN", &hi_low),
        // Swapped, other and third pins.
        resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(6, "LOW"), (14, "HI")]),
        resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(3, "HI"), (14, "LOW")]),
        resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(6, "HI"), (11, "LOW")]),
        resource(
            PROTOCOL,
            "ISO_11898_2_DWCAN",
            &[(6, "HI"), (14, "LOW"), (3, "K")],
        ),
        resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(6, "HI")]),
        resource(PROTOCOL, "ISO_11898_2_DWCAN", &[(6, "HI"), (6, "HI")]),
    ];
    for case in cases {
        assert!(
            matches!(create_with(&env, &case), Err(GuardRefusal::LinkSetup(_))),
            "{case:?}"
        );
    }
    // A resource without protocol or bus type is not the profile's either.
    let mut no_protocol = profile_resource();
    no_protocol.protocol = None;
    assert!(create_with(&env, &no_protocol).is_err());
}

#[test]
fn an_unlearned_pin_type_id_is_refused_and_a_learned_one_compares_by_name() {
    let mut env = Env::new(profile("T3"));
    let mut by_id = profile_resource();
    by_id.dlc_pin_data = vec![
        PinData {
            dlc_pin_number: 6,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeId(1)),
        },
        PinData {
            dlc_pin_number: 14,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeId(2)),
        },
    ];
    assert!(matches!(
        create_with(&env, &by_id),
        Err(GuardRefusal::UnknownId { .. })
    ));
    env.learned
        .objects
        .insert((ObjectType::ObjtPintype, 1), "HI".to_owned());
    env.learned
        .objects
        .insert((ObjectType::ObjtPintype, 2), "LOW".to_owned());
    assert!(create_with(&env, &by_id).is_ok());
}

#[test]
fn only_the_raw_mode_create_flag_is_allowed() {
    // RawMode exists on T0 only.
    let env = Env::new(loopback());
    let mut request = create_request(create::Resource::RscData(channel_resource("CAN_CH1")));
    let with = |request: &mut CreateComLogicalLinkRequest, flag| {
        request.cll_create_flag = Some(flag);
        env.decide(Request::CreateComLogicalLink(request))
    };
    let bits = |bits: &[CllCreateFlagBit]| {
        create::CllCreateFlag::CllCreateFlagBits(vci_service_interface::CllCreateFlag {
            bits: bits.iter().map(|&b| b as i32).collect(),
        })
    };
    assert_eq!(
        with(
            &mut request,
            bits(&[CllCreateFlagBit::CllCreateFlagRawMode])
        ),
        Ok(Grant::CreatesLink {
            endpoint: 0,
            raw_mode: true
        })
    );
    assert!(
        with(
            &mut request,
            bits(&[CllCreateFlagBit::CllCreateFlagChecksumMode])
        )
        .is_err()
    );
    assert!(with(&mut request, bits(&[CllCreateFlagBit::Unspecified])).is_err());
    assert_eq!(
        with(
            &mut request,
            create::CllCreateFlag::CllCreateFlagRaw(vec![0x80, 0, 0, 0])
        ),
        Ok(Grant::CreatesLink {
            endpoint: 0,
            raw_mode: true
        })
    );
    for raw in [vec![0x40], vec![0xC0], vec![0x80, 0x01]] {
        assert!(
            with(&mut request, create::CllCreateFlag::CllCreateFlagRaw(raw)).is_err(),
            "a create flag with a bit other than RawMode"
        );
    }
}

#[test]
fn a_second_link_is_refused_on_a_one_link_profile() {
    let env = Env::linked(profile("T1"), false);
    assert_eq!(
        create_with(&env, &profile_resource()),
        Err(GuardRefusal::LinkCount)
    );
}

/// The resource of a loopback end that is a channel of the VCI, named by its channel.
fn channel_resource(channel: &str) -> ResourceData {
    resource(channel, "ISO_11898_2_DWCAN", &[(6, "HI"), (14, "LOW")])
}

fn create_channel(env: &Env, channel: &str) -> Result<Grant, GuardRefusal> {
    create_with(env, &channel_resource(channel))
}

/// The grant of a T0 link, which is always RawMode.
fn creates(endpoint: usize) -> Result<Grant, GuardRefusal> {
    Ok(Grant::CreatesLink {
        endpoint,
        raw_mode: true,
    })
}

#[test]
fn a_loopback_takes_one_link_per_channel_and_binds_the_one_named() {
    let mut env = Env::new(loopback());
    // Each end creates its own channel's link, and a request for B binds B first.
    assert_eq!(create_channel(&env, "CAN_CH1"), creates(0));
    assert_eq!(create_channel(&env, "CAN_CH2"), creates(1));
    // The name is read as the worker reads it.
    assert_eq!(create_channel(&env, "can_ch02"), creates(1));
    env.links.insert(CLL, BoundLink::new(1, false));
    assert_eq!(create_channel(&env, "CAN_CH1"), creates(0));
    // Endpoint B's resource twice is refused.
    assert_eq!(
        create_channel(&env, "CAN_CH2"),
        Err(GuardRefusal::LinkCount)
    );
    env.links.insert(11, BoundLink::new(0, false));
    // A third channel is no endpoint's, and neither is the profile's protocol name.
    for name in ["CAN_CH3", PROTOCOL, "CAN"] {
        assert!(
            matches!(create_channel(&env, name), Err(GuardRefusal::LinkSetup(_))),
            "{name}"
        );
    }
    assert_eq!(
        create_channel(&env, "CAN_CH1"),
        Err(GuardRefusal::LinkCount)
    );
}

#[test]
fn a_socketcan_end_has_no_worker_link() {
    let text = format!(
        "class = \"T0\"\n[[endpoint]]\nsocketcan = \"can0\"\n[endpoint.link]\n{}\
         [[endpoint]]\nchannel = \"CAN_CH1\"\n[endpoint.vci]\n{VCI}\n[endpoint.link]\n{}",
        link_toml(&L {
            request: 0x123,
            response: 0x124,
            functional: None,
            ..L::default()
        }),
        link_toml(&L {
            request: 0x124,
            response: 0x123,
            functional: None,
            ..L::default()
        })
    );
    let mut env = Env::new(parse(&text).unwrap());
    // Only the VCI end is a worker link, and it is endpoint 1.
    assert_eq!(create_channel(&env, "CAN_CH1"), creates(1));
    env.links.insert(CLL, BoundLink::new(1, false));
    assert_eq!(
        create_channel(&env, "CAN_CH1"),
        Err(GuardRefusal::LinkCount)
    );
    assert!(create_channel(&env, PROTOCOL).is_err());
}

#[test]
fn comparams_are_compared_with_the_profile() {
    let env = Env::linked(profile("T3"), false);
    assert!(set(&env, unum("CP_Baudrate", 500_000)).is_ok());
    assert!(set(&env, unum("cp_baudrate", 500_000)).is_ok());
    assert!(set(&env, unum("CP_Baudrate", 250_000)).is_err());
    // A snum with the same value is the same value.
    assert!(
        set(
            &env,
            param_by_name("CP_Baudrate", param_item::ParamData::Snum32(500_000))
        )
        .is_ok()
    );
    assert!(
        set(
            &env,
            param_by_name("CP_Baudrate", param_item::ParamData::Bytefield(vec![1]))
        )
        .is_err()
    );
    // Other physical-layer ComParams, whatever the value, in any case.
    for name in ["CP_TerminationType", "cp_canfdbaudrate", "CP_ListenOnly"] {
        assert!(set(&env, unum(name, 0)).is_err(), "{name}");
    }
    // A ComParam of the bus type class that no list names is physical too.
    let mut unknown_bus = unum("CP_VendorWire", 1);
    unknown_bus.com_param_class = PduParamClass::PduPcBustype as i32;
    assert!(set(&env, unknown_bus).is_err());

    assert!(set(&env, unum("CP_CanPhysReqId", 0x7E0)).is_ok());
    assert!(set(&env, unum("CP_CanPhysReqId", 0x7E1)).is_err());
    assert!(set(&env, unum("CP_CanRespUSDTId", 0x7E8)).is_ok());
    assert!(set(&env, unum("CP_CanRespUSDTId", 0x7E9)).is_err());
    assert!(set(&env, unum("CP_CanRespUUDTId", 0x7E8)).is_err());
    assert!(set(&env, unum("CP_CanFuncReqId", 0x7DF)).is_ok());
    assert!(set(&env, unum("CP_CanFuncReqId", 0x7DE)).is_err());
}

#[test]
fn an_alias_of_the_baud_rate_is_compared_like_it() {
    let env = Env::linked(profile("T3"), false);
    for name in ["DATA_RATE", "data_rate", "CP_Baudrate"] {
        assert!(set(&env, unum(name, 500_000)).is_ok(), "{name}");
        assert!(set(&env, unum(name, 250_000)).is_err(), "{name}");
        assert!(
            set_table(&env, vec![unum(name, 250_000)]).is_err(),
            "{name}"
        );
        assert!(
            set_table(
                &env,
                [vec![unum(name, 500_000)], table_items(0x7E0, 0x7E8)].concat()
            )
            .is_ok(),
            "{name}"
        );
    }
    // Through a learned id as well.
    let mut env = env;
    env.learned
        .objects
        .insert((ObjectType::ObjtComparam, 5), "DATA_RATE".to_owned());
    let by_id = |value| ParamItem {
        id: Some(param_item::Id::ParamId(5)),
        com_param_class: PduParamClass::PduPcCom as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    };
    assert!(set(&env, by_id(500_000)).is_ok());
    assert!(set(&env, by_id(250_000)).is_err());
}

#[test]
fn every_alias_resolves_to_its_canonical_rule() {
    use crate::comparam::{ALIASES, canonical};
    // A profile that sets padding, a functional id and an address, so the rules have values.
    let env = Env::linked(
        profile_with(
            "T3",
            &L {
                padding: Some(0x55),
                comparams: "CP_BlockSize = 8",
                ..L::default()
            },
        ),
        false,
    );
    for (alias, canon) in ALIASES {
        assert_eq!(canonical(alias), *canon, "{alias}");
        assert_eq!(canonical(&alias.to_ascii_uppercase()), *canon, "{alias}");
        // The canonical name is not itself an alias, so there is one step.
        assert_eq!(canonical(canon), *canon, "{canon}");
        for value in [0, 1, 8, 500_000, 0x7E0, 0x55] {
            assert_eq!(
                set(&env, unum(alias, value)).is_ok(),
                set(&env, unum(canon, value)).is_ok(),
                "{alias} / {canon} = {value}"
            );
        }
    }
}

#[test]
fn unknown_and_vendor_comparams_are_refused_whatever_their_class() {
    let env = Env::linked(profile("T3"), false);
    let classes = [
        PduParamClass::PduPcSpecified,
        PduParamClass::PduPcTiming,
        PduParamClass::PduPcInit,
        PduParamClass::PduPcCom,
        PduParamClass::PduPcErrhdl,
        PduParamClass::PduPcBustype,
        PduParamClass::PduPcUniqueId,
        PduParamClass::PduPcTesterPresent,
    ];
    for name in [
        "CP_VendorWire",
        "CP_TesterPresentImmed",
        "VENDOR_SPECIFIC",
        "CP_SamplesPerBit",
        "CP_W1Max",
        "CP_P3Min",
        "CP_SwCan_HighVoltage",
        "CP_J1939SourceAddress",
        "CP_CanFirstConsecutiveFrameValue",
        "",
    ] {
        for class in classes {
            let mut item = unum(name, 1);
            item.com_param_class = class as i32;
            assert!(
                matches!(set(&env, item.clone()), Err(GuardRefusal::LinkSetup(_))),
                "{name} {class:?}"
            );
            assert!(set_table(&env, vec![item]).is_err(), "{name} {class:?}");
        }
    }
    // The class never admits a name, and the allow-listed ones pass under any class but the
    // bus type's.
    let mut timing = unum("CP_P2Max", 1);
    timing.com_param_class = PduParamClass::PduPcSpecified as i32;
    assert!(set(&env, timing.clone()).is_ok());
    timing.com_param_class = PduParamClass::PduPcBustype as i32;
    assert!(set(&env, timing).is_err());
}

#[test]
fn each_allow_listed_comparam_passes() {
    let env = Env::linked(profile("T3"), false);
    for name in [
        "CP_P2Max",
        "CP_P2Star",
        "CP_P2Max_Ecu",
        "CP_P2Star_Ecu",
        "CP_P3Phys",
        "CP_P3Func",
        "CP_CyclicRespTimeout",
        "CP_Ar",
        "CP_Ar_Ecu",
        "CP_As",
        "CP_As_Ecu",
        "CP_Br",
        "CP_Br_Ecu",
        "CP_Bs",
        "CP_Bs_Ecu",
        "CP_Cr",
        "CP_Cr_Ecu",
        "CP_Cs",
        "CP_Cs_Ecu",
        "CP_BlockSize",
        "CP_BlockSize_Ecu",
        "CP_BlockSizeOverride",
        "CP_STmin",
        "CP_STmin_Ecu",
        "CP_STminOverride",
        "CP_CanMaxNumWaitFrames",
        "CP_RC78Handling",
        "CP_RC78CompletionTimeout",
        "CP_RCByteOffset",
        "CP_TesterPresentTime",
        "CP_TesterPresentHandling",
        "CP_TesterPresentReqRsp",
        "CP_TesterPresentExpPosResp",
        "CP_TesterPresentExpNegResp",
        "CP_TesterPresentSendType",
        // The aliases of some of them.
        "ISO15765_STMIN",
        "ISO15765_BS",
        "BS_TX",
        "P2_MAX",
    ] {
        assert!(set(&env, unum(name, 1)).is_ok(), "{name}");
    }
}

#[test]
fn identifiers_in_the_response_table_are_compared_too() {
    let env = Env::linked(profile("T3"), false);
    let table = |phys: u32, resp: u32| set_table(&env, table_items(phys, resp));
    assert!(table(0x7E0, 0x7E8).is_ok());
    assert!(table(0x7E1, 0x7E8).is_err());
    assert!(table(0x7E0, 0x7E9).is_err());
}

#[test]
fn a_functional_identifier_needs_one_in_the_profile() {
    let env = Env::linked(
        profile_with(
            "T3",
            &L {
                functional: None,
                ..L::default()
            },
        ),
        false,
    );
    assert!(set(&env, unum("CP_CanFuncReqId", 0x7DF)).is_err());
    assert!(set(&env, unum("CP_RequestAddrMode", 1)).is_ok());
    assert!(set(&env, unum("CP_RequestAddrMode", 2)).is_err());
    assert!(set(&env, unum("CP_TesterPresentAddrMode", 0)).is_ok());
    assert!(set(&env, unum("CP_TesterPresentAddrMode", 1)).is_err());
    let with = Env::linked(profile("T3"), false);
    assert!(set(&with, unum("CP_RequestAddrMode", 2)).is_ok());
    assert!(set(&with, unum("CP_TesterPresentAddrMode", 1)).is_ok());
    assert!(set(&with, unum("CP_RequestAddrMode", 9)).is_err());
    assert!(set(&with, unum("CP_TesterPresentAddrMode", 2)).is_err());
}

#[test]
fn a_comparam_the_profile_fixes_keeps_its_value() {
    let env = Env::linked(
        profile_with(
            "T3",
            &L {
                comparams: "CP_BlockSize = 8",
                ..L::default()
            },
        ),
        false,
    );
    assert!(set(&env, unum("CP_BlockSize", 8)).is_ok());
    assert!(set(&env, unum("cp_blocksize", 0)).is_err());
}

#[test]
fn timing_comparams_pass_and_ids_resolve_through_what_was_learned() {
    let mut env = Env::linked(profile("T3"), false);
    assert!(set(&env, unum("CP_P2Max", 1_000_000)).is_ok());
    assert!(set(&env, unum("CP_RC78Handling", 1)).is_ok());

    let by_id = |id: u32, value: u32| ParamItem {
        id: Some(param_item::Id::ParamId(id)),
        com_param_class: PduParamClass::PduPcCom as i32,
        param_data: Some(param_item::ParamData::Unum32(value)),
    };
    assert_eq!(
        set(&env, by_id(0x80A1, 1)),
        Err(GuardRefusal::UnknownId {
            kind: "ComParam",
            id: 0x80A1
        })
    );
    env.learned
        .objects
        .insert((ObjectType::ObjtComparam, 0x80A1), "CP_P2Max".to_owned());
    env.learned
        .objects
        .insert((ObjectType::ObjtComparam, 0x80A2), "CP_Baudrate".to_owned());
    assert!(set(&env, by_id(0x80A1, 1)).is_ok());
    // The learned name is what is compared.
    assert!(set(&env, by_id(0x80A2, 500_000)).is_ok());
    assert!(set(&env, by_id(0x80A2, 125_000)).is_err());
    // An id in a response table resolves the same way.
    assert!(set_table(&env, vec![by_id(0x80A2, 125_000)]).is_err());
    // A ComParam with neither id nor name cannot be classified.
    let mut nameless = unum("CP_P2Max", 1);
    nameless.id = None;
    assert!(matches!(
        set(&env, nameless),
        Err(GuardRefusal::Undecodable(_))
    ));
}

#[test]
fn a_tester_present_message_is_judged_as_a_request() {
    let message = |bytes: &[u8]| {
        param_by_name(
            "CP_TesterPresentMessage",
            param_item::ParamData::Bytefield(bytes.to_vec()),
        )
    };
    let t3 = Env::linked(profile("T3"), false);
    assert!(set(&t3, message(&[0x3E, 0x80])).is_ok());
    assert!(set(&t3, message(&[0x3E, 0x00])).is_ok());
    // The worker sends it cyclically: nothing but TesterPresent, session control included.
    for other in [
        &[0x10, 0x03][..],
        &[0x10, 0x01],
        &[0x22, 0xF1, 0x90],
        &[0x19, 0x0A],
        &[0x2E, 0xF1, 0x90, 0x01],
    ] {
        assert!(set(&t3, message(other)).is_err(), "{other:02x?}");
    }
    assert!(set(&t3, message(&[0x2E, 0xF1, 0x90, 0x01])).is_err());
    let alias = param_by_name(
        "CP_TesterPresentMsg",
        param_item::ParamData::Bytefield(vec![0x2E, 0xF1, 0x90, 0x01]),
    );
    assert!(set(&t3, alias).is_err());
    assert!(set(&t3, unum("CP_TesterPresentMessage", 0x3E)).is_err());
    // The same on T1 (a cyclic session change would need teardown state); T0 has no ECU to
    // address.
    let t1 = Env::linked(profile("T1"), false);
    assert!(set(&t1, message(&[0x3E, 0x00])).is_ok());
    assert!(set(&t1, message(&[0x3E, 0x80])).is_ok());
    for other in [
        &[0x10, 0x02][..],
        &[0x2E, 0xF1, 0x90, 0x01],
        &[0x3E, 0x01],
        &[],
    ] {
        assert!(set(&t1, message(other)).is_err(), "{other:02x?}");
    }
    assert!(set(&Env::linked(loopback(), false), message(&[0x3E, 0x80])).is_err());
}

#[test]
fn the_can_id_format_must_describe_the_profiles_link() {
    let normal = Env::linked(profile("T3"), false);
    // Table B.13: bit 2 segmented, bit 1 29-bit, bit 3 and 6 the addressing, bits 5 and 4 the
    // padding overwrite, bit 0 flow control.
    for name in [
        "CP_CanPhysReqFormat",
        "CP_CanFuncReqFormat",
        "CP_CanRespUSDTFormat",
    ] {
        assert!(set(&normal, unum(name, 0x05)).is_ok(), "{name}");
        assert!(set(&normal, unum(name, 0x04)).is_ok(), "{name}");
        // 29-bit, extended, mixed, unsegmented, undefined bits.
        for wrong in [0x07, 0x0D, 0x4D, 0x01, 0x85] {
            assert!(
                set(&normal, unum(name, wrong)).is_err(),
                "{name} {wrong:#x}"
            );
        }
    }
    assert!(set(&normal, unum("CP_CanRespUUDTFormat", 0x00)).is_ok());
    assert!(set(&normal, unum("CP_CanRespUUDTFormat", 0x02)).is_err());

    let wide = Env::linked(
        profile_with(
            "T3",
            &L {
                id_bits: 29,
                request: 0x18DA_10F1,
                response: 0x18DA_F110,
                functional: None,
                addressing: "mixed",
                address: Some(0x55),
                padding: Some(0xCC),
                ..L::default()
            },
        ),
        false,
    );
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x4E | 0x01)).is_ok());
    // Padding overwrite: 11 enables it, which the profile's padding allows; 10 disables it.
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x4E | 0x30)).is_ok());
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x4E | 0x20)).is_err());
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x4E | 0x10)).is_err());
    // Extended instead of mixed, 11-bit.
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x0E)).is_err());
    assert!(set(&wide, unum("CP_CanPhysReqFormat", 0x45)).is_err());
    // The address bytes and the filler.
    for name in ["CP_CanPhysReqExtAddr", "CP_CanRespUSDTExtAddr"] {
        assert!(set(&wide, unum(name, 0x55)).is_ok());
        assert!(set(&wide, unum(name, 0x56)).is_err());
        assert!(set(&normal, unum(name, 0)).is_ok());
        assert!(set(&normal, unum(name, 0x55)).is_err());
    }
    assert!(set(&wide, unum("CP_CanFillerByte", 0xCC)).is_ok());
    assert!(set(&wide, unum("CP_CanFillerByte", 0x55)).is_err());
    assert!(set(&wide, unum("CP_CanFillerByteHandling", 1)).is_ok());
    assert!(set(&wide, unum("CP_CanFillerByteHandling", 0)).is_err());
    assert!(set(&normal, unum("CP_CanFillerByteHandling", 0)).is_ok());
    assert!(set(&normal, unum("CP_CanFillerByteHandling", 1)).is_err());
    assert!(set(&normal, unum("CP_CanFillerByte", 0x1FF)).is_err());
}

fn connect(env: &Env) -> Result<Grant, GuardRefusal> {
    env.decide(Request::ConnectComLogicalLink(
        &ConnectComLogicalLinkRequest { cll_handle: cll() },
    ))
}

/// What the seam does with a grant after the worker accepted the request.
fn apply(env: &mut Env, grant: Result<Grant, GuardRefusal>) {
    if let Ok(Grant::SetsComParams { cll, names }) = grant {
        env.links.get_mut(&cll).unwrap().set.extend(names);
    }
}

#[test]
fn a_link_connects_only_once_the_profiles_comparams_are_set() {
    let link = L {
        comparams: "CP_BlockSize = 8",
        ..L::default()
    };
    let mut env = Env::linked(profile_with("T3", &link), false);
    // Right after create nothing is set.
    assert!(matches!(connect(&env), Err(GuardRefusal::LinkSetup(_))));

    // A ComParam without a profile value (timing) sets nothing the connect needs.
    let timing = set(&env, unum("CP_P2Max", 1));
    assert_eq!(timing, Ok(Grant::Pass));
    apply(&mut env, timing);

    let steps: Vec<(&str, ParamItem, &str)> = vec![
        ("baud", unum("DATA_RATE", 500_000), "cp_baudrate"),
        ("blocksize", unum("cp_blocksize", 8), "cp_blocksize"),
    ];
    for (label, item, canonical) in steps {
        let grant = set(&env, item);
        assert_eq!(
            grant,
            Ok(Grant::SetsComParams {
                cll: CLL,
                names: vec![canonical.to_owned()]
            }),
            "{label}"
        );
        apply(&mut env, grant);
        assert!(connect(&env).is_err(), "still missing after {label}");
    }
    // The identifiers go in through the response table, the functional one through SetComParam.
    let table = set_table(&env, table_items(0x7E0, 0x7E8));
    assert_eq!(
        table,
        Ok(Grant::SetsComParams {
            cll: CLL,
            names: [
                "cp_canphysreqformat",
                "cp_canphysreqid",
                "cp_canrespusdtformat",
                "cp_canrespusdtid"
            ]
            .map(str::to_owned)
            .to_vec()
        })
    );
    apply(&mut env, table);
    // All but one set: refused, and the message names what is missing.
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(why.contains("cp_canfuncreqid"), "{why}");
    let grant = set(&env, unum("CP_CanFuncReqId", 0x7DF));
    apply(&mut env, grant);
    // A 29-bit format on an 11-bit profile is refused; the functional format is still missing.
    assert!(set(&env, unum("CP_CanFuncReqFormat", 0x07)).is_err());
    let grant = set(&env, unum("CP_CanFuncReqFormat", 0x05));
    apply(&mut env, grant);
    // The worker's preset sends tester present functionally: both modes are pinned.
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(why.contains("cp_testerpresentaddrmode"), "{why}");
    for (name, value) in [("CP_RequestAddrMode", 1), ("CP_TesterPresentAddrMode", 0)] {
        let grant = set(&env, unum(name, value));
        assert!(grant.is_ok(), "{name}");
        apply(&mut env, grant);
    }
    // The worker's preset pads, so an unpadded profile has to switch the padding off.
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(why.contains("cp_canfillerbytehandling"), "{why}");
    assert!(set(&env, unum("CP_CanFillerByteHandling", 1)).is_err());
    let grant = set(&env, unum("CP_CanFillerByteHandling", 0));
    apply(&mut env, grant);
    // The tester-present message is the last one: the preset arms a cyclic one, and a D-PDU
    // library's default is unknown. Only TesterPresent passes the rule, and records it.
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(why.contains("cp_testerpresentmessage"), "{why}");
    let message = |bytes: &[u8]| {
        param_by_name(
            "CP_TesterPresentMessage",
            param_item::ParamData::Bytefield(bytes.to_vec()),
        )
    };
    assert!(set(&env, message(&[0x10, 0x03])).is_err());
    assert!(connect(&env).is_err());
    let grant = set(&env, message(&[0x3E, 0x80]));
    assert_eq!(
        grant,
        Ok(Grant::SetsComParams {
            cll: CLL,
            names: vec!["cp_testerpresentmessage".to_owned()]
        })
    );
    apply(&mut env, grant);
    assert_eq!(connect(&env), Ok(Grant::Pass));

    // A value changed to another than the profile's is refused at SetComParam anyway, and a
    // set that is refused records nothing.
    assert!(set(&env, unum("CP_Baudrate", 250_000)).is_err());
    assert!(set(&env, unum("DATA_RATE", 250_000)).is_err());
}

#[test]
fn the_connect_check_requires_what_the_profile_pins() {
    let l = L {
        id_bits: 29,
        request: 0x18DA_10F1,
        response: 0x18DA_F110,
        functional: None,
        addressing: "extended",
        address: Some(0x42),
        padding: Some(0x55),
        comparams: "CP_STmin = 2",
    };
    let profile = profile_with("T2", &l);
    let Setup::Link { link, .. } = &profile.setup else {
        unreachable!()
    };
    assert_eq!(
        link::required(link, Class::T2),
        [
            "cp_baudrate",
            "cp_canfillerbyte",
            "cp_canfillerbytehandling",
            "cp_canphysreqextaddr",
            "cp_canphysreqformat",
            "cp_canphysreqid",
            "cp_canrespusdtextaddr",
            "cp_canrespusdtformat",
            "cp_canrespusdtid",
            "cp_requestaddrmode",
            "cp_stmin",
            "cp_testerpresentaddrmode",
            "cp_testerpresentmessage",
        ]
    );
    // The plain 11-bit link needs the baud rate and the identifiers only.
    let plain = profile_with(
        "T3",
        &L {
            functional: None,
            ..L::default()
        },
    );
    let Setup::Link { link, .. } = &plain.setup else {
        unreachable!()
    };
    assert_eq!(
        link::required(link, Class::T3),
        [
            "cp_baudrate",
            "cp_canfillerbytehandling",
            "cp_canphysreqformat",
            "cp_canphysreqid",
            "cp_canrespusdtformat",
            "cp_canrespusdtid",
            "cp_requestaddrmode",
            "cp_testerpresentaddrmode",
            "cp_testerpresentmessage"
        ]
    );
}

#[test]
fn a_response_table_replaces_the_old_one_and_must_be_complete() {
    let mut env = Env::linked(profile("T3"), false);
    let complete = table_items;
    // A complete table passes and records its names.
    let grant = set_table(&env, complete(0x7E0, 0x7E8));
    assert_eq!(
        grant,
        Ok(Grant::SetsComParams {
            cll: CLL,
            names: [
                "cp_canphysreqformat",
                "cp_canphysreqid",
                "cp_canrespusdtformat",
                "cp_canrespusdtid"
            ]
            .map(str::to_owned)
            .to_vec()
        })
    );
    apply(&mut env, grant);
    // An empty table, or one without the response id or the request id, is refused: also after
    // a complete one, since the new table replaces it.
    assert!(matches!(
        set_table(&env, vec![]),
        Err(GuardRefusal::LinkSetup(_))
    ));
    let Err(GuardRefusal::LinkSetup(why)) = set_table(&env, vec![unum("CP_CanPhysReqId", 0x7E0)])
    else {
        panic!("a table without the response id is refused");
    };
    assert!(why.contains("cp_canrespusdtid"), "{why}");
    assert!(set_table(&env, vec![unum("CP_CanRespUSDTId", 0x7E8)]).is_err());
    // The pairing must sit in one entry: split across two, or repeated in a second entry, it
    // is refused, since the worker addresses from the first entry and pairs within one.
    let two = |first: Vec<ParamItem>, second: Vec<ParamItem>| {
        env.decide(Request::SetUniqueRespIdTable(
            &SetUniqueRespIdTableRequest {
                cll_handle: cll(),
                unique_resp_id_table: Some(UniqueRespIdTableItem {
                    unique_data: vec![
                        EcuUniqueRespData {
                            unique_resp_identifier: 1,
                            params: first,
                        },
                        EcuUniqueRespData {
                            unique_resp_identifier: 2,
                            params: second,
                        },
                    ],
                }),
            },
        ))
    };
    assert!(matches!(
        two(
            vec![unum("CP_CanPhysReqId", 0x7E0)],
            vec![unum("CP_CanRespUSDTId", 0x7E8)]
        ),
        Err(GuardRefusal::LinkSetup(_))
    ));
    assert!(matches!(
        two(complete(0x7E0, 0x7E8), complete(0x7E0, 0x7E8)),
        Err(GuardRefusal::LinkSetup(_))
    ));
    let none = env.decide(Request::SetUniqueRespIdTable(
        &SetUniqueRespIdTableRequest {
            cll_handle: cll(),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![],
            }),
        },
    ));
    assert!(matches!(none, Err(GuardRefusal::LinkSetup(_))));
    // A wrong value is refused as before.
    assert!(set_table(&env, complete(0x7E1, 0x7E8)).is_err());
}

#[test]
fn a_table_must_carry_the_formats_and_address_the_link_needs() {
    let env = Env::linked(
        profile_with(
            "T3",
            &L {
                id_bits: 29,
                request: 0x18DA_10F1,
                response: 0x18DA_F110,
                functional: Some(0x18DB_33F1),
                addressing: "extended",
                address: Some(0x42),
                ..L::default()
            },
        ),
        false,
    );
    let ids = vec![
        unum("CP_CanPhysReqId", 0x18DA_10F1),
        unum("CP_CanRespUSDTId", 0x18DA_F110),
    ];
    assert!(set_table(&env, ids.clone()).is_err());
    let mut full = ids;
    full.extend([
        unum("CP_CanPhysReqFormat", 0x0F),
        unum("CP_CanRespUSDTFormat", 0x0F),
        unum("CP_CanPhysReqExtAddr", 0x42),
        unum("CP_CanRespUSDTExtAddr", 0x42),
    ]);
    // The functional identifier is a COM-class ComParam, not held by the table.
    assert!(set_table(&env, full).is_ok());
}

/// A T0 loopback of two VCIs (two J2534 modules), no channels.
fn two_vci_loopback() -> Profile {
    let vci = |label: &str, pname: &str| {
        VCI.replace("label = \"bench\"", &format!("label = \"{label}\""))
            .replace("pname = \"Vendor VCI 1\"", &format!("pname = \"{pname}\""))
    };
    let end = |vci: String, request: u32, response: u32| {
        format!(
            "[[endpoint]]\n[endpoint.vci]\n{vci}\n[endpoint.link]\n{}",
            link_toml(&L {
                request,
                response,
                functional: None,
                ..L::default()
            })
        )
    };
    parse(&format!(
        "class = \"T0\"\n{}{}",
        end(vci("bench-a", "Vendor VCI A"), 0x123, 0x124),
        end(vci("bench-b", "Vendor VCI B"), 0x124, 0x123)
    ))
    .unwrap()
}

#[test]
fn each_worker_of_a_two_vci_loopback_binds_only_its_own_end() {
    let profile = two_vci_loopback();
    let Setup::Loopback(ends) = &profile.setup else {
        unreachable!()
    };
    let vci_a = ends[0].vci.clone().unwrap();
    let vci_b = ends[1].vci.clone().unwrap();
    let mut other = vci_a.clone();
    let a = Env::for_vci(profile.clone(), vci_a);
    let b = Env::for_vci(profile, vci_b);
    // The resource is the same for both; the context decides. B creating first binds B.
    assert_eq!(create_with(&b, &profile_resource()), creates(1));
    assert_eq!(create_with(&a, &profile_resource()), creates(0));
    // Another VCI's context still binds only its own end, once.
    let mut a = a;
    a.links.insert(CLL, BoundLink::new(0, false));
    assert_eq!(
        create_with(&a, &profile_resource()),
        Err(GuardRefusal::LinkCount)
    );
    let mut b = b;
    b.links.insert(CLL, BoundLink::new(1, false));
    assert_eq!(
        create_with(&b, &profile_resource()),
        Err(GuardRefusal::LinkCount)
    );
    // A VCI the profile does not name gets no link.
    other.label = Some("elsewhere".to_owned());
    other.pname = Some("Another VCI".to_owned());
    let stranger = Env::for_vci(a.profile.clone(), other);
    assert!(create_with(&stranger, &profile_resource()).is_err());
}

#[test]
fn both_channels_of_one_vci_stay_available_to_its_worker() {
    // The two-channel loopback: one VCI, so one worker holds both ends.
    let env = Env::new(loopback());
    assert_eq!(create_channel(&env, "CAN_CH1"), creates(0));
    assert_eq!(create_channel(&env, "CAN_CH2"), creates(1));
}

#[test]
fn t0_has_raw_links_only_and_nothing_to_start() {
    let env = Env::new(loopback());
    // A link that is not RawMode would speak the protocol.
    let request = create_request(create::Resource::RscData(channel_resource("CAN_CH1")));
    assert!(matches!(
        env.decide(Request::CreateComLogicalLink(&request)),
        Err(GuardRefusal::LinkSetup(_))
    ));
    assert_eq!(create_channel(&env, "CAN_CH1"), creates(0));
    // No communication to start or stop, whatever the data, on a raw link too.
    let raw = Env::linked(loopback(), true);
    for cop in [
        ComOperationType::CoptStartcomm,
        ComOperationType::CoptStopcomm,
    ] {
        for data in [&[][..], &[0x10, 0x03]] {
            assert!(
                matches!(
                    raw.decide(Request::StartComPrimitive(&start(cop, data, 0))),
                    Err(GuardRefusal::Frame(_))
                ),
                "{cop:?} {data:02x?}"
            );
        }
    }
    // A raw frame to its own request identifier still passes.
    assert!(raw_send(&raw, &frame(0x123, &[0x01, 0x00])).is_ok());
    // T1 still starts communication.
    let t1 = Env::linked(profile("T1"), false);
    assert!(
        t1.decide(Request::StartComPrimitive(&start(
            ComOperationType::CoptStartcomm,
            &[],
            0
        )))
        .is_ok()
    );
}

#[test]
fn raw_frame_flags_must_equal_the_profile_in_both_directions() {
    // RawMode exists on T0: a 29-bit, extended, padded loopback.
    let l = |request, response| L {
        id_bits: 29,
        request,
        response,
        functional: None,
        addressing: "extended",
        address: Some(0x42),
        padding: Some(0x55),
        ..L::default()
    };
    let end = |channel: &str, request, response| {
        format!(
            "[[endpoint]]\nchannel = \"{channel}\"\n[endpoint.vci]\n{VCI}\n[endpoint.link]\n{}",
            link_toml(&l(request, response))
        )
    };
    let wide = parse(&format!(
        "class = \"T0\"\n{}{}",
        end("CAN_CH1", 0x18DA_10F1, 0x18DA_F110),
        end("CAN_CH2", 0x18DA_F110, 0x18DA_10F1)
    ))
    .unwrap();
    let env = Env::linked(wide, true);
    let data = frame(0x18DA_10F1, &[0x42, 0x03, 0x22, 0xF1, 0x90]);
    let all = [
        TxFlagBit::TxFlagCan29bitId,
        TxFlagBit::TxFlagIso15765AddrType,
        TxFlagBit::TxFlagIso15765FramePad,
    ];
    let send_with = |bits: &[TxFlagBit]| {
        env.decide(Request::StartComPrimitive(&start_flags(
            &data,
            flag_bits(bits),
        )))
    };
    assert!(send_with(&all).is_ok());
    // The same as raw bytes: byte 2 bit 0, byte 3 bits 7 and 6.
    let raw = com_primitive_ctrl_data::TxFlag::TxFlagRaw(vec![0, 0, 0x01, 0xC0]);
    assert!(
        env.decide(Request::StartComPrimitive(&start_flags(&data, raw)))
            .is_ok()
    );
    // Omitting any one of the three is refused (the frame would go out in another format).
    for omitted in 0..3 {
        let bits: Vec<TxFlagBit> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != omitted)
            .map(|(_, b)| *b)
            .collect();
        assert!(
            matches!(send_with(&bits), Err(GuardRefusal::Frame(_))),
            "omitted {omitted}"
        );
    }
    assert!(send_with(&[]).is_err());

    // The whole header on T0: the id, then the address byte, then something to send.
    let go = |data: &[u8]| {
        env.decide(Request::StartComPrimitive(&start_flags(
            data,
            flag_bits(&all),
        )))
    };
    assert!(go(&frame(0x18DA_10F1, &[0x42, 0x01])).is_ok());
    assert!(go(&frame(0x18DA_10F1, &[0x43, 0x01])).is_err());
    assert!(go(&frame(0x18DA_10F1, &[])).is_err());
    assert!(go(&[0x18, 0xDA, 0x10]).is_err());
    assert!(go(&frame(0x18DA_F110, &[0x42, 0x01])).is_err());

    // A padded 11-bit T0 profile refuses an omitted padding bit and takes it when present.
    let padded = parse(&format!(
        "class = \"T0\"\n{}{}",
        t0_end("CAN_CH1", 0x123, 0x124, Some(0xAA)),
        t0_end("CAN_CH2", 0x124, 0x123, Some(0xAA))
    ))
    .unwrap();
    let t0 = Env::linked(padded, true);
    let frame = frame(0x123, &[0x01]);
    let t0_send = |bits: &[TxFlagBit]| {
        t0.decide(Request::StartComPrimitive(&start_flags(
            &frame,
            flag_bits(bits),
        )))
    };
    assert!(t0_send(&[TxFlagBit::TxFlagIso15765FramePad]).is_ok());
    assert!(t0_send(&[]).is_err());
}

#[test]
fn the_formats_are_set_before_connect_on_an_11_bit_normal_link() {
    let mut env = Env::linked(
        profile_with(
            "T3",
            &L {
                functional: None,
                ..L::default()
            },
        ),
        false,
    );
    let required = link::required(env.endpoint_link(0), env.profile.class);
    let others: Vec<String> = required
        .iter()
        .filter(|n| !n.ends_with("format"))
        .cloned()
        .collect();
    env.links.get_mut(&CLL).unwrap().set.extend(others);
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(
        why.contains("cp_canphysreqformat") && why.contains("cp_canrespusdtformat"),
        "{why}"
    );
    // A 29-bit format is not the profile's.
    assert!(set(&env, unum("CP_CanPhysReqFormat", 0x07)).is_err());
    assert!(set(&env, unum("CP_CanRespUSDTFormat", 0x06)).is_err());
    let grant = set(&env, unum("CP_CanPhysReqFormat", 0x05));
    apply(&mut env, grant);
    assert!(connect(&env).is_err());
    let grant = set(&env, unum("CP_CanRespUSDTFormat", 0x05));
    apply(&mut env, grant);
    assert_eq!(connect(&env), Ok(Grant::Pass));
    // The extended address of a normal profile is not required.
    assert!(
        !link::required(env.endpoint_link(0), env.profile.class)
            .iter()
            .any(|n| n.ends_with("extaddr"))
    );
}

#[test]
fn the_address_modes_are_set_before_connect() {
    // Without a functional identifier both modes must be physical, and the preset's tester
    // present (functional, to its own identifier) must not be left in place.
    let mut env = Env::linked(
        profile_with(
            "T3",
            &L {
                functional: None,
                ..L::default()
            },
        ),
        false,
    );
    let required = link::required(env.endpoint_link(0), env.profile.class);
    let others: Vec<String> = required
        .iter()
        .filter(|n| !n.ends_with("addrmode"))
        .cloned()
        .collect();
    env.links.get_mut(&CLL).unwrap().set.extend(others);
    let Err(GuardRefusal::LinkSetup(why)) = connect(&env) else {
        panic!("connect should be refused");
    };
    assert!(
        why.contains("cp_requestaddrmode") && why.contains("cp_testerpresentaddrmode"),
        "{why}"
    );
    // Functional is refused, so the preset's cannot be kept by setting it again.
    assert!(set(&env, unum("CP_TesterPresentAddrMode", 1)).is_err());
    assert!(set(&env, unum("CP_RequestAddrMode", 2)).is_err());
    let grant = set(&env, unum("CP_RequestAddrMode", 1));
    apply(&mut env, grant);
    assert!(
        connect(&env).is_err(),
        "the tester present mode is still missing"
    );
    let grant = set(&env, unum("CP_TesterPresentAddrMode", 0));
    apply(&mut env, grant);
    assert_eq!(connect(&env), Ok(Grant::Pass));

    // With a functional identifier either value is allowed, but both are still to be set.
    let mut env = Env::ready(profile("T3"));
    env.links.get_mut(&CLL).unwrap().set.clear();
    let required = link::required(env.endpoint_link(0), env.profile.class);
    let others: Vec<String> = required
        .into_iter()
        .filter(|n| !n.ends_with("addrmode"))
        .collect();
    env.links.get_mut(&CLL).unwrap().set.extend(others);
    assert!(connect(&env).is_err());
    for (name, value) in [("CP_RequestAddrMode", 2), ("CP_TesterPresentAddrMode", 1)] {
        let grant = set(&env, unum(name, value));
        assert!(grant.is_ok(), "{name}");
        apply(&mut env, grant);
    }
    assert_eq!(connect(&env), Ok(Grant::Pass));
}

#[test]
fn frame_padding_flags_follow_the_profiles_padding() {
    let read = [0x22, 0xF1, 0x90];
    let named = || flag_bits(&[TxFlagBit::TxFlagIso15765FramePad]);
    let raw = || com_primitive_ctrl_data::TxFlag::TxFlagRaw(vec![0, 0, 0, 0x40]);
    let unpadded = Env::linked(profile("T3"), false);
    let padded = Env::linked(
        profile_with(
            "T3",
            &L {
                padding: Some(0x55),
                ..L::default()
            },
        ),
        false,
    );
    let send_with = |env: &Env, flag: Option<com_primitive_ctrl_data::TxFlag>| {
        let request = match flag {
            Some(flag) => start_flags(&read, flag),
            None => start(ComOperationType::CoptSendrecv, &read, 1),
        };
        env.decide(Request::StartComPrimitive(&request))
    };
    // Without the flag a read passes; with it, on a profile that does not pad, it is refused.
    assert!(send_with(&unpadded, None).is_ok());
    assert!(matches!(
        send_with(&unpadded, Some(named())),
        Err(GuardRefusal::Frame(_))
    ));
    assert!(matches!(
        send_with(&unpadded, Some(raw())),
        Err(GuardRefusal::Frame(_))
    ));
    // On a padded profile it passes.
    assert!(send_with(&padded, Some(named())).is_ok());
    assert!(send_with(&padded, Some(raw())).is_ok());
    // The other flags that would override the profile, on a link that is not RawMode.
    let wide = flag_bits(&[TxFlagBit::TxFlagCan29bitId]);
    let addressed = flag_bits(&[TxFlagBit::TxFlagIso15765AddrType]);
    assert!(send_with(&unpadded, Some(wide)).is_err());
    assert!(send_with(&unpadded, Some(addressed)).is_err());
    // Every class with a profile link, a receive-only primitive too.
    for class in ["T1", "T2"] {
        let env = Env::linked(profile(class), false);
        assert!(send_with(&env, Some(named())).is_err(), "{class}");
    }
    let mut receive = start_flags(&[], named());
    receive.cop_ctrl_data.as_mut().unwrap().num_send_cycles = 0;
    assert!(
        unpadded
            .decide(Request::StartComPrimitive(&receive))
            .is_err()
    );
}

#[test]
fn unknown_handles_and_other_modules_are_refused() {
    let env = Env::new(profile("T3"));
    let handle = cll();
    assert_eq!(
        env.decide(Request::ConnectComLogicalLink(
            &ConnectComLogicalLinkRequest { cll_handle: handle }
        )),
        Err(GuardRefusal::UnknownHandle)
    );
    assert_eq!(
        env.decide(Request::StartComPrimitive(&start(
            ComOperationType::CoptStartcomm,
            &[],
            0
        ))),
        Err(GuardRefusal::UnknownHandle)
    );
    assert_eq!(
        env.decide(Request::ConnectComLogicalLink(
            &ConnectComLogicalLinkRequest { cll_handle: None }
        )),
        Err(GuardRefusal::UnknownHandle)
    );
    let linked = Env::ready(profile("T3"));
    assert_eq!(
        linked.decide(Request::ConnectComLogicalLink(
            &ConnectComLogicalLinkRequest { cll_handle: handle }
        )),
        Ok(Grant::Pass)
    );
    let other_module = Some(ComLogicalLinkHandle {
        module_handle: MODULE + 1,
        cll_handle: CLL,
    });
    assert_eq!(
        linked.decide(Request::ConnectComLogicalLink(
            &ConnectComLogicalLinkRequest {
                cll_handle: other_module
            }
        )),
        Err(GuardRefusal::Module)
    );
    let mut create = create_request(create::Resource::RscData(profile_resource()));
    create.module_handle = Some(ModuleHandle { module_handle: 2 });
    assert_eq!(
        env.decide(Request::CreateComLogicalLink(&create)),
        Err(GuardRefusal::Module)
    );
}

#[test]
fn the_harness_opens_the_module_not_the_check() {
    let env = Env::new(profile("T3"));
    let connect = vci_service_interface::ModuleConnectRequest {
        module_handle: module(),
    };
    assert!(matches!(
        env.decide(Request::ModuleConnect(&connect)),
        Err(GuardRefusal::LinkSetup(_))
    ));
    let disconnect = vci_service_interface::ModuleDisconnectRequest {
        module_handle: module(),
    };
    assert_eq!(
        env.decide(Request::ModuleDisconnect(&disconnect)),
        Ok(Grant::Pass)
    );
    // Without a handle it would disconnect whatever the worker holds.
    let anonymous = vci_service_interface::ModuleDisconnectRequest {
        module_handle: None,
    };
    assert_eq!(
        env.decide(Request::ModuleDisconnect(&anonymous)),
        Err(GuardRefusal::Module)
    );
}

#[test]
fn destroying_a_link_tells_the_caller_which() {
    let env = Env::linked(profile("T3"), false);
    let destroy = vci_service_interface::DestroyComLogicalLinkRequest { cll_handle: cll() };
    assert_eq!(
        env.decide(Request::DestroyComLogicalLink(&destroy)),
        Ok(Grant::DestroysLink(CLL))
    );
}

// ---------------------------------------------------------------------------------------------
// UDS requests

fn session_grant(class: &str, bytes: &[u8]) -> Result<Grant, GuardRefusal> {
    send(&Env::linked(profile(class), false), bytes, 1)
}

#[test]
fn t2_and_t3_pass_the_read_only_list_and_nothing_else() {
    let passes: &[&[u8]] = &[
        &[0x10, 0x01],
        &[0x10, 0x03],
        &[0x10, 0x83],
        &[0x3E, 0x00],
        &[0x3E, 0x80],
        &[0x22, 0xF1, 0x90],
        &[0x22, 0xF1, 0x90, 0xF1, 0x87],
        &[0x19, 0x02, 0x08],
        &[0x19, 0x0A],
        &[0x19, 0x56, 0x01],
        &[0x19, 0x42, 0x01, 0x02],
    ];
    let refused: &[&[u8]] = &[
        &[0x10, 0x02],
        &[0x10, 0x04],
        &[0x10, 0x40],
        &[0x10, 0x03, 0x00],
        &[0x10],
        &[0x11, 0x01],
        &[0x14, 0xFF, 0xFF, 0xFF],
        &[0x23, 0x12, 0x00, 0x00, 0x10, 0x00, 0x04],
        &[0x24, 0xF1, 0x90],
        &[0x27, 0x01],
        &[0x28, 0x00, 0x01],
        &[0x29, 0x01],
        &[0x2A, 0x01, 0xF1],
        &[0x2C, 0x01, 0xF2, 0x00],
        &[0x2E, 0xF1, 0x90, 0x01],
        &[0x2F, 0xF1, 0x90, 0x03],
        &[0x31, 0x01, 0xFF, 0x00],
        &[0x34, 0x00, 0x44],
        &[0x36, 0x01],
        &[0x3D, 0x00],
        &[0x3E, 0x01],
        &[0x3E],
        &[0x3E, 0x00, 0x00],
        &[0x85, 0x02],
        &[0x86, 0x00],
        &[0x87, 0x01],
        &[0x62, 0xF1, 0x90],
        &[0x7F, 0x22, 0x31],
        &[],
        &[0x19, 0x00],
        &[0x19, 0x7F],
        &[0x19],
        &[0x22],
        &[0x22, 0xF1],
        &[0x22, 0xF1, 0x90, 0xF1],
        &[0x00],
        &[0x01, 0x00],
        &[0x04],
    ];
    for class in ["T2", "T3"] {
        for bytes in passes {
            assert!(
                session_grant(class, bytes).is_ok(),
                "{class} {bytes:02x?} should pass"
            );
        }
        for bytes in refused {
            assert!(
                session_grant(class, bytes).is_err(),
                "{class} {bytes:02x?} should be refused"
            );
        }
    }
}

#[test]
fn a_refused_request_names_the_class_service_and_sub_function() {
    assert_eq!(
        session_grant("T3", &[0x2E, 0xF1, 0x90, 0x01]),
        Err(GuardRefusal::Request {
            class: Class::T3,
            sid: 0x2E,
            sub: Some(0x71)
        })
    );
    assert!(matches!(
        session_grant("T3", &[]),
        Err(GuardRefusal::Undecodable(_))
    ));
}

#[test]
fn a_session_change_is_reported_for_the_return_to_default() {
    assert_eq!(session_grant("T3", &[0x10, 0x01]), Ok(Grant::Pass));
    assert_eq!(
        session_grant("T3", &[0x10, 0x03]),
        Ok(Grant::EntersSession(0x03))
    );
    assert_eq!(
        session_grant("T2", &[0x10, 0x83]),
        Ok(Grant::EntersSession(0x03))
    );
    assert_eq!(
        session_grant("T1", &[0x10, 0x02]),
        Ok(Grant::EntersSession(0x02))
    );
    assert_eq!(session_grant("T3", &[0x22, 0xF1, 0x90]), Ok(Grant::Pass));
}

#[test]
fn t1_passes_every_request_undecoded() {
    for bytes in [
        &[0x2E, 0xF1, 0x90, 0x01][..],
        &[0x31, 0x01, 0xFF, 0x00],
        &[0x11, 0x01],
    ] {
        assert!(session_grant("T1", bytes).is_ok(), "{bytes:02x?}");
    }
    // A raw-bound handle on T1 (never created by the guard) is refused, not passed: its id
    // and its UDS payload would go unseen.
    let env = Env::linked(profile("T1"), true);
    assert!(send(&env, &[0, 0, 0x07, 0xE0, 0x10, 0x03], 1).is_err());
    assert!(send(&env, &[0, 0, 0x01, 0x23, 0x02, 0x2E, 0xF1], 1).is_err());
}

#[test]
fn a_periodic_message_is_judged_like_a_single_request() {
    let env = Env::linked(profile("T3"), false);
    for cycles in [-1, 5] {
        assert!(send(&env, &[0x3E, 0x80], cycles).is_ok(), "{cycles}");
        assert!(
            send(&env, &[0x2E, 0xF1, 0x90, 0x01], cycles).is_err(),
            "{cycles}"
        );
    }
    // A primitive that only receives carries nothing, or a request that is judged anyway.
    assert!(send(&env, &[], 0).is_ok());
    assert!(send(&env, &[0x2E, 0xF1, 0x90, 0x01], 0).is_err());
    assert!(send(&env, &[0x3E, 0x80], 0).is_ok());
    assert!(send(&env, &[0x3E, 0x80], -2).is_err());
    assert!(send(&env, &[], 1).is_err());
    // No control data at all means no sending.
    let mut bare = start(ComOperationType::CoptSendrecv, &[0x2E, 0xF1, 0x90, 0x01], 1);
    bare.cop_ctrl_data = None;
    assert!(env.decide(Request::StartComPrimitive(&bare)).is_err());
}

#[test]
fn communication_primitives_other_than_a_send() {
    let env = Env::linked(profile("T3"), false);
    let run =
        |cop_type, data: &[u8]| env.decide(Request::StartComPrimitive(&start(cop_type, data, 0)));
    assert!(run(ComOperationType::CoptStartcomm, &[]).is_ok());
    assert!(run(ComOperationType::CoptStopcomm, &[]).is_ok());
    assert!(run(ComOperationType::CoptUpdateparam, &[]).is_ok());
    assert!(run(ComOperationType::CoptDelay, &[]).is_ok());
    // A start message is a request like any other.
    assert!(run(ComOperationType::CoptStartcomm, &[0x10, 0x03]).is_ok());
    assert!(run(ComOperationType::CoptStartcomm, &[0x11, 0x01]).is_err());
    assert!(run(ComOperationType::CoptStopcomm, &[0x2E, 0xF1, 0x90, 0x01]).is_err());
    // Restoring ComParams would undo the comparison with the profile, on every class.
    for class in ["T0", "T1", "T2", "T3"] {
        let profile = if class == "T0" {
            loopback()
        } else {
            profile(class)
        };
        let env = Env::linked(profile, false);
        assert!(matches!(
            env.decide(Request::StartComPrimitive(&start(
                ComOperationType::CoptRestoreParam,
                &[],
                0
            ))),
            Err(GuardRefusal::LinkSetup(_))
        ));
        assert!(matches!(
            env.decide(Request::StartComPrimitive(&start(
                ComOperationType::CoptUnspecified,
                &[],
                0
            ))),
            Err(GuardRefusal::Undecodable(_))
        ));
    }
    let mut other = start(ComOperationType::CoptDelay, &[], 0);
    other.cop_type = 0x1234;
    assert!(env.decide(Request::StartComPrimitive(&other)).is_err());
}

// ---------------------------------------------------------------------------------------------
// Raw frames

/// `[id][pci and data]` for the default 11-bit link.
fn frame(id: u32, rest: &[u8]) -> Vec<u8> {
    let mut data = id.to_be_bytes().to_vec();
    data.extend_from_slice(rest);
    data
}

fn raw_send(env: &Env, data: &[u8]) -> Result<Grant, GuardRefusal> {
    send(env, data, 1)
}

#[test]
fn a_raw_mode_link_is_refused_outside_t0() {
    let raw = create::CllCreateFlag::CllCreateFlagBits(vci_service_interface::CllCreateFlag {
        bits: vec![CllCreateFlagBit::CllCreateFlagRawMode as i32],
    });
    let raw_bytes = create::CllCreateFlag::CllCreateFlagRaw(vec![0x80, 0, 0, 0]);
    for class in ["T1", "T2", "T3"] {
        let env = Env::new(profile(class));
        for flag in [raw.clone(), raw_bytes.clone()] {
            let mut request = create_request(create::Resource::RscData(profile_resource()));
            request.cll_create_flag = Some(flag);
            assert!(
                matches!(
                    env.decide(Request::CreateComLogicalLink(&request)),
                    Err(GuardRefusal::LinkSetup(_))
                ),
                "{class}"
            );
        }
        // The same link without RawMode is fine, and a T1 link starts communication.
        assert!(create_with(&env, &profile_resource()).is_ok());
    }
    let t1 = Env::linked(profile("T1"), false);
    assert!(
        t1.decide(Request::StartComPrimitive(&start(
            ComOperationType::CoptStartcomm,
            &[],
            0
        )))
        .is_ok()
    );
    // T0 keeps RawMode.
    let env = Env::new(loopback());
    let mut request = create_request(create::Resource::RscData(channel_resource("CAN_CH1")));
    request.cll_create_flag = Some(raw.clone());
    assert_eq!(
        env.decide(Request::CreateComLogicalLink(&request)),
        Ok(Grant::CreatesLink {
            endpoint: 0,
            raw_mode: true
        })
    );
}

#[test]
fn a_raw_bound_handle_outside_t0_is_refused_not_decoded() {
    // Native ISO 15765 segments raw data itself, so this is RoutineControl on the wire although
    // its first byte after the identifier looks like a flow control PCI.
    let routine = frame(0x7E0, &[0x31, 0x01, 0xFF, 0x00]);
    let read = frame(0x7E0, &[0x03, 0x22, 0xF1, 0x90]);
    for class in ["T1", "T2", "T3"] {
        let env = Env::linked(profile(class), true);
        for data in [&routine, &read] {
            assert!(
                matches!(raw_send(&env, data), Err(GuardRefusal::Frame(_))),
                "{class} {data:02x?}"
            );
        }
        // Receive-only primitives and the ones without a message still pass.
        assert!(send(&env, &[], 0).is_ok());
    }
}

#[test]
fn t0_passes_only_frames_between_its_endpoints() {
    let env = Env::linked(loopback(), true);
    // Endpoint 0 sends to its own request identifier (the other end's response identifier).
    assert!(raw_send(&env, &frame(0x123, &[0x03, 0x2E, 0xF1, 0x90])).is_ok());
    // Content is not read on T0.
    assert!(raw_send(&env, &frame(0x123, &[0xAA])).is_ok());
    assert!(raw_send(&env, &frame(0x124, &[0x03, 0x22, 0xF1, 0x90])).is_err());
    assert!(raw_send(&env, &frame(0x7DF, &[0x03, 0x22, 0xF1, 0x90])).is_err());
    assert!(raw_send(&env, &[0x00, 0x01]).is_err());
    // The other endpoint's link sends to its own.
    let mut other = Env::new(loopback());
    other.links.insert(CLL, BoundLink::new(1, true));
    assert!(raw_send(&other, &frame(0x124, &[0x01, 0x00])).is_ok());
    assert!(raw_send(&other, &frame(0x123, &[0x01, 0x00])).is_err());
    // A frame whose flags disagree with the link.
    let wide_flag = start_flags(
        &frame(0x123, &[0x01]),
        flag_bits(&[TxFlagBit::TxFlagCan29bitId]),
    );
    assert!(env.decide(Request::StartComPrimitive(&wide_flag)).is_err());
    // A request through a link that is not raw is an ECU-addressed request.
    let plain = Env::linked(loopback(), false);
    assert!(matches!(
        send(&plain, &[0x22, 0xF1, 0x90], 1),
        Err(GuardRefusal::Frame(_))
    ));
    assert!(send(&plain, &[], 0).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Hardware controls

#[test]
fn hardware_controls_are_refused_on_every_class() {
    let with_control = parse(&format!(
        "class = \"T1\"\n[vci]\n{VCI}\n[link]\n{}\n\
         [[hardware-control]]\nkind = \"programming-voltage\"\nname = \"pv\"\npin = 12\n\
         millivolts = 5000\n",
        link_toml(&L::default())
    ))
    .unwrap();
    let profiles = [
        ("T0", loopback()),
        ("T1", profile("T1")),
        ("T1 with a control", with_control),
        ("T2", profile("T2")),
        ("T3", profile("T3")),
    ];
    let refused = [
        "PDU_IOCTL_SET_PROG_VOLTAGE",
        "pdu_ioctl_set_prog_voltage",
        "PDU_IOCTL_GENERIC",
        "PDU_IOCTL_SW_CAN_HS",
        "PDU_IOCTL_SW_CAN_NS",
        "PDU_IOCTL_START_REPEAT_MESSAGE",
        "PDU_IOCTL_QUERY_REPEAT_MESSAGE",
        "PDU_IOCTL_STOP_REPEAT_MESSAGE",
        "PDU_IOCTL_SEND_BREAK",
        "PDU_IOCTL_SET_DEVICE_CONFIG",
        "PDU_IOCTL_SET_POLL_RESPONSE",
        "PDU_IOCTL_BECOME_MASTER",
        "PDU_IOCTL_RESET",
        "PDU_IOCTL_READ_PROG_VOLTAGE",
        "PDU_IOCTL_READ_IGNITION_SENSE_STATE",
        "PDU_IOCTL_READ_J1962PIN_VOLTAGE",
        "PDU_IOCTL_GET_CABLE_ID",
        "PDU_IOCTL_GET_DEVICE_CONFIG",
        "PDU_IOCTL_GET_NDIS_ADAPTER_INFO",
        "VENDOR_SPECIFIC_THING",
        "",
    ];
    let passed = [
        "PDU_IOCTL_READ_VBATT",
        "pdu_ioctl_read_vbatt",
        "PDU_IOCTL_CLEAR_RX_QUEUE",
        "PDU_IOCTL_CLEAR_TX_QUEUE",
        "PDU_IOCTL_SUSPEND_TX_QUEUE",
        "PDU_IOCTL_RESUME_TX_QUEUE",
        "PDU_IOCTL_SET_BUFFER_SIZE",
        "PDU_IOCTL_START_MSG_FILTER",
        "PDU_IOCTL_STOP_MSG_FILTER",
        "PDU_IOCTL_CLEAR_MSG_FILTER",
        "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES",
    ];
    for (label, profile) in profiles {
        let mut env = Env::linked(profile, false);
        for name in refused {
            assert!(
                matches!(
                    ioctl_name(&env, name),
                    Err(GuardRefusal::HardwareControl(_))
                ),
                "{label}: {name}"
            );
        }
        for name in passed {
            assert_eq!(ioctl_name(&env, name), Ok(Grant::Pass), "{label}: {name}");
        }
        // By id: unlearned ids are refused, learned ones mean what they were learned as.
        let by_id = |env: &Env, id| ioctl(env, io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id));
        assert_eq!(
            by_id(&env, 0x9001),
            Err(GuardRefusal::UnknownId {
                kind: "IO control",
                id: 0x9001
            })
        );
        env.learned.objects.insert(
            (ObjectType::ObjtIoCtrl, 0x9001),
            "PDU_IOCTL_READ_VBATT".to_owned(),
        );
        env.learned.objects.insert(
            (ObjectType::ObjtIoCtrl, 0x9002),
            "PDU_IOCTL_SET_PROG_VOLTAGE".to_owned(),
        );
        // An id learned as another object type is not an IO control.
        env.learned.objects.insert(
            (ObjectType::ObjtComparam, 0x9003),
            "PDU_IOCTL_READ_VBATT".to_owned(),
        );
        assert_eq!(by_id(&env, 0x9001), Ok(Grant::Pass), "{label}");
        assert!(by_id(&env, 0x9002).is_err(), "{label}");
        assert!(by_id(&env, 0x9003).is_err(), "{label}");
    }
}

#[test]
fn an_io_control_needs_a_command_and_a_known_scope() {
    let env = Env::linked(profile("T3"), false);
    let no_command = IoCtlRequest {
        handle: Some(io_ctl_request::Handle::CllHandle(cll().unwrap())),
        io_ctrl_command: None,
        input_data: None,
        has_output: false,
    };
    assert!(matches!(
        env.decide(Request::IoCtl(&no_command)),
        Err(GuardRefusal::Undecodable(_))
    ));
    let named = |handle| IoCtlRequest {
        handle,
        io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandName(
            "PDU_IOCTL_READ_VBATT".to_owned(),
        )),
        input_data: None,
        has_output: true,
    };
    assert!(env.decide(Request::IoCtl(&named(None))).is_err());
    let system = Some(io_ctl_request::Handle::SystemHandle(
        vci_service_interface::SystemHandle {},
    ));
    assert_eq!(env.decide(Request::IoCtl(&named(system))), Ok(Grant::Pass));
    let module = Some(io_ctl_request::Handle::ModuleHandle(module().unwrap()));
    assert_eq!(env.decide(Request::IoCtl(&named(module))), Ok(Grant::Pass));
    let other = Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
        module_handle: 5,
    }));
    assert_eq!(
        env.decide(Request::IoCtl(&named(other))),
        Err(GuardRefusal::Module)
    );
    let unknown_link = Some(io_ctl_request::Handle::CllHandle(ComLogicalLinkHandle {
        module_handle: MODULE,
        cll_handle: 99,
    }));
    assert_eq!(
        env.decide(Request::IoCtl(&named(unknown_link))),
        Err(GuardRefusal::UnknownHandle)
    );
}

// ---------------------------------------------------------------------------------------------
// Every RPC

/// The RPC names `service VciService` declares, from the proto file the worker interface is
/// generated from.
fn proto_rpcs() -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vci-service-interface/src/proto/service.proto");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let service = text
        .split("service VciService")
        .nth(1)
        .expect("the service");
    service
        .lines()
        .filter_map(|line| line.trim().strip_prefix("rpc "))
        .map(|rest| rest.split('(').next().unwrap().trim().to_owned())
        .collect()
}

#[test]
fn every_rpc_has_a_variant() {
    let mut proto = proto_rpcs();
    let mut variants: Vec<String> = RPC_NAMES.iter().map(|s| (*s).to_owned()).collect();
    proto.sort();
    variants.sort();
    assert_eq!(proto, variants);
}

#[test]
fn every_request_variant_is_decided() {
    use vci_service_interface as v;
    let env = Env::ready(profile("T3"));
    let module_request = v::GetModuleIdsRequest {};
    let connect = v::ModuleConnectRequest {
        module_handle: module(),
    };
    let disconnect = v::ModuleDisconnectRequest {
        module_handle: module(),
    };
    let version = v::GetVersionRequest {
        module_handle: module(),
    };
    let timestamp = v::GetTimestampRequest {
        module_handle: module(),
    };
    let resource_status = v::GetResourceStatusRequest {
        resources: vec![ModuleAndResourceId {
            module_handle: module(),
            resource: None,
        }],
    };
    let resource_ids = v::GetResourceIdsRequest {
        module_handle: module(),
        resource_data: None,
    };
    let conflicting = v::GetConflictingResourcesRequest {
        resource: None,
        input_module_list: None,
    };
    let create_ok = create_request(create::Resource::RscData(profile_resource()));
    let destroy = v::DestroyComLogicalLinkRequest { cll_handle: cll() };
    let connect_link = ConnectComLogicalLinkRequest { cll_handle: cll() };
    let disconnect_link = v::DisconnectComLogicalLinkRequest { cll_handle: cll() };
    let lock = v::LockResourceRequest {
        cll_handle: cll(),
        lock_mask: 0,
    };
    let unlock = v::UnlockResourceRequest {
        cll_handle: cll(),
        lock_mask: 0,
    };
    let get_param = v::GetComParamRequest {
        cll_handle: cll(),
        param: None,
    };
    let set_param = SetComParamRequest {
        cll_handle: cll(),
        param_item: Some(unum("CP_P2Max", 1)),
    };
    let start_request = start(ComOperationType::CoptStartcomm, &[], 0);
    let cancel = CancelComPrimitiveRequest {
        cop_handle: Some(ComPrimitiveHandle {
            module_handle: MODULE,
            cll_handle: CLL,
            cop_handle: 1,
        }),
    };
    let status = GetStatusRequest {
        handle: Some(get_status_request::Handle::CllHandle(cll().unwrap())),
    };
    let event_item = v::GetEventItemRequest {
        handle: Some(v::get_event_item_request::Handle::CllHandle(cll().unwrap())),
    };
    let subscribe = v::SubscribeEventRequest {
        handle: Some(v::subscribe_event_request::Handle::CllHandle(
            cll().unwrap(),
        )),
    };
    let io = IoCtlRequest {
        handle: Some(io_ctl_request::Handle::CllHandle(cll().unwrap())),
        io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandName(
            "PDU_IOCTL_SET_PROG_VOLTAGE".to_owned(),
        )),
        input_data: None,
        has_output: false,
    };
    let object_id = v::GetObjectIdRequest {
        object_type: ObjectType::ObjtComparam as i32,
        shortname: "CP_P2Max".to_owned(),
    };
    let get_table = v::GetUniqueRespIdTableRequest { cll_handle: cll() };
    let set_table_request = SetUniqueRespIdTableRequest {
        cll_handle: cll(),
        unique_resp_id_table: Some(UniqueRespIdTableItem {
            unique_data: vec![EcuUniqueRespData {
                unique_resp_identifier: 1,
                params: vec![unum("CP_CanPhysReqId", 0x7E1)],
            }],
        }),
    };

    // Every variant, with the verdict the class and its state give it.
    let cases: Vec<(Request<'_>, bool)> = vec![
        (Request::GetModuleIds(&module_request), true),
        (Request::ModuleConnect(&connect), false),
        (Request::ModuleDisconnect(&disconnect), true),
        (Request::GetVersion(&version), true),
        (Request::GetTimestamp(&timestamp), true),
        (Request::GetResourceStatus(&resource_status), true),
        (Request::GetResourceIds(&resource_ids), true),
        (Request::GetConflictingResources(&conflicting), true),
        // The linked environment holds the profile's one link already.
        (Request::CreateComLogicalLink(&create_ok), false),
        (Request::DestroyComLogicalLink(&destroy), true),
        (Request::ConnectComLogicalLink(&connect_link), true),
        (Request::DisconnectComLogicalLink(&disconnect_link), true),
        (Request::LockResource(&lock), true),
        (Request::UnlockResource(&unlock), true),
        (Request::GetComParam(&get_param), true),
        (Request::SetComParam(&set_param), true),
        (Request::StartComPrimitive(&start_request), true),
        (Request::CancelComPrimitive(&cancel), true),
        (Request::GetStatus(&status), true),
        (Request::GetEventItem(&event_item), true),
        (Request::SubscribeEvent(&subscribe), true),
        (Request::IoCtl(&io), false),
        (Request::GetObjectId(&object_id), true),
        (Request::GetUniqueRespIdTable(&get_table), true),
        (Request::SetUniqueRespIdTable(&set_table_request), false),
    ];
    let mut seen: Vec<&str> = cases.iter().map(|(r, _)| r.rpc()).collect();
    for (request, allowed) in &cases {
        assert_eq!(
            env.decide(*request).is_ok(),
            *allowed,
            "{}: {:?}",
            request.rpc(),
            env.decide(*request)
        );
    }
    seen.sort_unstable();
    let mut all = RPC_NAMES.to_vec();
    all.sort_unstable();
    assert_eq!(seen, all, "a variant is missing from this test");

    // Without the check's link, every link-scoped request is refused and the rest still pass.
    let bare = Env::new(profile("T3"));
    for (request, _) in &cases {
        let scoped = matches!(
            request,
            Request::DestroyComLogicalLink(_)
                | Request::ConnectComLogicalLink(_)
                | Request::DisconnectComLogicalLink(_)
                | Request::LockResource(_)
                | Request::UnlockResource(_)
                | Request::GetComParam(_)
                | Request::SetComParam(_)
                | Request::StartComPrimitive(_)
                | Request::CancelComPrimitive(_)
                | Request::GetStatus(_)
                | Request::GetEventItem(_)
                | Request::SubscribeEvent(_)
                | Request::GetUniqueRespIdTable(_)
                | Request::SetUniqueRespIdTable(_)
        );
        if scoped {
            assert_eq!(
                bare.decide(*request),
                Err(GuardRefusal::UnknownHandle),
                "{}",
                request.rpc()
            );
        }
    }
}
