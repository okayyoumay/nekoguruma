//! Link setup: which link a check may create and which ComParams it may set on it (design 13.5
//! "Guarded VCI access", first bullet; ADR-333 item 2).
//!
//! The harness creates exactly the links the profile describes. A request that creates another
//! one, or that changes the physical layer, the identifiers or the addressing from what the
//! profile says, is refused. ComParams the profile does not speak about (timing, error handling,
//! tester-present timing) pass.

use vci_service_interface::{
    CllCreateFlagBit, CreateComLogicalLinkRequest, ObjectType, ParamItem, PduParamClass, PinData,
    ResourceData, create_com_logical_link_request as create, param_item, pin_data, resource_data,
};

use super::{Context, Grant, GuardRefusal, uds};
use crate::profile::{Addressing, Class, Link, PHYSICAL_COMPARAMS};

/// The bus type the agent names a CAN link's resource by (ISO 22900-2 (2022) Annex B.1).
const BUS_TYPE: &str = "ISO_11898_2_DWCAN";
/// The DLC pin types of dual-wire CAN (ISO 22900-2 (2022) Annex B.2): the first of the profile's
/// pins is CAN high, the second CAN low.
const PIN_TYPES: [&str; 2] = ["HI", "LOW"];

fn refuse(why: impl Into<String>) -> GuardRefusal {
    GuardRefusal::LinkSetup(why.into())
}

/// `CreateComLogicalLink`: the resource must be one of the profile's endpoints that has no link
/// yet, and the create flag may only ask for RawMode.
pub(super) fn create(
    ctx: &Context<'_>,
    request: &CreateComLogicalLinkRequest,
) -> Result<Grant, GuardRefusal> {
    let resource = match &request.resource {
        Some(create::Resource::RscData(data)) => data.clone(),
        Some(create::Resource::ResourceId(id)) => {
            ctx.learned
                .resources
                .get(id)
                .cloned()
                .ok_or(GuardRefusal::UnknownId {
                    kind: "resource",
                    id: *id,
                })?
        }
        Some(create::Resource::ResourceName(_)) => {
            return Err(refuse(
                "a resource by name cannot be compared with the profile's link",
            ));
        }
        None => return Err(GuardRefusal::Undecodable("a link without a resource")),
    };
    let raw_mode = create_flag(request)?;
    // A worker passes RawMode data through to the device, and a native ISO 15765 channel
    // segments it itself, so the guard cannot tell what a raw payload sends (ADR-333 item 3).
    if raw_mode && matches!(ctx.profile.class, Class::T2 | Class::T3) {
        return Err(refuse(
            "a RawMode link is refused on T2 and T3: the worker passes its data through, so \
             the request it carries cannot be classified",
        ));
    }

    let links = ctx.endpoint_links();
    let mut mismatch = None;
    let mut matched = false;
    for (endpoint, link) in links.iter().enumerate() {
        match resource_matches(ctx, &resource, link) {
            Ok(()) => {
                matched = true;
                if !ctx.links.values().any(|b| b.endpoint == endpoint) {
                    return Ok(Grant::CreatesLink { endpoint, raw_mode });
                }
            }
            Err(error) => {
                mismatch.get_or_insert(error);
            }
        }
    }
    if matched {
        Err(GuardRefusal::LinkCount)
    } else {
        Err(mismatch.unwrap_or(GuardRefusal::LinkCount))
    }
}

/// The create flag: no flag, or RawMode alone (ISO 22900-2 (2022) D.2.3). Returns whether
/// RawMode is set. The checksum mode and any other bit are refused.
fn create_flag(request: &CreateComLogicalLinkRequest) -> Result<bool, GuardRefusal> {
    const RAW_MODE_BYTE0: u8 = 0x80;
    match &request.cll_create_flag {
        None => Ok(false),
        Some(create::CllCreateFlag::CllCreateFlagBits(flag)) => {
            let mut raw = false;
            for &bit in &flag.bits {
                if bit == CllCreateFlagBit::CllCreateFlagRawMode as i32 {
                    raw = true;
                } else {
                    return Err(refuse(format!("create flag bit {bit} is not RawMode")));
                }
            }
            Ok(raw)
        }
        Some(create::CllCreateFlag::CllCreateFlagRaw(bytes)) => {
            let rest_clear = bytes.iter().skip(1).all(|&b| b == 0);
            let first = bytes.first().copied().unwrap_or(0);
            if first & !RAW_MODE_BYTE0 != 0 || !rest_clear {
                return Err(refuse("the create flag sets a bit other than RawMode"));
            }
            Ok(first & RAW_MODE_BYTE0 != 0)
        }
    }
}

/// The name an id stands for, or a refusal when it was not learned.
fn learned_name(
    ctx: &Context<'_>,
    object: ObjectType,
    kind: &'static str,
    id: u32,
) -> Result<String, GuardRefusal> {
    ctx.learned
        .objects
        .get(&(object, id))
        .cloned()
        .ok_or(GuardRefusal::UnknownId { kind, id })
}

/// Whether `resource` is the resource of the profile's `link`: its protocol (ignoring ASCII
/// case), the dual-wire CAN bus type and exactly the two pins, CAN high and CAN low.
fn resource_matches(
    ctx: &Context<'_>,
    resource: &ResourceData,
    link: &Link,
) -> Result<(), GuardRefusal> {
    let protocol = match &resource.protocol {
        Some(resource_data::Protocol::ProtocolName(name)) => name.clone(),
        Some(resource_data::Protocol::ProtocolId(id)) => {
            learned_name(ctx, ObjectType::ObjtProtocol, "protocol", *id)?
        }
        None => return Err(refuse("the resource names no protocol")),
    };
    if !protocol.eq_ignore_ascii_case(&link.protocol) {
        return Err(refuse(format!(
            "protocol {protocol:?} is not the profile's {:?}",
            link.protocol
        )));
    }
    let bus = match &resource.bus_type {
        Some(resource_data::BusType::BusTypeName(name)) => name.clone(),
        Some(resource_data::BusType::BusTypeId(id)) => {
            learned_name(ctx, ObjectType::ObjtBustype, "bus type", *id)?
        }
        None => return Err(refuse("the resource names no bus type")),
    };
    if !bus.eq_ignore_ascii_case(BUS_TYPE) {
        return Err(refuse(format!("bus type {bus:?} is not {BUS_TYPE}")));
    }
    let mut pins = Vec::new();
    for pin in &resource.dlc_pin_data {
        pins.push((pin.dlc_pin_number, pin_type(ctx, pin)?));
    }
    let expected = [
        (u32::from(link.pins[0]), PIN_TYPES[0]),
        (u32::from(link.pins[1]), PIN_TYPES[1]),
    ];
    let ok = pins.len() == 2
        && expected.iter().all(|(number, kind)| {
            pins.iter()
                .any(|(n, k)| n == number && k.eq_ignore_ascii_case(kind))
        });
    if ok {
        Ok(())
    } else {
        Err(refuse(format!(
            "the pins {pins:?} are not CAN high on {} and CAN low on {}",
            link.pins[0], link.pins[1]
        )))
    }
}

fn pin_type(ctx: &Context<'_>, pin: &PinData) -> Result<String, GuardRefusal> {
    match &pin.dlc_pin_type {
        Some(pin_data::DlcPinType::DlcPinTypeName(name)) => Ok(name.clone()),
        Some(pin_data::DlcPinType::DlcPinTypeId(id)) => {
            learned_name(ctx, ObjectType::ObjtPintype, "pin type", *id)
        }
        None => Err(refuse("a pin without a type")),
    }
}

/// The numeric value of a ComParam item, when it has one.
fn numeric(item: &ParamItem) -> Option<i64> {
    match &item.param_data {
        Some(param_item::ParamData::Unum32(v)) => Some(i64::from(*v)),
        Some(param_item::ParamData::Snum32(v)) => Some(i64::from(*v)),
        _ => None,
    }
}

/// Whether a CAN ID format value (ISO 22900-2 (2022) Table B.13, with the combination of bits 6
/// and 3 of Table B.14) describes the profile's identifier width and addressing. Bits 7 and
/// above are undefined and refused. `usdt` says the ComParam is for the segmented transport
/// (bit 2 must be set); the padding overwrite (bits 5 and 4) must not contradict the profile's
/// padding. The flow control bit (0) is not the profile's to decide.
fn format_matches(raw: u32, link: &Link, usdt: bool) -> Result<(), String> {
    if raw & !0x7F != 0 {
        return Err(format!(
            "{raw:#x} sets bits the CAN ID format does not define"
        ));
    }
    let wide = raw & 0x02 != 0;
    if wide != (link.id_bits == 29) {
        return Err(format!(
            "{raw:#x} has a different CAN ID width than the profile's {} bits",
            link.id_bits
        ));
    }
    let (ext, ext2) = (raw & 0x08 != 0, raw & 0x40 != 0);
    let expected = match link.addressing {
        Addressing::Normal => (false, false),
        Addressing::Extended => (true, false),
        Addressing::Mixed => (true, true),
    };
    if (ext, ext2) != expected {
        return Err(format!(
            "{raw:#x} has another addressing scheme than the profile's {:?}",
            link.addressing
        ));
    }
    if usdt && raw & 0x04 == 0 {
        return Err(format!("{raw:#x} is not a segmented (USDT) format"));
    }
    match (raw >> 4) & 0x3 {
        0 => {}
        2 if link.padding.is_none() => {}
        3 if link.padding.is_some() => {}
        _ => return Err(format!("{raw:#x} overrides the profile's padding")),
    }
    Ok(())
}

/// Checks one ComParam a check sets, by its name or by the id the harness learned for it.
pub(super) fn check_param(
    ctx: &Context<'_>,
    link: &Link,
    item: &ParamItem,
) -> Result<(), GuardRefusal> {
    let name = match &item.id {
        Some(param_item::Id::ParamName(name)) => name.clone(),
        Some(param_item::Id::ParamId(id)) => {
            learned_name(ctx, ObjectType::ObjtComparam, "ComParam", *id)?
        }
        None => return Err(GuardRefusal::Undecodable("a ComParam without id or name")),
    };
    let lower = name.to_ascii_lowercase();
    let value = numeric(item);
    let wrong = |what: &str| refuse(format!("{name} {what}"));
    // The value as the profile's number, or a refusal.
    let equals = |expected: u32| match value {
        Some(v) if v == i64::from(expected) => Ok(()),
        Some(v) => Err(wrong(&format!("is {v}, the profile's is {expected}"))),
        None => Err(wrong("has no numeric value")),
    };

    match lower.as_str() {
        "cp_baudrate" => equals(link.baud_rate)?,
        _ if PHYSICAL_COMPARAMS
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&name))
            || item.com_param_class == PduParamClass::PduPcBustype as i32 =>
        {
            return Err(wrong(
                "is a physical-layer ComParam; the profile gives them",
            ));
        }
        "cp_canphysreqid" => equals(link.request_id)?,
        "cp_canrespusdtid" => equals(link.response_id)?,
        "cp_canfuncreqid" => match link.functional_id {
            Some(id) => equals(id)?,
            None => {
                return Err(wrong(
                    "is set, but the profile has no functional identifier",
                ));
            }
        },
        "cp_canrespuudtid" => return Err(wrong("sets an unsegmented response identifier")),
        "cp_canphysreqformat"
        | "cp_canfuncreqformat"
        | "cp_canrespusdtformat"
        | "cp_canrespuudtformat" => {
            let usdt = lower != "cp_canrespuudtformat";
            let raw = value
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| wrong("has no unsigned numeric value"))?;
            format_matches(raw, link, usdt).map_err(|why| wrong(&why))?;
        }
        "cp_canphysreqextaddr"
        | "cp_canfuncreqextaddr"
        | "cp_canrespusdtextaddr"
        | "cp_canrespuudtextaddr" => equals(u32::from(link.address.unwrap_or(0)))?,
        "cp_canfillerbyte" => match (link.padding, value) {
            (Some(padding), _) => equals(u32::from(padding))?,
            (None, Some(v)) if (0..=0xFF).contains(&v) => {}
            _ => return Err(wrong("is not a byte value")),
        },
        "cp_canfillerbytehandling" => equals(u32::from(link.padding.is_some()))?,
        "cp_requestaddrmode" => match value {
            // Physical addressing; the others are functional (ISO 22900-2 (2022) B.3).
            Some(1) => {}
            Some(2..=4) if link.functional_id.is_some() => {}
            Some(2..=4) => {
                return Err(wrong(
                    "asks for functional addressing without a functional id",
                ));
            }
            _ => return Err(wrong("is not a defined addressing mode")),
        },
        "cp_testerpresentaddrmode" => match value {
            Some(0) => {}
            Some(1 | 3 | 4) if link.functional_id.is_some() => {}
            Some(1 | 3 | 4) => {
                return Err(wrong(
                    "asks for functional addressing without a functional id",
                ));
            }
            _ => return Err(wrong("is not a defined addressing mode")),
        },
        "cp_testerpresentmessage" | "cp_testerpresentmsg" => {
            let Some(param_item::ParamData::Bytefield(message)) = &item.param_data else {
                return Err(GuardRefusal::Undecodable(
                    "a tester-present message that is not a byte field",
                ));
            };
            uds::check_tester_present(ctx.profile.class, message)?;
        }
        _ => {}
    }

    // A transport ComParam the profile fixes keeps the profile's value.
    if let Some((_, &expected)) = link
        .comparams
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
    {
        match value {
            Some(v) if v == expected => {}
            Some(v) => {
                return Err(wrong(&format!("is {v}, the profile's is {expected}")));
            }
            None => return Err(wrong("has no numeric value")),
        }
    }
    Ok(())
}
