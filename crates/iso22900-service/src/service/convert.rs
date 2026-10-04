use std::mem::{offset_of, size_of};

use tonic::Status;

use iso22900::{
    BorrowedParamItem, ComParam, ComParamValue, ComPrimitiveControl, E_PDU_CPST, E_PDU_OBJT,
    ExpectedResponse, FlagData as IsoFlagData, ObjectId, ObjectType, ParamClass, ParamDataType,
    ResourceDescriptor, StatusInfo, StructFieldEncoding, T_PDU_PC, UniqueResponseEntry,
};
use iso22900_sys::bindings::{
    PDU_PARAM_STRUCT_ACCESS_TIMING, PDU_PARAM_STRUCT_SESS_TIMING,
    PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER,
};
use vci_service_interface::{
    ComPrimitiveCtrlData, ParamAccessTiming, ParamAccessTimingList, ParamItem, ParamLongfield,
    ParamSessionTiming, ParamSessionTimingList, ParamStructfield, ParamTlsVersionAndCipher,
    ParamTlsVersionAndCipherList, ParamVendorSpecificStruct, PduParamClass, ResourceData,
    StatusResponse, UniqueRespIdTableItem, com_primitive_ctrl_data,
    create_com_logical_link_request, param_item, param_structfield, pin_data, resource_data,
    status_response,
};

use crate::error::map_runtime_error;

/// Vendor `type_url` grammar (ADR-218 Decision item 3): the `ComParamStructType`
/// discriminant, formatted as 8 lowercase hex digits.
const VENDOR_TYPE_URL_PREFIX: &str = "pdu-cpst:0x";

/// Constructs an `E_PDU_CPST` from a raw `ComParamStructType` discriminant.
///
/// `E_PDU_CPST` is a bindgen-generated tuple struct wrapping the C `int`
/// type, but the underlying Rust type bindgen picks for it differs by
/// target: `c_int` (signed) on `x86_64-pc-windows-msvc` alone, `c_uint`
/// (unsigned) on every other pre-committed target -- an MSVC-specific ABI
/// quirk for this particular C enum (see `iso22900-sys/src/bindings/*.rs`).
/// The `as` cast below is a total, bit-pattern-preserving reinterpretation
/// (both types are 32 bits wide and represent the same underlying C value),
/// never a narrowing/fallible conversion. A real vendor-assigned
/// `ComParamStructType` can legitimately have its high bit set
/// (`>= 0x8000_0000`), which would make a checked `u32::try_into::<i32>()`
/// conversion panic on the one target where the field is signed -- do not
/// replace this with `.try_into().unwrap()`.
// `as` is a no-op on targets where `E_PDU_CPST` already wraps `c_uint`, but a
// real (and required) reinterpretation on `x86_64-pc-windows-msvc` where it
// wraps `c_int` -- see the doc comments above. Suppress
// `clippy::unnecessary_cast`, which only sees the former on this
// environment's default target.
#[allow(clippy::unnecessary_cast)]
pub(super) fn cpst_from_raw(raw: u32) -> E_PDU_CPST {
    E_PDU_CPST(raw as _)
}

/// Reads the raw `ComParamStructType` discriminant out of an `E_PDU_CPST`.
/// See [`cpst_from_raw`] for why this is a plain `as` cast rather than a
/// checked conversion.
#[allow(clippy::unnecessary_cast)]
pub(super) fn cpst_raw(struct_type: E_PDU_CPST) -> u32 {
    struct_type.0 as u32
}

/// Returns `true` for the three ISO 22900-2 standard struct types
/// (`PDU_CPST_SESSION_TIMING`/`PDU_CPST_ACCESS_TIMING`/
/// `PDU_CPST_TLS_VERSION_AND_CIPHER`), mapped 1:1 to the proto's typed list
/// variants. Any other `ComParamStructType` is vendor-specific (ADR-218
/// Decision items 2-3).
pub(super) fn is_standard_struct_type(struct_type: E_PDU_CPST) -> bool {
    matches!(
        struct_type,
        E_PDU_CPST::PDU_CPST_SESSION_TIMING
            | E_PDU_CPST::PDU_CPST_ACCESS_TIMING
            | E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER
    )
}

/// Returns the native struct's `size_of` for a standard struct type, or
/// `None` for a vendor struct type (ADR-218 Decision item 2).
fn standard_struct_entry_size(struct_type: E_PDU_CPST) -> Option<u32> {
    match struct_type {
        E_PDU_CPST::PDU_CPST_SESSION_TIMING => {
            Some(size_of::<PDU_PARAM_STRUCT_SESS_TIMING>() as u32)
        }
        E_PDU_CPST::PDU_CPST_ACCESS_TIMING => {
            Some(size_of::<PDU_PARAM_STRUCT_ACCESS_TIMING>() as u32)
        }
        E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER => {
            Some(size_of::<PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER>() as u32)
        }
        _ => None,
    }
}

fn format_vendor_type_url(struct_type: u32) -> String {
    format!("{VENDOR_TYPE_URL_PREFIX}{struct_type:08x}")
}

/// Parses a `ParamVendorSpecificStruct.type_url` per ADR-218 Decision item 3's
/// grammar: `"pdu-cpst:0x<8 lowercase hex digits>"`.
fn parse_vendor_type_url(type_url: &str) -> Result<u32, Status> {
    let invalid = || {
        Status::invalid_argument(format!(
            "vendor_specific.type_url must match \"pdu-cpst:0x<8 lowercase hex digits>\", got {type_url:?}"
        ))
    };
    let hex = type_url
        .strip_prefix(VENDOR_TYPE_URL_PREFIX)
        .ok_or_else(invalid)?;
    if hex.len() != 8
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    u32::from_str_radix(hex, 16).map_err(|_| invalid())
}

fn map_proto_param_class(class: PduParamClass) -> Result<ParamClass, Status> {
    match class {
        PduParamClass::PduPcTiming => Ok(ParamClass::TIMING),
        PduParamClass::PduPcInit => Ok(ParamClass::INIT),
        PduParamClass::PduPcCom => Ok(ParamClass::COM),
        PduParamClass::PduPcErrhdl => Ok(ParamClass::ERRHDL),
        PduParamClass::PduPcBustype => Ok(ParamClass::BUSTYPE),
        PduParamClass::PduPcUniqueId => Ok(ParamClass::UNIQUE_ID),
        PduParamClass::PduPcTesterPresent => Ok(ParamClass::TESTER_PRESENT),
        PduParamClass::PduPcSpecified => Err(Status::invalid_argument(
            "PDU_PC_SPECIFIED is not a concrete class",
        )),
    }
}

fn map_raw_param_class(class: T_PDU_PC) -> Result<ParamClass, Status> {
    let proto = PduParamClass::try_from(class.0 as i32)
        .map_err(|_| Status::internal("received unknown com parameter class from API"))?;
    map_proto_param_class(proto)
}

/// Read the value of a borrowed com-param into a [`ComParamValue`].
/// This removes the inline match duplication from `get_com_param` and `get_unique_resp_id_table`.
///
/// `resolve_vendor_entry_size` implements ADR-218 Decision item 4's (as
/// amended) read-side resolution for a vendor STRUCTFIELD value's
/// `size_of_entry`: per-library operator config, or `failed_precondition`
/// -- config is the sole source, on both the read and write sides, since a
/// process-lifetime cache learned from a client's own write is never more
/// trustworthy than the request it came from (a successful native write
/// never validates the caller's declared size). Never called for a standard
/// struct type (size is always known) or when `ParamActEntries == 0`
/// (Decision item 4d). Both `get_com_param` and `get_unique_resp_id_table`
/// share this same function on read, and `to_iso_param`/
/// `from_unique_response_item` share the identical resolver shape on write,
/// so the resolution algorithm cannot drift between any of these call sites
/// (ADR-218 Decision item 5, extended to the write path).
pub(super) fn read_borrowed_param_value(
    borrowed: &BorrowedParamItem,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<ComParamValue, Status> {
    let value = match borrowed.data_type().0 {
        x if x == ParamDataType::U8.0 => {
            ComParamValue::U8(*borrowed.unum8_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::I8.0 => {
            ComParamValue::I8(*borrowed.snum8_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::U16.0 => {
            ComParamValue::U16(*borrowed.unum16_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::I16.0 => {
            ComParamValue::I16(*borrowed.snum16_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::U32.0 => {
            ComParamValue::U32(*borrowed.unum32_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::I32.0 => {
            ComParamValue::I32(*borrowed.snum32_data().map_err(map_runtime_error)?)
        }
        x if x == ParamDataType::BYTE_FIELD.0 => ComParamValue::Bytes(
            borrowed
                .bytefield_data()
                .map_err(map_runtime_error)?
                .data_array()
                .map_err(map_runtime_error)?
                .to_vec(),
        ),
        x if x == ParamDataType::LONG_FIELD.0 => ComParamValue::Longs(
            borrowed
                .longfield_data()
                .map_err(map_runtime_error)?
                .data_array()
                .map_err(map_runtime_error)?
                .to_vec(),
        ),
        x if x == ParamDataType::STRUCT_FIELD.0 => {
            let data = borrowed.structfield_data().map_err(map_runtime_error)?;
            let struct_type = data.param_struct_type();
            let actual_entries = data.param_actual_entries();

            // ADR-218 Decision item 4d: an empty table needs no size
            // resolution at all, standard or vendor.
            let (entry_size, bytes) = if actual_entries == 0 {
                (0u32, Vec::new())
            } else if let Some(entry_size) = standard_struct_entry_size(struct_type) {
                // Standard struct types are never vendor-specific, so this
                // never needs a caller-supplied entry size -- use the plain,
                // safe accessor instead of the vendor-entry-size one.
                let bytes = data
                    .struct_array_bytes()
                    .map_err(map_runtime_error)?
                    .to_vec();
                (entry_size, bytes)
            } else {
                let entry_size = resolve_vendor_entry_size(struct_type)?;
                // Safety: `entry_size` comes from `resolve_vendor_entry_size`,
                // which resolves solely from operator-configured
                // `vendor_struct_types` config (ADR-218 as amended) -- never
                // from client-controlled input -- so it reflects the
                // connected library's true per-entry byte layout.
                let bytes = unsafe { data.struct_array_bytes_with_entry_size(entry_size as usize) }
                    .map_err(map_runtime_error)?
                    .to_vec();
                (entry_size, bytes)
            };

            ComParamValue::StructField {
                struct_type,
                encoding: StructFieldEncoding {
                    entry_size: entry_size as usize,
                    bytes,
                },
            }
        }
        _ => {
            return Err(Status::unimplemented(
                "unsupported com parameter data type cannot be serialized",
            ));
        }
    };
    Ok(value)
}

/// Read a [`BorrowedParamItem`] into a [`ComParam`]. See
/// [`read_borrowed_param_value`] for `resolve_vendor_entry_size`'s contract.
pub(super) fn borrowed_param_to_comparam(
    borrowed: &BorrowedParamItem,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<ComParam, Status> {
    let value = read_borrowed_param_value(borrowed, resolve_vendor_entry_size)?;
    Ok(ComParam {
        id: ObjectId(borrowed.id()),
        data_type: borrowed.data_type(),
        class: map_raw_param_class(borrowed.class())?,
        value,
    })
}

/// Read a [`BorrowedParamItem`] into its proto [`ParamItem`]. See
/// [`read_borrowed_param_value`] for `resolve_vendor_entry_size`'s contract.
pub(super) fn borrowed_param_to_item(
    borrowed: &BorrowedParamItem,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<ParamItem, Status> {
    from_iso_param(borrowed_param_to_comparam(
        borrowed,
        resolve_vendor_entry_size,
    )?)
}

/// Resolves a [`ParamItem`]'s `id` oneof to an [`ObjectId`]. Name resolution
/// is CLL-independent (it is the caller's `resolve_name` -- typically
/// `PDUGetObjectId(OBJT_COMPARAM, ...)` -- that decides what a name means;
/// this function does not itself vary by CLL/protocol). Whether the
/// resolved id is actually usable on a given CLL is a separate concern,
/// enforced by the underlying D-PDU API call this id is subsequently used in.
pub(super) fn resolve_param_item_id(
    id: Option<param_item::Id>,
    mut resolve_name: impl FnMut(&str) -> Result<ObjectId, Status>,
) -> Result<ObjectId, Status> {
    match id.ok_or_else(|| Status::invalid_argument("param_item.id is required"))? {
        param_item::Id::ParamId(v) => Ok(ObjectId(v)),
        param_item::Id::ParamName(name) => resolve_name(&name),
    }
}

pub(super) fn to_resource_descriptor(
    data: ResourceData,
    mut resolve_object_id: impl FnMut(ObjectType, &str) -> Result<ObjectId, Status>,
) -> Result<ResourceDescriptor, Status> {
    let bus_type_id = match data.bus_type {
        Some(resource_data::BusType::BusTypeId(id)) => id,
        Some(resource_data::BusType::BusTypeName(name)) => {
            resolve_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_BUSTYPE), name.as_str())?.0
        }
        None => {
            return Err(Status::invalid_argument(
                "resource_data.bus_type is required",
            ));
        }
    };

    let protocol_id = match data.protocol {
        Some(resource_data::Protocol::ProtocolId(id)) => id,
        Some(resource_data::Protocol::ProtocolName(name)) => {
            resolve_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_PROTOCOL), name.as_str())?.0
        }
        None => {
            return Err(Status::invalid_argument(
                "resource_data.protocol is required",
            ));
        }
    };

    let pins = data
        .dlc_pin_data
        .into_iter()
        .map(|pin| {
            let pin_type_id = match pin.dlc_pin_type {
                Some(pin_data::DlcPinType::DlcPinTypeId(id)) => id,
                Some(pin_data::DlcPinType::DlcPinTypeName(name)) => {
                    resolve_object_id(ObjectType(E_PDU_OBJT::PDU_OBJT_PINTYPE), name.as_str())?.0
                }
                None => {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data[].dlc_pin_type is required",
                    ));
                }
            };

            Ok(iso22900::PinData {
                dlc_pin_number: pin.dlc_pin_number,
                dlc_pin_type_id: ObjectId(pin_type_id),
            })
        })
        .collect::<Result<Vec<_>, Status>>()?;

    Ok(ResourceDescriptor {
        bus_type_id: ObjectId(bus_type_id),
        protocol_id: ObjectId(protocol_id),
        pins,
    })
}

/// Converts a [`ParamItem`] to a [`ComParam`], using the already-resolved
/// `id` (see [`resolve_param_item_id`] -- callers resolve the `id` oneof
/// once, up front, since some also need the numeric id before this call,
/// e.g. to look up the param's current class).
///
/// `resolve_vendor_entry_size` is the same resolver shape
/// [`read_borrowed_param_value`] uses, applied to a non-empty vendor
/// STRUCTFIELD write (ADR-218 Decision item 3, as amended): the write's
/// declared `size_of_entry` must equal the resolved (operator-configured)
/// entry size, or the write is rejected. Never called for a standard struct
/// type or an empty (`count_of_entry == 0`) vendor write.
pub(super) fn to_iso_param(
    item: ParamItem,
    id: ObjectId,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<ComParam, Status> {
    let class = map_proto_param_class(
        PduParamClass::try_from(item.com_param_class)
            .map_err(|_| Status::invalid_argument("param_item.com_param_class is invalid"))?,
    )?;

    let (data_type, value) = match item.param_data {
        Some(param_item::ParamData::Unum32(v)) => (ParamDataType::U32, ComParamValue::U32(v)),
        Some(param_item::ParamData::Snum32(v)) => (ParamDataType::I32, ComParamValue::I32(v)),
        Some(param_item::ParamData::Bytefield(bytes)) => {
            (ParamDataType::BYTE_FIELD, ComParamValue::Bytes(bytes))
        }
        Some(param_item::ParamData::Longfield(data)) => {
            (ParamDataType::LONG_FIELD, ComParamValue::Longs(data.data))
        }
        Some(param_item::ParamData::Structfield(structfield)) => {
            let (struct_type, encoding) =
                structfield_from_proto(structfield, resolve_vendor_entry_size)?;
            (
                ParamDataType::STRUCT_FIELD,
                ComParamValue::StructField {
                    struct_type,
                    encoding,
                },
            )
        }
        None => {
            return Err(Status::invalid_argument(
                "param_item.param_data must be set",
            ));
        }
    };

    Ok(ComParam {
        id,
        data_type,
        class,
        value,
    })
}

pub(super) fn from_iso_param(param: ComParam) -> Result<ParamItem, Status> {
    let param_data = match param.value {
        ComParamValue::U8(value) => Some(param_item::ParamData::Unum32(value as u32)),
        ComParamValue::I8(value) => Some(param_item::ParamData::Snum32(value as i32)),
        ComParamValue::U16(value) => Some(param_item::ParamData::Unum32(value as u32)),
        ComParamValue::I16(value) => Some(param_item::ParamData::Snum32(value as i32)),
        ComParamValue::U32(value) => Some(param_item::ParamData::Unum32(value)),
        ComParamValue::I32(value) => Some(param_item::ParamData::Snum32(value)),
        ComParamValue::Bytes(bytes) => Some(param_item::ParamData::Bytefield(bytes)),
        ComParamValue::Longs(values) => Some(param_item::ParamData::Longfield(ParamLongfield {
            data: values,
        })),
        ComParamValue::StructField {
            struct_type,
            encoding,
        } => Some(param_item::ParamData::Structfield(structfield_to_proto(
            struct_type,
            encoding,
        ))),
        ComParamValue::Unsupported { .. } => {
            return Err(Status::unimplemented(
                "unsupported com parameter data type cannot be serialized",
            ));
        }
    };

    Ok(ParamItem {
        id: Some(param_item::Id::ParamId(param.id.0)),
        com_param_class: param.class.0.0 as i32,
        param_data,
    })
}

fn u8_field(value: u32, name: &str) -> Result<u8, Status> {
    u8::try_from(value).map_err(|_| Status::invalid_argument(format!("{name} must fit in 8 bits")))
}

fn u16_field(value: u32, name: &str) -> Result<u16, Status> {
    u16::try_from(value)
        .map_err(|_| Status::invalid_argument(format!("{name} must fit in 16 bits")))
}

/// Converts a [`ParamStructfield`] (write direction) into the native
/// `struct_type`/[`StructFieldEncoding`] pair, per ADR-218 Decision items
/// 2-3. See [`vendor_struct_from_proto`] for `resolve_vendor_entry_size`'s
/// contract; it is never consulted for a standard struct type.
fn structfield_from_proto(
    structfield: ParamStructfield,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<(E_PDU_CPST, StructFieldEncoding), Status> {
    match structfield
        .data
        .ok_or_else(|| Status::invalid_argument("param_item.structfield.data must be set"))?
    {
        param_structfield::Data::VendorSpecific(vendor) => {
            vendor_struct_from_proto(vendor, resolve_vendor_entry_size)
        }
        param_structfield::Data::SessionTiming(list) => Ok((
            E_PDU_CPST::PDU_CPST_SESSION_TIMING,
            encode_session_timing(list.entries)?,
        )),
        param_structfield::Data::AccessTiming(list) => Ok((
            E_PDU_CPST::PDU_CPST_ACCESS_TIMING,
            encode_access_timing(list.entries)?,
        )),
        param_structfield::Data::TlsVersionAndCipher(list) => Ok((
            E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER,
            encode_tls_version_and_cipher(list.entries)?,
        )),
    }
}

/// Validates and converts a [`ParamVendorSpecificStruct`] per ADR-218
/// Decision item 3's write-side rules: `type_url` grammar, rejection of a
/// standard struct-type value (`0x1`-`0x3`) under the vendor path, and
/// `value.len() == size_of_entry * count_of_entry` (with `size_of_entry > 0`
/// whenever `count_of_entry > 0`).
///
/// **Entry-size resolution (ADR-218 Decision item 4, as amended).** A
/// successful native `PDUSetComParam` never validates a caller-declared
/// entry size against the connected library's real per-entry layout, so a
/// vendor struct type's entry size is resolved from operator config alone,
/// for both read and write -- there is no write-then-trust shortcut. For a
/// non-empty write (`count_of_entry > 0`), `resolve_vendor_entry_size` is
/// called and its result compared against `size_of_entry`:
/// unconfigured -> its `failed_precondition` propagates unchanged; a
/// configured-but-mismatched value -> `invalid_argument` naming both sizes.
/// An empty write (`count_of_entry == 0`) needs no resolution in either
/// direction and is accepted unconditionally regardless of `size_of_entry`
/// -- this is what closes the empty-write-poison exploit path (a client
/// cannot make an arbitrary `size_of_entry` alongside `count_of_entry: 0`
/// carry any weight, since nothing downstream ever consults it for an empty
/// write).
fn vendor_struct_from_proto(
    vendor: ParamVendorSpecificStruct,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<(E_PDU_CPST, StructFieldEncoding), Status> {
    let struct_type_raw = parse_vendor_type_url(&vendor.type_url)?;
    if is_standard_struct_type(cpst_from_raw(struct_type_raw)) {
        return Err(Status::invalid_argument(
            "vendor_specific.type_url must not name a standard ComParamStructType (0x1-0x3); \
             use the typed session_timing/access_timing/tls_version_and_cipher variant instead",
        ));
    }

    let size_of_entry = vendor.size_of_entry as usize;
    let count_of_entry = vendor.count_of_entry as usize;
    if size_of_entry == 0 && count_of_entry > 0 {
        return Err(Status::invalid_argument(
            "vendor_specific.size_of_entry must be greater than zero when count_of_entry is nonzero",
        ));
    }
    let expected_len = size_of_entry.checked_mul(count_of_entry).ok_or_else(|| {
        Status::invalid_argument("vendor_specific.size_of_entry * count_of_entry overflows")
    })?;
    if vendor.value.len() != expected_len {
        return Err(Status::invalid_argument(format!(
            "vendor_specific.value.len() ({}) must equal size_of_entry * count_of_entry ({})",
            vendor.value.len(),
            expected_len
        )));
    }

    if count_of_entry > 0 {
        let struct_type = cpst_from_raw(struct_type_raw);
        let configured = resolve_vendor_entry_size(struct_type)?;
        if configured != vendor.size_of_entry {
            return Err(Status::invalid_argument(format!(
                "vendor_specific.size_of_entry ({}) does not match the configured entry size \
                 ({configured}) for ComParamStructType 0x{struct_type_raw:08x}",
                vendor.size_of_entry
            )));
        }
    }

    Ok((
        cpst_from_raw(struct_type_raw),
        StructFieldEncoding {
            entry_size: size_of_entry,
            bytes: vendor.value,
        },
    ))
}

fn encode_session_timing(entries: Vec<ParamSessionTiming>) -> Result<StructFieldEncoding, Status> {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_SESS_TIMING>();
    const SESSION_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, session);
    const P2MAX_HIGH_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Max_high);
    const P2MAX_LOW_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Max_low);
    const P2STAR_HIGH_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Star_high);
    const P2STAR_LOW_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Star_low);

    let total_len = ENTRY_SIZE
        .checked_mul(entries.len())
        .ok_or_else(|| Status::invalid_argument("session_timing.entries is too large to encode"))?;
    let mut bytes = vec![0u8; total_len];
    for (i, entry) in entries.iter().enumerate() {
        let base = i * ENTRY_SIZE;
        let session = u16_field(entry.session, "session_timing.session")?;
        let session_bytes = session.to_ne_bytes();
        bytes[base + SESSION_OFFSET] = session_bytes[0];
        bytes[base + SESSION_OFFSET + 1] = session_bytes[1];

        // P2Max/P2Star are each split across two UNUM8 fields, high byte
        // first (ISO 22900-2:2022 §B.3.3.2.2, mirroring the same
        // most-significant-byte-first convention §B.3.3.2.4's NOTE states
        // explicitly for CipherList's UNUM16 encoding).
        let p2_max = u16_field(entry.p2_max, "session_timing.p2_max")?;
        bytes[base + P2MAX_HIGH_OFFSET] = (p2_max >> 8) as u8;
        bytes[base + P2MAX_LOW_OFFSET] = (p2_max & 0xFF) as u8;

        let p2_star = u16_field(entry.p2_star, "session_timing.p2_star")?;
        bytes[base + P2STAR_HIGH_OFFSET] = (p2_star >> 8) as u8;
        bytes[base + P2STAR_LOW_OFFSET] = (p2_star & 0xFF) as u8;
    }
    Ok(StructFieldEncoding {
        entry_size: ENTRY_SIZE,
        bytes,
    })
}

fn encode_access_timing(entries: Vec<ParamAccessTiming>) -> Result<StructFieldEncoding, Status> {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_ACCESS_TIMING>();
    const P2MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P2Min);
    const P2MAX_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P2Max);
    const P3MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P3Min);
    const P3MAX_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P3Max);
    const P4MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P4Min);
    const TIMING_SET_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, TimingSet);

    let total_len = ENTRY_SIZE
        .checked_mul(entries.len())
        .ok_or_else(|| Status::invalid_argument("access_timing.entries is too large to encode"))?;
    let mut bytes = vec![0u8; total_len];
    for (i, entry) in entries.iter().enumerate() {
        let base = i * ENTRY_SIZE;
        bytes[base + P2MIN_OFFSET] = u8_field(entry.p2_min, "access_timing.p2_min")?;
        bytes[base + P2MAX_OFFSET] = u8_field(entry.p2_max, "access_timing.p2_max")?;
        bytes[base + P3MIN_OFFSET] = u8_field(entry.p3_min, "access_timing.p3_min")?;
        bytes[base + P3MAX_OFFSET] = u8_field(entry.p3_max, "access_timing.p3_max")?;
        bytes[base + P4MIN_OFFSET] = u8_field(entry.p4_min, "access_timing.p4_min")?;
        bytes[base + TIMING_SET_OFFSET] = u8_field(entry.timing_set, "access_timing.timing_set")?;
    }
    Ok(StructFieldEncoding {
        entry_size: ENTRY_SIZE,
        bytes,
    })
}

const MAX_TLS_CIPHERS: usize = 5;

fn encode_tls_version_and_cipher(
    entries: Vec<ParamTlsVersionAndCipher>,
) -> Result<StructFieldEncoding, Status> {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER>();
    const MAJOR_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, TlsMajorVersion);
    const MINOR_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, TlsMinorVersion);
    const ACT_ENTRIES_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, CipherActEntries);
    const CIPHER_LIST_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, CipherList);

    let total_len = ENTRY_SIZE.checked_mul(entries.len()).ok_or_else(|| {
        Status::invalid_argument("tls_version_and_cipher.entries is too large to encode")
    })?;
    let mut bytes = vec![0u8; total_len];
    for (i, entry) in entries.iter().enumerate() {
        let base = i * ENTRY_SIZE;
        bytes[base + MAJOR_OFFSET] = u8_field(
            entry.tls_major_version,
            "tls_version_and_cipher.tls_major_version",
        )?;
        bytes[base + MINOR_OFFSET] = u8_field(
            entry.tls_minor_version,
            "tls_version_and_cipher.tls_minor_version",
        )?;
        if entry.cipher_list.len() > MAX_TLS_CIPHERS {
            return Err(Status::invalid_argument(format!(
                "tls_version_and_cipher.cipher_list supports at most {MAX_TLS_CIPHERS} entries, got {}",
                entry.cipher_list.len()
            )));
        }
        bytes[base + ACT_ENTRIES_OFFSET] = entry.cipher_list.len() as u8;
        for (j, &cipher) in entry.cipher_list.iter().enumerate() {
            let cipher = u16_field(cipher, "tls_version_and_cipher.cipher_list")?;
            let cipher_bytes = cipher.to_ne_bytes();
            let offset = base + CIPHER_LIST_OFFSET + j * size_of::<u16>();
            bytes[offset] = cipher_bytes[0];
            bytes[offset + 1] = cipher_bytes[1];
        }
    }
    Ok(StructFieldEncoding {
        entry_size: ENTRY_SIZE,
        bytes,
    })
}

/// Converts a native `struct_type`/[`StructFieldEncoding`] pair (read
/// direction) into a [`ParamStructfield`], per ADR-218 Decision items 2-3.
/// Infallible: `encoding.bytes` always originates from a successful native
/// read (or the empty-table short-circuit), never from unvalidated client
/// input.
fn structfield_to_proto(
    struct_type: E_PDU_CPST,
    encoding: StructFieldEncoding,
) -> ParamStructfield {
    let data = match struct_type {
        E_PDU_CPST::PDU_CPST_SESSION_TIMING => {
            param_structfield::Data::SessionTiming(decode_session_timing(&encoding.bytes))
        }
        E_PDU_CPST::PDU_CPST_ACCESS_TIMING => {
            param_structfield::Data::AccessTiming(decode_access_timing(&encoding.bytes))
        }
        E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER => {
            param_structfield::Data::TlsVersionAndCipher(decode_tls_version_and_cipher(
                &encoding.bytes,
            ))
        }
        other => param_structfield::Data::VendorSpecific(ParamVendorSpecificStruct {
            type_url: format_vendor_type_url(cpst_raw(other)),
            size_of_entry: encoding.entry_size as u32,
            count_of_entry: encoding
                .bytes
                .len()
                .checked_div(encoding.entry_size)
                .unwrap_or(0) as u32,
            value: encoding.bytes,
        }),
    };
    ParamStructfield { data: Some(data) }
}

fn decode_session_timing(bytes: &[u8]) -> ParamSessionTimingList {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_SESS_TIMING>();
    const SESSION_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, session);
    const P2MAX_HIGH_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Max_high);
    const P2MAX_LOW_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Max_low);
    const P2STAR_HIGH_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Star_high);
    const P2STAR_LOW_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_SESS_TIMING, P2Star_low);

    let entries = bytes
        .as_chunks::<ENTRY_SIZE>()
        .0
        .iter()
        .map(|chunk| {
            let session = u16::from_ne_bytes([chunk[SESSION_OFFSET], chunk[SESSION_OFFSET + 1]]);
            let p2_max =
                (u32::from(chunk[P2MAX_HIGH_OFFSET]) << 8) | u32::from(chunk[P2MAX_LOW_OFFSET]);
            let p2_star =
                (u32::from(chunk[P2STAR_HIGH_OFFSET]) << 8) | u32::from(chunk[P2STAR_LOW_OFFSET]);
            ParamSessionTiming {
                session: session as u32,
                p2_max,
                p2_star,
            }
        })
        .collect();
    ParamSessionTimingList { entries }
}

fn decode_access_timing(bytes: &[u8]) -> ParamAccessTimingList {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_ACCESS_TIMING>();
    const P2MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P2Min);
    const P2MAX_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P2Max);
    const P3MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P3Min);
    const P3MAX_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P3Max);
    const P4MIN_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, P4Min);
    const TIMING_SET_OFFSET: usize = offset_of!(PDU_PARAM_STRUCT_ACCESS_TIMING, TimingSet);

    let entries = bytes
        .as_chunks::<ENTRY_SIZE>()
        .0
        .iter()
        .map(|chunk| ParamAccessTiming {
            p2_min: u32::from(chunk[P2MIN_OFFSET]),
            p2_max: u32::from(chunk[P2MAX_OFFSET]),
            p3_min: u32::from(chunk[P3MIN_OFFSET]),
            p3_max: u32::from(chunk[P3MAX_OFFSET]),
            p4_min: u32::from(chunk[P4MIN_OFFSET]),
            timing_set: u32::from(chunk[TIMING_SET_OFFSET]),
        })
        .collect();
    ParamAccessTimingList { entries }
}

fn decode_tls_version_and_cipher(bytes: &[u8]) -> ParamTlsVersionAndCipherList {
    const ENTRY_SIZE: usize = size_of::<PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER>();
    const MAJOR_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, TlsMajorVersion);
    const MINOR_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, TlsMinorVersion);
    const ACT_ENTRIES_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, CipherActEntries);
    const CIPHER_LIST_OFFSET: usize =
        offset_of!(PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, CipherList);

    let entries = bytes
        .as_chunks::<ENTRY_SIZE>()
        .0
        .iter()
        .map(|chunk| {
            let act_entries = (chunk[ACT_ENTRIES_OFFSET] as usize).min(MAX_TLS_CIPHERS);
            let cipher_list = (0..act_entries)
                .map(|j| {
                    let offset = CIPHER_LIST_OFFSET + j * size_of::<u16>();
                    u32::from(u16::from_ne_bytes([chunk[offset], chunk[offset + 1]]))
                })
                .collect();
            ParamTlsVersionAndCipher {
                tls_major_version: u32::from(chunk[MAJOR_OFFSET]),
                tls_minor_version: u32::from(chunk[MINOR_OFFSET]),
                cipher_list,
            }
        })
        .collect();
    ParamTlsVersionAndCipherList { entries }
}

fn flag_proto_bits_to_bytes(bits: &[i32]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    for &v in bits {
        if v <= 0 {
            continue; // 0 = UNSPECIFIED, negative values are invalid
        }
        let pos = (v as u32) - 1;
        let byte_idx = (pos / 8) as usize;
        let bit_in_byte = pos % 8;
        if byte_idx >= buf.len() {
            buf.resize(byte_idx + 1, 0);
        }
        buf[byte_idx] |= 1u8 << bit_in_byte;
    }
    buf
}

pub(super) fn tx_flag_to_iso(flag: Option<com_primitive_ctrl_data::TxFlag>) -> IsoFlagData {
    IsoFlagData {
        bytes: match flag {
            None => vec![],
            Some(com_primitive_ctrl_data::TxFlag::TxFlagBits(f)) => {
                flag_proto_bits_to_bytes(&f.bits)
            }
            Some(com_primitive_ctrl_data::TxFlag::TxFlagRaw(raw)) => raw,
        },
    }
}

pub(super) fn cll_create_flag_to_iso(
    flag: Option<create_com_logical_link_request::CllCreateFlag>,
) -> IsoFlagData {
    IsoFlagData {
        bytes: match flag {
            None => vec![],
            Some(create_com_logical_link_request::CllCreateFlag::CllCreateFlagBits(f)) => {
                flag_proto_bits_to_bytes(&f.bits)
            }
            Some(create_com_logical_link_request::CllCreateFlag::CllCreateFlagRaw(raw)) => raw,
        },
    }
}

pub(super) fn to_iso_control(control: ComPrimitiveCtrlData) -> ComPrimitiveControl {
    ComPrimitiveControl {
        time: control.time,
        send_cycles: control.num_send_cycles,
        receive_cycles: control.num_receive_cycles,
        temp_param_update: control.temp_param_update,
        tx_flags: tx_flag_to_iso(control.tx_flag),
        expected_responses: control
            .expected_response_array
            .into_iter()
            .map(|entry| ExpectedResponse {
                response_type: entry.response_type,
                acceptance_id: entry.acceptance_id,
                mask: entry.mask_data,
                pattern: entry.pattern_data,
                unique_response_ids: entry.unique_resp_ids,
            })
            .collect(),
    }
}

pub(super) fn to_status_response(
    data: StatusInfo,
    target: status_response::Status,
) -> StatusResponse {
    StatusResponse {
        status: Some(target),
        timestamp: data.timestamp,
        extra_info: data.extra_info,
    }
}

/// See [`to_iso_param`] for `resolve_vendor_entry_size`'s contract, shared
/// unchanged across every param in every entry of `table`.
pub(super) fn from_unique_response_item(
    table: UniqueRespIdTableItem,
    mut resolve_name: impl FnMut(&str) -> Result<ObjectId, Status>,
    resolve_vendor_entry_size: &mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>,
) -> Result<Vec<UniqueResponseEntry>, Status> {
    table
        .unique_data
        .into_iter()
        .map(|entry| {
            let params = entry
                .params
                .into_iter()
                .map(|item| {
                    let id = resolve_param_item_id(item.id.clone(), &mut resolve_name)?;
                    to_iso_param(item, id, resolve_vendor_entry_size)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(UniqueResponseEntry {
                unique_response_id: entry.unique_resp_identifier,
                params,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vendor-assigned `ComParamStructType` may legitimately have its high
    /// bit set (this is normal for a 32-bit vendor-assigned id, not a
    /// malformed value). `cpst_from_raw`/`cpst_raw` must round-trip such a
    /// value bit-for-bit on every target -- in particular on
    /// `x86_64-pc-windows-msvc`, where `E_PDU_CPST` wraps a signed `c_int`
    /// and a checked `u32::try_into::<i32>()` would panic for any value
    /// `>= 0x8000_0000`. This test only runs on the default
    /// `x86_64-unknown-linux-gnu` target (where `E_PDU_CPST` is unsigned),
    /// but the `as` cast it exercises is the same total, bit-preserving cast
    /// used on every target -- see `cpst_from_raw`'s doc comment.
    #[test]
    fn cpst_raw_round_trips_high_bit_set_value() {
        let raw = 0x8000_0001u32;
        let struct_type = cpst_from_raw(raw);
        assert_eq!(cpst_raw(struct_type), raw);
    }

    #[test]
    fn flag_bits_single_bit_1() {
        let out = flag_proto_bits_to_bytes(&[1]);
        assert_eq!(out, vec![0x01]);
    }

    #[test]
    fn flag_bits_single_bit_8() {
        let out = flag_proto_bits_to_bytes(&[8]);
        assert_eq!(out, vec![0x80]);
    }

    #[test]
    fn flag_bits_crosses_byte_boundary() {
        // bit 9 is the first bit of the second byte
        let out = flag_proto_bits_to_bytes(&[9]);
        assert_eq!(out, vec![0x00, 0x01]);
    }

    #[test]
    fn flag_bits_skips_zero_and_negative() {
        let out = flag_proto_bits_to_bytes(&[0, -1, 1]);
        assert_eq!(out, vec![0x01]);
    }

    #[test]
    fn resolve_param_item_id_uses_param_id_directly() {
        let id = resolve_param_item_id(Some(param_item::Id::ParamId(42)), |_| {
            panic!("resolver should not be called for a numeric id")
        });
        assert_eq!(id.unwrap(), ObjectId(42));
    }

    #[test]
    fn resolve_param_item_id_calls_resolver_for_name() {
        let id = resolve_param_item_id(Some(param_item::Id::ParamName("cp_foo".into())), |name| {
            assert_eq!(name, "cp_foo");
            Ok(ObjectId(99))
        });
        assert_eq!(id.unwrap(), ObjectId(99));
    }

    #[test]
    fn resolve_param_item_id_rejects_missing_id() {
        let result = resolve_param_item_id(None, |_| panic!("resolver should not be called"));
        assert!(result.is_err());
    }

    // ── ADR-218: STRUCTFIELD ComParam support ──────────────────────────────

    use iso22900_sys::bindings::{
        E_PDU_IT, E_PDU_PC, E_PDU_PT, PDU_PARAM_ITEM, PDU_PARAM_STRUCTFIELD_DATA,
    };

    /// Returns the raw native-layout bytes of `value`, mirroring what a real
    /// `PDUGetComParam`/`PDUSetComParam` call would read/write for a standard
    /// struct type -- used so tests never have to hand-compute field offsets
    /// or byte order themselves.
    fn native_bytes<T: Copy>(value: &T) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
            .to_vec()
    }

    /// Builds a [`BorrowedParamItem`] wrapping a STRUCTFIELD payload backed
    /// by `data`. `data` must outlive the returned item.
    fn borrowed_structfield_item(
        struct_type: E_PDU_CPST,
        act_entries: u32,
        data: &mut [u8],
    ) -> (BorrowedParamItem, PDU_PARAM_STRUCTFIELD_DATA) {
        let mut meta = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: struct_type,
            ParamMaxEntries: act_entries,
            ParamActEntries: act_entries,
            pStructArray: if data.is_empty() {
                std::ptr::null_mut()
            } else {
                data.as_mut_ptr().cast()
            },
        };
        let raw = PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 1,
            ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
            ComParamClass: E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut meta as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
        };
        (BorrowedParamItem(raw), meta)
    }

    fn vendor_struct_type(raw: u32) -> E_PDU_CPST {
        cpst_from_raw(raw)
    }

    // -- Standard struct type round-trip (Decision item 2) --

    #[test]
    fn session_timing_round_trips_through_convert_and_native_bytes() {
        let native = PDU_PARAM_STRUCT_SESS_TIMING {
            session: 7,
            P2Max_high: 0x01,
            P2Max_low: 0x2c,
            P2Star_high: 0x00,
            P2Star_low: 0x32,
        };
        let expected_bytes = native_bytes(&native);
        let expected_proto = ParamSessionTiming {
            session: 7,
            p2_max: 0x012c,
            p2_star: 0x32,
        };

        // Write: proto -> native bytes.
        let (struct_type, encoding) = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::SessionTiming(
                    ParamSessionTimingList {
                        entries: vec![expected_proto],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called for a standard struct type"),
        )
        .expect("session_timing should encode");
        assert_eq!(struct_type, E_PDU_CPST::PDU_CPST_SESSION_TIMING);
        assert_eq!(
            encoding.entry_size,
            size_of::<PDU_PARAM_STRUCT_SESS_TIMING>()
        );
        assert_eq!(encoding.bytes, expected_bytes);

        // Read: native bytes -> proto, via the same struct_type/encoding pair
        // a real `PDUGetComParam` payload would produce.
        let structfield = structfield_to_proto(struct_type, encoding);
        match structfield.data {
            Some(param_structfield::Data::SessionTiming(list)) => {
                assert_eq!(list.entries, vec![expected_proto]);
            }
            other => panic!("expected session_timing, got {other:?}"),
        }
    }

    #[test]
    fn access_timing_round_trips_through_convert_and_native_bytes() {
        let native = PDU_PARAM_STRUCT_ACCESS_TIMING {
            P2Min: 1,
            P2Max: 2,
            P3Min: 3,
            P3Max: 4,
            P4Min: 5,
            TimingSet: 1,
        };
        let expected_bytes = native_bytes(&native);
        let expected_proto = ParamAccessTiming {
            p2_min: 1,
            p2_max: 2,
            p3_min: 3,
            p3_max: 4,
            p4_min: 5,
            timing_set: 1,
        };

        let (struct_type, encoding) = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::AccessTiming(
                    ParamAccessTimingList {
                        entries: vec![expected_proto],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called for a standard struct type"),
        )
        .expect("access_timing should encode");
        assert_eq!(struct_type, E_PDU_CPST::PDU_CPST_ACCESS_TIMING);
        assert_eq!(encoding.bytes, expected_bytes);

        let structfield = structfield_to_proto(struct_type, encoding);
        match structfield.data {
            Some(param_structfield::Data::AccessTiming(list)) => {
                assert_eq!(list.entries, vec![expected_proto]);
            }
            other => panic!("expected access_timing, got {other:?}"),
        }
    }

    #[test]
    fn tls_version_and_cipher_round_trips_through_convert_and_native_bytes() {
        // Unlike SESS_TIMING/ACCESS_TIMING, this native struct has a padding
        // byte between `CipherActEntries` and `CipherList` (offsets 3-3
        // inclusive) whose value is compiler-dependent, not
        // deterministically zero -- so this test checks `entry_size` and the
        // read/write round trip through the typed proto shape, rather than
        // comparing raw bytes against a hand-built native struct literal (see
        // `session_timing`/`access_timing`'s sibling tests for that check on
        // the two padding-free standard structs).
        let expected_proto = ParamTlsVersionAndCipher {
            tls_major_version: 3,
            tls_minor_version: 3,
            cipher_list: vec![0xC02B, 0xC02F],
        };

        let (struct_type, encoding) = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::TlsVersionAndCipher(
                    ParamTlsVersionAndCipherList {
                        entries: vec![expected_proto.clone()],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called for a standard struct type"),
        )
        .expect("tls_version_and_cipher should encode");
        assert_eq!(struct_type, E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER);
        assert_eq!(
            encoding.entry_size,
            size_of::<PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER>()
        );

        let structfield = structfield_to_proto(struct_type, encoding);
        match structfield.data {
            Some(param_structfield::Data::TlsVersionAndCipher(list)) => {
                assert_eq!(list.entries, vec![expected_proto]);
            }
            other => panic!("expected tls_version_and_cipher, got {other:?}"),
        }
    }

    // -- Vendor struct type: write-side validation (Decision item 3) --

    #[test]
    fn vendor_struct_rejects_malformed_type_url_grammar() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "not-a-type-url".to_string(),
                        size_of_entry: 1,
                        count_of_entry: 1,
                        value: vec![0],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called before type_url parses"),
        )
        .expect_err("malformed type_url grammar should be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn vendor_struct_rejects_standard_struct_type_under_vendor_path() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "pdu-cpst:0x00000001".to_string(),
                        size_of_entry: 6,
                        count_of_entry: 1,
                        value: vec![0; 6],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called for a standard struct type value"),
        )
        .expect_err("a standard ComParamStructType under the vendor path should be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn vendor_struct_rejects_value_length_mismatch() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "pdu-cpst:0x80000001".to_string(),
                        size_of_entry: 2,
                        count_of_entry: 3,
                        value: vec![0; 5], // should be 2 * 3 = 6
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called before the length check"),
        )
        .expect_err("value.len() != size_of_entry * count_of_entry should be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn vendor_struct_rejects_zero_size_of_entry_with_nonzero_count() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "pdu-cpst:0x80000001".to_string(),
                        size_of_entry: 0,
                        count_of_entry: 2,
                        value: vec![],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called before the zero-size check"),
        )
        .expect_err("size_of_entry == 0 with count_of_entry > 0 should be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn vendor_struct_round_trips_type_url_and_value_verbatim() {
        let vendor = ParamVendorSpecificStruct {
            type_url: "pdu-cpst:0x80000001".to_string(),
            size_of_entry: 2,
            count_of_entry: 3,
            value: vec![1, 2, 3, 4, 5, 6],
        };
        let (struct_type, encoding) = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(vendor.clone())),
            },
            &mut |struct_type| {
                assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
                Ok(2)
            },
        )
        .expect("well-formed vendor structfield matching the configured size should encode");
        assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
        assert_eq!(encoding.entry_size, 2);
        assert_eq!(encoding.bytes, vendor.value);

        let structfield = structfield_to_proto(struct_type, encoding);
        match structfield.data {
            Some(param_structfield::Data::VendorSpecific(round_tripped)) => {
                assert_eq!(round_tripped, vendor);
            }
            other => panic!("expected vendor_specific, got {other:?}"),
        }
    }

    // -- Vendor struct type: write-side entry-size resolution (Decision item
    // 4, as amended -- config is the sole source, for reads and writes) --

    #[test]
    fn vendor_struct_write_propagates_resolver_failed_precondition_when_unconfigured() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "pdu-cpst:0x80000001".to_string(),
                        size_of_entry: 2,
                        count_of_entry: 2,
                        value: vec![1, 2, 3, 4],
                    },
                )),
            },
            &mut |_| {
                Err(Status::failed_precondition(
                    "vendor ComParamStructType 0x80000001 has no known entry size",
                ))
            },
        )
        .expect_err("an unconfigured vendor struct type write should fail loudly");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn vendor_struct_write_rejects_size_of_entry_mismatched_with_configured_value() {
        let err = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        type_url: "pdu-cpst:0x80000001".to_string(),
                        size_of_entry: 2,
                        count_of_entry: 2,
                        value: vec![1, 2, 3, 4],
                    },
                )),
            },
            &mut |_| Ok(4),
        )
        .expect_err(
            "a declared size_of_entry that does not match the configured value should be rejected",
        );
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains('4'),
            "error message should name the configured size: {}",
            err.message()
        );
    }

    #[test]
    fn vendor_struct_empty_write_never_calls_resolver() {
        let (struct_type, encoding) = structfield_from_proto(
            ParamStructfield {
                data: Some(param_structfield::Data::VendorSpecific(
                    ParamVendorSpecificStruct {
                        // The empty-write-poison shape: an arbitrary,
                        // untrustworthy size_of_entry alongside
                        // count_of_entry: 0 must carry no weight at all.
                        type_url: "pdu-cpst:0x80000001".to_string(),
                        size_of_entry: 0xFFFF_FFFF,
                        count_of_entry: 0,
                        value: vec![],
                    },
                )),
            },
            &mut |_| panic!("resolver should not be called for an empty (count_of_entry: 0) write"),
        )
        .expect("an empty vendor structfield write needs no size resolution in either direction");
        assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
        assert_eq!(encoding.entry_size, 0xFFFF_FFFF);
        assert!(encoding.bytes.is_empty());
    }

    // -- Read-side entry-size resolution (Decision item 4) --

    #[test]
    fn read_standard_struct_type_never_calls_vendor_resolver() {
        let native = PDU_PARAM_STRUCT_ACCESS_TIMING {
            P2Min: 1,
            P2Max: 2,
            P3Min: 3,
            P3Max: 4,
            P4Min: 5,
            TimingSet: 1,
        };
        let mut bytes = native_bytes(&native);
        let (borrowed, _meta) =
            borrowed_structfield_item(E_PDU_CPST::PDU_CPST_ACCESS_TIMING, 1, &mut bytes);

        let value = read_borrowed_param_value(&borrowed, &mut |_| {
            panic!("resolver should not be called for a standard struct type")
        })
        .expect("standard struct type read should succeed without a resolver");

        match value {
            ComParamValue::StructField {
                struct_type,
                encoding,
            } => {
                assert_eq!(struct_type, E_PDU_CPST::PDU_CPST_ACCESS_TIMING);
                assert_eq!(
                    encoding.entry_size,
                    size_of::<PDU_PARAM_STRUCT_ACCESS_TIMING>()
                );
                assert_eq!(encoding.bytes, native_bytes(&native));
            }
            other => panic!("expected StructField, got {other:?}"),
        }
    }

    #[test]
    fn read_empty_vendor_struct_table_never_calls_resolver() {
        let (borrowed, _meta) =
            borrowed_structfield_item(vendor_struct_type(0x8000_0001), 0, &mut []);

        let value = read_borrowed_param_value(&borrowed, &mut |_| {
            panic!("resolver should not be called when ParamActEntries == 0")
        })
        .expect("an empty vendor struct table should not require size resolution");

        match value {
            ComParamValue::StructField {
                struct_type,
                encoding,
            } => {
                assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
                assert_eq!(encoding.entry_size, 0);
                assert!(encoding.bytes.is_empty());
            }
            other => panic!("expected StructField, got {other:?}"),
        }
    }

    #[test]
    fn read_vendor_struct_type_uses_resolver_result() {
        let mut bytes = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let (borrowed, _meta) =
            borrowed_structfield_item(vendor_struct_type(0x8000_0001), 2, &mut bytes);

        let value = read_borrowed_param_value(&borrowed, &mut |struct_type| {
            assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
            Ok(2)
        })
        .expect("vendor struct type read should succeed once the resolver answers");

        match value {
            ComParamValue::StructField {
                struct_type,
                encoding,
            } => {
                assert_eq!(struct_type, vendor_struct_type(0x8000_0001));
                assert_eq!(encoding.entry_size, 2);
                assert_eq!(encoding.bytes, vec![0xAA, 0xBB, 0xCC, 0xDD]);
            }
            other => panic!("expected StructField, got {other:?}"),
        }
    }

    #[test]
    fn read_vendor_struct_type_propagates_resolver_failed_precondition() {
        let mut bytes = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let (borrowed, _meta) =
            borrowed_structfield_item(vendor_struct_type(0x8000_0001), 2, &mut bytes);

        let err = read_borrowed_param_value(&borrowed, &mut |_| {
            Err(Status::failed_precondition(
                "vendor ComParamStructType 0x80000001 has no known entry size",
            ))
        })
        .expect_err("an unresolvable vendor struct type should fail loudly, not fabricate a size");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
