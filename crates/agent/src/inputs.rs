//! What an interrupted-write restart reads from the vehicle (design 8.2.5, 8.9; ADR-229,
//! ADR-245 item 2).
//!
//! A procedure declares a [`Source`] for each identity value and each precondition. This module
//! turns a source into a [`Reading`]:
//!
//! - [`Source::RuntimeInput`]: a fact the agent supplies itself, through [`RuntimeInputs`].
//!   [`crate::WorkerHost`] answers the supply voltage from the VCI (`PDU_IOCTL_READ_VBATT`) and
//!   reports every other input as [`Reading::CannotBeEstablished`]: the worker interface has no
//!   defined source for external supply, ignition, engine state or vehicle speed (raw pin
//!   voltages and analog inputs carry no agreed meaning for them), and `sim-vci` simulates only
//!   the battery voltage (ADR-238). [`FixedInputs`] holds fixed
//!   readings, for tests and for later wiring.
//! - [`Source::EcuService`]: a field of a diagnostic response, located through a
//!   [`ServiceSources`] table. A job with a document gets its table from the document
//!   ([`ServiceSources::from_document`], ADR-345), which declares the service and the
//!   field; a job without one keeps the table its caller builds by hand (ADR-245,
//!   consequences). The table's shape (below) is all that is read, so a document source of
//!   another shape is refused when the job is checked.
//!
//! "Cannot be established" and "cannot be decoded" are readings, not errors. A caller treats
//! either as a failed check (ADR-229). A failure to reach or use the worker (a transport
//! failure, a refused RPC, a request the policy refuses) is an error ([`HostError`]), which
//! fails the check as well.

use std::collections::HashMap;
use std::time::Duration;

use crate::client::WorkerApi;
use crate::document::JobDocument;
use diag_ir::declaration::{ByteOrder, DataKind, EcuVariant, Field, LengthKind, Placement, Shape};
use diag_ir::encode::encode_request;
use diag_ir::{DiagHost, Program, RuntimeInput, Source};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use vci_service_interface::{
    DataItem, GetObjectIdRequest, IoCtlRequest, ObjectType, data_item, io_ctl_request,
};

use crate::host::{HostError, unary};
use crate::link::Link;

/// D-PDU API IOCTL `PDU_IOCTL_READ_VBATT`: the battery voltage in millivolts, answered as an
/// unsigned 32-bit value. Its id is looked up by this name through `GetObjectId`.
const IOCTL_READ_VBATT_NAME: &str = "PDU_IOCTL_READ_VBATT";

/// What one read of a source gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// An integer fact: millivolts, a 0/1 flag, km/h, or an integer response field.
    Value(i64),
    /// A text fact: a VIN, a part number or a version.
    Text(String),
    /// The agent has no way to obtain this fact (an input without a source on this VCI).
    CannotBeEstablished,
    /// The source exists but its value could not be decoded: the source is not in the table,
    /// the ECU answered negatively, or the response is too short or malformed for the field.
    CannotBeDecoded,
}

impl Reading {
    /// Whether a value was obtained. A check treats `false` as failed.
    pub fn is_known(&self) -> bool {
        matches!(self, Reading::Value(_) | Reading::Text(_))
    }

    /// The integer value, if the reading is one.
    pub fn value(&self) -> Option<i64> {
        match self {
            Reading::Value(value) => Some(*value),
            _ => None,
        }
    }

    /// The text, if the reading is one.
    pub fn text(&self) -> Option<&str> {
        match self {
            Reading::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// The runtime inputs of the agent (`diag_ir::RuntimeInput`).
pub trait RuntimeInputs {
    /// Reads `input`. An input without a source gives [`Reading::CannotBeEstablished`]; a
    /// failure to reach or use the worker is an error.
    fn read(&mut self, input: RuntimeInput) -> Result<Reading, HostError>;
}

/// Runtime inputs with fixed readings. An input that was not set is
/// [`Reading::CannotBeEstablished`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixedInputs {
    readings: HashMap<RuntimeInputKey, Reading>,
}

// `RuntimeInput` has no `Hash`; the map keys on this mirror of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuntimeInputKey {
    SupplyVoltageMillivolts,
    ExternalSupplyConnected,
    IgnitionOn,
    EngineRunning,
    VehicleSpeedKmh,
}

impl From<RuntimeInput> for RuntimeInputKey {
    fn from(input: RuntimeInput) -> Self {
        match input {
            RuntimeInput::SupplyVoltageMillivolts => Self::SupplyVoltageMillivolts,
            RuntimeInput::ExternalSupplyConnected => Self::ExternalSupplyConnected,
            RuntimeInput::IgnitionOn => Self::IgnitionOn,
            RuntimeInput::EngineRunning => Self::EngineRunning,
            RuntimeInput::VehicleSpeedKmh => Self::VehicleSpeedKmh,
        }
    }
}

impl FixedInputs {
    /// No input set: every read is [`Reading::CannotBeEstablished`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the reading of `input`.
    pub fn with(mut self, input: RuntimeInput, reading: Reading) -> Self {
        self.readings.insert(input.into(), reading);
        self
    }
}

impl RuntimeInputs for FixedInputs {
    fn read(&mut self, input: RuntimeInput) -> Result<Reading, HostError> {
        Ok(self
            .readings
            .get(&input.into())
            .cloned()
            .unwrap_or(Reading::CannotBeEstablished))
    }
}

/// What the host knows about the VCI's `READ_VBATT` IOCTL. The lookup runs once per host (one
/// job on one link); its outcome, "unsupported" included, is kept, so a later worker failure
/// cannot turn an input the VCI does not offer into an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum VbattId {
    /// Not looked up yet.
    #[default]
    Unknown,
    /// The worker does not know the name: the VCI offers no voltage.
    Unsupported,
    Id(u32),
}

/// The cache entry a `GetObjectId` result gives. A J2534 worker answers an unknown name with
/// `NotFound` or `InvalidArgument`; any other failure stays an error and is not cached.
fn vbatt_lookup(found: Result<u32, HostError>) -> Result<VbattId, HostError> {
    match found {
        Ok(id) => Ok(VbattId::Id(id)),
        Err(HostError::Rpc { status, .. })
            if matches!(
                status.code(),
                tonic::Code::NotFound | tonic::Code::InvalidArgument
            ) =>
        {
            Ok(VbattId::Unsupported)
        }
        Err(error) => Err(error),
    }
}

/// Reads the supply voltage from the VCI of `link`. The IOCTL id is resolved by name on the
/// first call and kept in `cached`, together with an "unsupported" outcome. When the J2534
/// worker does not know the name, the VCI offers no voltage: [`Reading::CannotBeEstablished`].
/// A D-PDU worker reports an unknown name as an internal error, which stays a [`HostError`]
/// (it fails the check all the same). Blocks on `handle`, like the host's other primitives.
pub(crate) fn read_supply_voltage<C: WorkerApi>(
    handle: &Handle,
    client: &mut C,
    link: &Link,
    cached: &mut VbattId,
    deadline: Duration,
) -> Result<Reading, HostError> {
    if *cached == VbattId::Unknown {
        let request = GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: IOCTL_READ_VBATT_NAME.to_owned(),
        };
        let found = handle
            .block_on(unary(
                deadline,
                "GetObjectId",
                client.get_object_id(request),
            ))
            .map(|response| response.pdu_object_id);
        *cached = vbatt_lookup(found)?;
    }
    let id = match *cached {
        VbattId::Id(id) => id,
        VbattId::Unsupported | VbattId::Unknown => return Ok(Reading::CannotBeEstablished),
    };
    let request = IoCtlRequest {
        handle: Some(io_ctl_request::Handle::ModuleHandle(link.module_handle)),
        io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id)),
        input_data: None,
        has_output: true,
    };
    let response = handle.block_on(unary(deadline, "IoCtl", client.io_ctl(request)))?;
    Ok(voltage_from_data(response.output_data.as_ref()))
}

/// The millivolts a `READ_VBATT` answer carries: an unsigned 32-bit item. Anything else (no
/// output, another item) cannot be decoded.
fn voltage_from_data(data: Option<&DataItem>) -> Reading {
    match data.and_then(|item| item.data.as_ref()) {
        Some(data_item::Data::Unum32Value(millivolts)) => Reading::Value(i64::from(*millivolts)),
        _ => Reading::CannotBeDecoded,
    }
}

// ---------------------------------------------------------------- Service sources

/// How a response field is turned into a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Encoding {
    /// An unsigned big-endian integer of 1 to 8 bytes that fits an `i64`.
    UnsignedBigEndian,
    /// Printable ASCII (0x20 to 0x7E), at least one non-space character. Spaces around the text
    /// are kept as they are (no trimming); any other byte, padding such as 0x00 included, makes
    /// the field undecodable.
    Ascii,
}

/// Where one `EcuService` field is and how to read it.
///
/// Until the declaration part has a decoder, only one form is accepted: a ReadDataByIdentifier
/// (0x22) request for a single data identifier, `[0x22, hi, lo]`, with `offset` 2. Other
/// services do not echo their request in a form this table can check, and a request for
/// several identifiers lets the ECU leave some out. The response must be exactly the positive
/// SID, the echoed identifier and the field: the echo must equal the request's, and nothing
/// may follow the field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceField {
    /// `Source::EcuService::service_id` (starts at 1).
    pub service_id: u32,
    /// `Source::EcuService::field_id` (starts at 1).
    pub field_id: u32,
    /// The whole request, SID first (as `host::service_request_bytes` builds it): `[0x22, hi,
    /// lo]`.
    pub request: Vec<u8>,
    /// Bytes between the response SID and the field: the echoed data identifier, so 2.
    pub offset: usize,
    /// Length of the field in bytes.
    pub length: usize,
    pub encoding: Encoding,
}

/// Why a table was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TableError {
    #[error("service {service_id} field {field_id} is declared twice")]
    Duplicate { service_id: u32, field_id: u32 },
    #[error(
        "service {service_id} field {field_id}: only a single-identifier ReadDataByIdentifier \
         request ([0x22, hi, lo], offset 2) is supported"
    )]
    Unsupported { service_id: u32, field_id: u32 },
    #[error(
        "service {service_id} field {field_id}: ids start at 1, and a field is 1 byte or longer \
         (at most 8 for an integer)"
    )]
    BadField { service_id: u32, field_id: u32 },
    /// The job's document does not declare the source in the table's shape
    /// ([`ServiceSources::from_document`]).
    #[error(
        "service {service_id} field {field_id}: the document's declaration cannot be read: {reason}"
    )]
    Document {
        service_id: u32,
        field_id: u32,
        reason: String,
    },
}

/// The table that maps `EcuService` sources to requests and response fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ServiceField>", into = "Vec<ServiceField>")]
pub struct ServiceSources {
    fields: Vec<ServiceField>,
}

impl TryFrom<Vec<ServiceField>> for ServiceSources {
    type Error = TableError;

    fn try_from(fields: Vec<ServiceField>) -> Result<Self, TableError> {
        for (index, field) in fields.iter().enumerate() {
            let (service_id, field_id) = (field.service_id, field.field_id);
            if !matches!(field.request.as_slice(), [0x22, _, _]) || field.offset != 2 {
                return Err(TableError::Unsupported {
                    service_id,
                    field_id,
                });
            }
            let too_long = field.encoding == Encoding::UnsignedBigEndian && field.length > 8;
            if service_id == 0 || field_id == 0 || field.length == 0 || too_long {
                return Err(TableError::BadField {
                    service_id,
                    field_id,
                });
            }
            if fields[..index]
                .iter()
                .any(|f| f.service_id == service_id && f.field_id == field_id)
            {
                return Err(TableError::Duplicate {
                    service_id,
                    field_id,
                });
            }
        }
        Ok(Self { fields })
    }
}

impl From<ServiceSources> for Vec<ServiceField> {
    fn from(table: ServiceSources) -> Self {
        table.fields
    }
}

impl ServiceSources {
    /// Builds a table; refuses a duplicate `(service_id, field_id)` and any entry that is not a
    /// single-identifier ReadDataByIdentifier request with offset 2.
    pub fn new(fields: Vec<ServiceField>) -> Result<Self, TableError> {
        Self::try_from(fields)
    }

    /// Builds the table of `program`'s `EcuService` sources (identity and precondition sources)
    /// from the variant the job's `document` declares, one entry per distinct source (ADR-345).
    /// A source's service is a single-identifier ReadDataByIdentifier: its request is
    /// encoded as the runtime encodes it (`diag_ir::encode::encode_request`), the positive
    /// response echoes the identifier as its prefix, and the source's field is a fixed-length,
    /// byte-aligned ASCII text or unsigned big-endian integer that follows the echo and is the
    /// last byte of the response. The field's conversion is not applied (ADR-345 item 1). A source
    /// must also suit what reads it: the VIN is 17 bytes of ASCII text, a precondition is an
    /// unsigned integer (ADR-345 item 3). A source the document does not declare, or
    /// declares in another shape (a terminated or variable-length field among them), is an
    /// error, and so is one the table's own checks refuse ([`ServiceSources::new`]).
    pub fn from_document(document: &JobDocument, program: &Program) -> Result<Self, TableError> {
        let variant = document.variant();
        // Each distinct source with the roles that read it.
        let mut named: Vec<(u32, u32, Vec<Role>)> = Vec::new();
        for (source, role) in program_sources(program) {
            if let Source::EcuService {
                service_id,
                field_id,
            } = source
            {
                match named
                    .iter_mut()
                    .find(|(s, f, _)| (*s, *f) == (service_id, field_id))
                {
                    Some((_, _, roles)) => roles.push(role),
                    None => named.push((service_id, field_id, vec![role])),
                }
            }
        }
        let fields = named
            .into_iter()
            .map(|(service_id, field_id, roles)| {
                let document_error = |reason: String| TableError::Document {
                    service_id,
                    field_id,
                    reason,
                };
                // The VIN role has its own entry, which an identification plan's VIN sources go
                // through as well (ADR-399).
                let field = if roles.contains(&Role::Vin) {
                    vin_source_field(&variant, service_id, field_id)
                } else {
                    declared_field(&variant, service_id, field_id)
                }
                .map_err(document_error)?;
                // A source used in two roles must suit both.
                for role in roles.into_iter().filter(|role| *role != Role::Vin) {
                    role.accepts(&field).map_err(document_error)?;
                }
                Ok(field)
            })
            .collect::<Result<Vec<_>, TableError>>()?;
        Self::new(fields)
    }

    pub fn get(&self, service_id: u32, field_id: u32) -> Option<&ServiceField> {
        self.fields
            .iter()
            .find(|f| f.service_id == service_id && f.field_id == field_id)
    }
}

/// What reads a source: it decides the kind of field the source may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The VIN identity: 17 bytes of ASCII text.
    Vin,
    /// The hardware part number or software version identity: ASCII text or an unsigned integer.
    Version,
    /// A precondition's value, compared with a range: an unsigned integer.
    Precondition,
}

impl Role {
    fn accepts(self, field: &ServiceField) -> Result<(), String> {
        // The agent holds an unsigned value as a signed 64-bit integer, which every value of up
        // to seven bytes fits (`csv-tables.md` 4.13).
        if field.encoding == Encoding::UnsignedBigEndian && field.length > 7 {
            return Err("an unsigned source is at most 7 bytes".to_owned());
        }
        match (self, field.encoding) {
            (Role::Vin, Encoding::Ascii) if field.length == 17 => Ok(()),
            (Role::Vin, _) => Err("the VIN source must be 17 bytes of ASCII text".to_owned()),
            (Role::Version, _) => Ok(()),
            (Role::Precondition, Encoding::UnsignedBigEndian) => Ok(()),
            (Role::Precondition, _) => {
                Err("a precondition source must be an unsigned integer".to_owned())
            }
        }
    }
}

/// Every source `program` declares with the role that reads it: the identity sources and the
/// default- and programming-session sources of the preconditions.
fn program_sources(program: &Program) -> Vec<(Source, Role)> {
    let identity = &program.identity;
    let preconditions = &program.preconditions;
    let mut sources: Vec<(Source, Role)> = Vec::new();
    sources.extend(identity.vin.map(|s| (s, Role::Vin)));
    for source in [identity.hardware_part_number, identity.software_version]
        .into_iter()
        .flatten()
    {
        sources.push((source, Role::Version));
    }
    for precondition in [
        &preconditions.voltage_mv,
        &preconditions.external_supply,
        &preconditions.ignition,
        &preconditions.engine,
        &preconditions.vehicle_speed,
    ]
    .into_iter()
    .flatten()
    {
        for source in [
            precondition.default_session,
            precondition.programming_session,
        ]
        .into_iter()
        .flatten()
        {
            sources.push((source, Role::Precondition));
        }
    }
    sources
}

/// The table entry for the VIN source field `field_id` of service `service_id` in `variant`: the
/// declaration in the table's shape (`declared_field`) that is also 17 bytes of ASCII text
/// ([`Role::Vin`]), or what keeps it from that. Both [`ServiceSources::from_document`] and an
/// identification plan's check go through it (ADR-345 item 3, ADR-399).
pub(crate) fn vin_source_field(
    variant: &EcuVariant<'_>,
    service_id: u32,
    field_id: u32,
) -> Result<ServiceField, String> {
    let field = declared_field(variant, service_id, field_id)?;
    Role::Vin.accepts(&field)?;
    Ok(field)
}

/// The table entry for field `field_id` of service `service_id` in `variant`, or what keeps the
/// declaration from the table's shape.
fn declared_field(
    variant: &EcuVariant<'_>,
    service_id: u32,
    field_id: u32,
) -> Result<ServiceField, String> {
    let service = variant
        .services()
        .and_then(|services| services.iter().find(|s| s.id() == service_id))
        .ok_or("the document declares no such service")?;
    let request = service.request().ok_or("the service declares no request")?;
    let positive = service
        .positive()
        .ok_or("the service declares no positive response")?;
    // The operand, after the SID; a request with parameters has no values to encode here.
    let operand = encode_request(&request, &std::collections::HashMap::new())
        .map_err(|error| format!("the request cannot be encoded without parameters: {error}"))?;
    let mut bytes = vec![request.sid()];
    bytes.extend_from_slice(&operand);
    let prefix = positive.prefix().map_or(&[][..], |p| p.bytes());
    if positive.sid() != request.sid().wrapping_add(0x40) || prefix != operand.as_slice() {
        return Err("the positive response does not echo the request".to_owned());
    }
    // Where each field of the response lies, in bytes from the SID.
    let mut edge = 1 + prefix.len();
    let mut end_of_response = 0;
    let mut target = None;
    for field in positive.fields().into_iter().flatten() {
        if field.shape_type() != Shape::NONE || field.length_kind() != LengthKind::Fixed {
            return Err("the response has a field that is not of fixed length".to_owned());
        }
        let (byte, bit) = match field.placement() {
            Placement::Absolute => ((field.bit_offset() / 8) as usize, field.bit_offset() % 8),
            Placement::AfterPrevious if field.bit_offset() <= 7 => (edge, field.bit_offset()),
            _ => return Err("the response has a field with an unusable placement".to_owned()),
        };
        let width = field.bit_length();
        edge = byte + (u64::from(bit) + u64::from(width)).div_ceil(8) as usize;
        end_of_response = end_of_response.max(edge);
        if field.id() == field_id {
            // The VM decodes the first field of an id (`diag_ir`); a second one would make the
            // table and the procedure read different bytes for the same source.
            if target.is_some() {
                return Err("the response declares the field twice".to_owned());
            }
            target = Some((field, byte, bit, edge));
        }
    }
    let (field, byte, bit, end) = target.ok_or("the service's response declares no such field")?;
    if bit != 0 || field.bit_length() == 0 || field.bit_length() % 8 != 0 {
        return Err("the field is not a whole number of bytes at a byte edge".to_owned());
    }
    if end != end_of_response {
        return Err("another field of the response reaches past the field".to_owned());
    }
    let encoding = field_encoding(&field)?;
    Ok(ServiceField {
        service_id,
        field_id,
        request: bytes,
        // From the byte after the SID; the table checks it is the echo's length.
        offset: byte.saturating_sub(1),
        length: (field.bit_length() / 8) as usize,
        encoding,
    })
}

/// The table's encoding of `field`: ASCII text or a big-endian unsigned integer, as stored. The
/// field's conversion, if any, is not applied: the table reads the raw bytes (ADR-345 item 1).
fn field_encoding(field: &Field<'_>) -> Result<Encoding, String> {
    match field.kind() {
        DataKind::Ascii => Ok(Encoding::Ascii),
        DataKind::Uint if field.byte_order() == ByteOrder::Big => Ok(Encoding::UnsignedBigEndian),
        _ => Err("the field is neither ASCII text nor a big-endian unsigned integer".to_owned()),
    }
}

/// Reads one source with one value that is both the sender and the runtime inputs (the
/// [`crate::WorkerHost`]): a runtime input through [`RuntimeInputs`], an `EcuService` field
/// through `table` by sending its request. A source the table lacks, a negative response, a
/// response that is short, long, or does not echo the request, and a field that does not fit
/// its encoding give [`Reading::CannotBeDecoded`]; a failure to send or receive is the
/// host's error.
pub fn resolve_source<H>(
    source: Source,
    table: &ServiceSources,
    host: &mut H,
) -> Result<Reading, HostError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    match source {
        Source::RuntimeInput(input) => host.read(input),
        Source::EcuService {
            service_id,
            field_id,
        } => {
            let Some(field) = table.get(service_id, field_id) else {
                return Ok(Reading::CannotBeDecoded);
            };
            let Some((&sid, payload)) = field.request.split_first() else {
                return Ok(Reading::CannotBeDecoded);
            };
            let response = host.service_request(u16::from(sid), payload)?;
            Ok(decode_field(field, sid, &response))
        }
    }
}

/// What [`read_field_bytes`] got for a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldBytes {
    /// The field's raw bytes, from a response [`resolve_source`] would read as a value.
    Field(Vec<u8>),
    /// A negative response to the field's request, with its response code.
    Negative(u8),
    /// Anything else: a source the table lacks, a runtime input (which has no field bytes), or
    /// a positive response that does not carry a field that decodes.
    Unreadable,
}

/// [`read_field_bytes`] for the identity `kind`, noting what was read with the host (the
/// communication log's `identity`, design 8.11). A request that failed notes nothing: its
/// exchange entry says why.
pub fn read_identity_bytes<H>(
    kind: diag_ir::IdentityKind,
    source: Source,
    table: &ServiceSources,
    host: &mut H,
) -> Result<FieldBytes, HostError>
where
    H: DiagHost<Error = HostError>,
{
    let read = read_field_bytes(source, table, host)?;
    host.note_identity(kind, identity_read_of(&read));
    Ok(read)
}

/// What the communication log's `identity` event says of a field read.
pub(crate) fn identity_read_of(read: &FieldBytes) -> diag_ir::IdentityRead<'_> {
    match read {
        FieldBytes::Field(bytes) => diag_ir::IdentityRead::Value(bytes),
        FieldBytes::Negative(nrc) => diag_ir::IdentityRead::Negative(*nrc),
        FieldBytes::Unreadable => diag_ir::IdentityRead::Unreadable,
    }
}

/// [`resolve_source`] for the identity `kind`, noting what was read with the host as
/// [`read_identity_bytes`] does, from the same one response: the decoded value, a negative
/// response with its code, or unreadable for anything else.
pub fn resolve_identity<H>(
    kind: diag_ir::IdentityKind,
    source: Source,
    table: &ServiceSources,
    host: &mut H,
) -> Result<Reading, HostError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs,
{
    let mut noted = None;
    let reading = match source {
        Source::RuntimeInput(input) => host.read(input)?,
        Source::EcuService {
            service_id,
            field_id,
        } => match table
            .get(service_id, field_id)
            .and_then(|field| Some((field, field.request.split_first()?)))
        {
            None => Reading::CannotBeDecoded,
            Some((field, (&sid, payload))) => {
                let response = host.service_request(u16::from(sid), payload)?;
                if let [0x7F, echoed, nrc] = response[..]
                    && echoed == sid
                {
                    noted = Some(diag_ir::IdentityRead::Negative(nrc));
                }
                decode_field(field, sid, &response)
            }
        },
    };
    let number;
    let read = noted.unwrap_or(match &reading {
        Reading::Text(text) => diag_ir::IdentityRead::Value(text.as_bytes()),
        Reading::Value(value) => {
            number = value.to_string();
            diag_ir::IdentityRead::Value(number.as_bytes())
        }
        Reading::CannotBeEstablished | Reading::CannotBeDecoded => {
            diag_ir::IdentityRead::Unreadable
        }
    });
    host.note_identity(kind, read);
    Ok(reading)
}

/// Reads the raw bytes of an `EcuService` field, for the write-job journal, which records the
/// ECU's identity as the ECU answered it (ADR-244, ADR-252). The request and the checks are
/// those of [`resolve_source`], and the field must also decode under its encoding. A negative
/// response is told apart, so a caller can recognise a declared answer such as "no valid
/// application" (design 8.2.5).
pub fn read_field_bytes<H>(
    source: Source,
    table: &ServiceSources,
    host: &mut H,
) -> Result<FieldBytes, HostError>
where
    H: DiagHost<Error = HostError>,
{
    let Source::EcuService {
        service_id,
        field_id,
    } = source
    else {
        return Ok(FieldBytes::Unreadable);
    };
    let Some(field) = table.get(service_id, field_id) else {
        return Ok(FieldBytes::Unreadable);
    };
    let Some((&sid, payload)) = field.request.split_first() else {
        return Ok(FieldBytes::Unreadable);
    };
    let response = host.service_request(u16::from(sid), payload)?;
    Ok(field_bytes_of(field, sid, &response))
}

/// What `response` to the request with service `sid` gives for `field`: the field's raw bytes when
/// the response is exactly the positive SID, the echo and a field that decodes, the response code
/// of a negative response to the request, else unreadable. [`read_field_bytes`] sends the request
/// and reads it so; a caller that sent the request itself reads each declaration of the one
/// response so (ADR-399).
pub(crate) fn field_bytes_of(field: &ServiceField, sid: u8, response: &[u8]) -> FieldBytes {
    if let [0x7F, echoed, nrc] = response[..]
        && echoed == sid
    {
        return FieldBytes::Negative(nrc);
    }
    extract_field(field, sid, response)
        .filter(|bytes| decode_bytes(field, bytes).is_known())
        .map_or(FieldBytes::Unreadable, |bytes| {
            FieldBytes::Field(bytes.to_vec())
        })
}

/// The bytes of `field` in `response` to the request with service `sid`, if the response is
/// exactly the positive SID, the echo and the field.
fn extract_field<'a>(field: &ServiceField, sid: u8, response: &'a [u8]) -> Option<&'a [u8]> {
    // A positive response carries the SID plus 0x40 (ISO 14229-1:2026 clause 7.4); a negative
    // one starts with 0x7F.
    let (&first, rest) = response.split_first()?;
    if first != sid.wrapping_add(0x40) {
        return None;
    }
    let params = &field.request[1..];
    // Exactly the echo, then the field: nothing missing, nothing extra.
    if field.offset != params.len()
        || field.offset.checked_add(field.length) != Some(rest.len())
        || rest[..field.offset] != *params
    {
        return None;
    }
    Some(&rest[field.offset..])
}

/// Extracts `field` from `response` to the request with service `sid`.
fn decode_field(field: &ServiceField, sid: u8, response: &[u8]) -> Reading {
    extract_field(field, sid, response)
        .map_or(Reading::CannotBeDecoded, |bytes| decode_bytes(field, bytes))
}

/// Turns the bytes of `field` into a reading by its encoding.
fn decode_bytes(field: &ServiceField, bytes: &[u8]) -> Reading {
    match field.encoding {
        Encoding::UnsignedBigEndian => {
            if bytes.is_empty() || bytes.len() > 8 {
                return Reading::CannotBeDecoded;
            }
            let value = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            i64::try_from(value).map_or(Reading::CannotBeDecoded, Reading::Value)
        }
        Encoding::Ascii => {
            if !bytes.iter().all(|b| (0x20..=0x7E).contains(b)) || bytes.iter().all(|b| *b == b' ')
            {
                return Reading::CannotBeDecoded;
            }
            Reading::Text(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RuntimeInput; 5] = [
        RuntimeInput::SupplyVoltageMillivolts,
        RuntimeInput::ExternalSupplyConnected,
        RuntimeInput::IgnitionOn,
        RuntimeInput::EngineRunning,
        RuntimeInput::VehicleSpeedKmh,
    ];

    fn fixed_reports(input: RuntimeInput, value: i64) {
        let mut inputs = FixedInputs::new().with(input, Reading::Value(value));
        assert_eq!(inputs.read(input).unwrap(), Reading::Value(value));
        for other in ALL.into_iter().filter(|other| *other != input) {
            assert_eq!(inputs.read(other).unwrap(), Reading::CannotBeEstablished);
        }
        assert_eq!(
            FixedInputs::new().read(input).unwrap(),
            Reading::CannotBeEstablished
        );
    }

    #[test]
    fn supply_voltage_is_reported_or_cannot_be_established() {
        fixed_reports(RuntimeInput::SupplyVoltageMillivolts, 12_600);
    }

    #[test]
    fn external_supply_is_reported_or_cannot_be_established() {
        fixed_reports(RuntimeInput::ExternalSupplyConnected, 1);
    }

    #[test]
    fn ignition_is_reported_or_cannot_be_established() {
        fixed_reports(RuntimeInput::IgnitionOn, 0);
    }

    #[test]
    fn engine_is_reported_or_cannot_be_established() {
        fixed_reports(RuntimeInput::EngineRunning, 0);
    }

    #[test]
    fn vehicle_speed_is_reported_or_cannot_be_established() {
        fixed_reports(RuntimeInput::VehicleSpeedKmh, 87);
    }

    #[test]
    fn an_unknown_vbatt_name_is_cached_as_unsupported() {
        let rpc = |code| {
            Err(HostError::Rpc {
                rpc: "GetObjectId",
                status: Box::new(tonic::Status::new(code, "no such object")),
            })
        };
        assert_eq!(vbatt_lookup(Ok(7)).unwrap(), VbattId::Id(7));
        assert_eq!(
            vbatt_lookup(rpc(tonic::Code::NotFound)).unwrap(),
            VbattId::Unsupported
        );
        assert_eq!(
            vbatt_lookup(rpc(tonic::Code::InvalidArgument)).unwrap(),
            VbattId::Unsupported
        );
        assert!(vbatt_lookup(rpc(tonic::Code::Unavailable)).is_err());
        assert_eq!(VbattId::default(), VbattId::Unknown);
    }

    #[test]
    fn a_vbatt_answer_decodes_to_millivolts() {
        let item = |data| DataItem { data: Some(data) };
        assert_eq!(
            voltage_from_data(Some(&item(data_item::Data::Unum32Value(12_345)))),
            Reading::Value(12_345)
        );
        assert_eq!(
            voltage_from_data(Some(&item(data_item::Data::Unum32Value(u32::MAX)))),
            Reading::Value(i64::from(u32::MAX))
        );
        // No output, an empty item, or another kind of item.
        assert_eq!(voltage_from_data(None), Reading::CannotBeDecoded);
        assert_eq!(
            voltage_from_data(Some(&DataItem { data: None })),
            Reading::CannotBeDecoded
        );
        assert_eq!(
            voltage_from_data(Some(&item(data_item::Data::BytearrayData(
                Default::default()
            )))),
            Reading::CannotBeDecoded
        );
    }

    /// Answers every request with the next scripted response, records the requests, and
    /// supplies fixed runtime inputs.
    struct Fake {
        responses: Vec<Result<Vec<u8>, HostError>>,
        sent: Vec<(u16, Vec<u8>)>,
        inputs: FixedInputs,
        /// The identity values noted with the host, as `IdentityRead` gives them.
        noted: Vec<(diag_ir::IdentityKind, String)>,
    }

    impl Fake {
        fn answering(response: &[u8]) -> Self {
            Self {
                responses: vec![Ok(response.to_vec())],
                sent: Vec::new(),
                inputs: FixedInputs::new(),
                noted: Vec::new(),
            }
        }
    }

    impl RuntimeInputs for Fake {
        fn read(&mut self, input: RuntimeInput) -> Result<Reading, HostError> {
            self.inputs.read(input)
        }
    }

    impl DiagHost for Fake {
        type Error = HostError;

        fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
            self.sent.push((service, payload.to_vec()));
            self.responses.remove(0)
        }
        fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, HostError> {
            unreachable!()
        }
        fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, HostError> {
            unreachable!()
        }
        fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, HostError> {
            unreachable!()
        }
        fn security_access(
            &mut self,
            _: u64,
            _: u8,
            _: &[u8],
        ) -> Result<Option<Vec<u8>>, HostError> {
            unreachable!()
        }
        fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), HostError> {
            unreachable!()
        }
        fn wait(&mut self, _: u64, _: u32) -> Result<bool, HostError> {
            unreachable!()
        }
        fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            unreachable!()
        }
        fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            unreachable!()
        }
        fn monitor_capture(&mut self, _: u32) -> Result<(), HostError> {
            unreachable!()
        }
        fn artifact_bytes(&mut self, _: u64, _: u32) -> Result<Vec<u8>, HostError> {
            unreachable!()
        }
        fn runtime_input(&mut self, input: RuntimeInput) -> Result<i64, HostError> {
            self.inputs
                .read(input)?
                .value()
                .ok_or(HostError::InputNotEstablished(input))
        }
        fn log(&mut self, _: u8, _: &str) {}
        fn note_identity(&mut self, kind: diag_ir::IdentityKind, read: diag_ir::IdentityRead<'_>) {
            self.noted.push((kind, format!("{read:?}")));
        }
    }

    fn vin_field() -> ServiceField {
        // Service 1, field 1: the VIN, data identifier F190, 17 characters after the echo.
        ServiceField {
            service_id: 1,
            field_id: 1,
            request: vec![0x22, 0xF1, 0x90],
            offset: 2,
            length: 17,
            encoding: Encoding::Ascii,
        }
    }

    fn table() -> ServiceSources {
        ServiceSources::new(vec![
            vin_field(),
            // Service 2, field 1: a two-byte counter at data identifier F100.
            ServiceField {
                service_id: 2,
                field_id: 1,
                request: vec![0x22, 0xF1, 0x00],
                offset: 2,
                length: 2,
                encoding: Encoding::UnsignedBigEndian,
            },
        ])
        .unwrap()
    }

    /// ADR-399: `field_bytes_of` reads a response the caller sent as `read_field_bytes` reads
    /// the one it sends, and `identity_read_of` tells the log what was read.
    #[test]
    fn a_response_read_without_sending_gives_the_same_bytes_as_a_read() {
        let field = vin_field();
        let mut response = vec![0x62, 0xF1, 0x90];
        response.extend_from_slice(b"WVWZZZ1JZ3W386752");
        let read = field_bytes_of(&field, 0x22, &response);
        assert_eq!(read, FieldBytes::Field(b"WVWZZZ1JZ3W386752".to_vec()));
        assert_eq!(
            identity_read_of(&read),
            diag_ir::IdentityRead::Value(b"WVWZZZ1JZ3W386752")
        );
        let negative = field_bytes_of(&field, 0x22, &[0x7F, 0x22, 0x31]);
        assert_eq!(negative, FieldBytes::Negative(0x31));
        assert_eq!(
            identity_read_of(&negative),
            diag_ir::IdentityRead::Negative(0x31)
        );
        // A short response, one for another service, and an empty one carry no field.
        for response in [&response[..10], &[0x7F, 0x10, 0x31], &[][..]] {
            let read = field_bytes_of(&field, 0x22, response);
            assert_eq!(read, FieldBytes::Unreadable, "{response:02x?}");
            assert_eq!(identity_read_of(&read), diag_ir::IdentityRead::Unreadable);
        }
    }

    fn resolve(source: Source, host: &mut Fake) -> Reading {
        resolve_source(source, &table(), host).unwrap()
    }

    fn ecu(service_id: u32, field_id: u32) -> Source {
        Source::EcuService {
            service_id,
            field_id,
        }
    }

    fn vin_response(vin: &[u8]) -> Vec<u8> {
        let mut response = vec![0x62, 0xF1, 0x90];
        response.extend_from_slice(vin);
        response
    }

    /// The real host serves as sender and inputs in one call (compile-level check).
    #[expect(dead_code, reason = "only has to compile")]
    fn worker_host_resolves_any_source(
        host: &mut crate::WorkerHost<worker_host::client::WorkerClient>,
        source: Source,
    ) {
        let _ = resolve_source(source, &ServiceSources::default(), host);
    }

    /// An identity read notes what it read with the host: the field's bytes, a negative
    /// response's code, or an answer that decodes to no value; a failed request notes nothing.
    #[test]
    fn identity_reads_note_what_they_read() {
        use diag_ir::IdentityKind::{HardwarePartNumber, Vin};
        let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW000001"));
        let reading = resolve_identity(Vin, ecu(1, 1), &table(), &mut host).unwrap();
        assert_eq!(reading, Reading::Text("WVWZZZ1JZXW000001".to_owned()));
        let mut host2 = Fake::answering(&[0x7F, 0x22, 0x31]);
        read_identity_bytes(HardwarePartNumber, ecu(1, 1), &table(), &mut host2).unwrap();
        let mut host3 = Fake::answering(&[0x62, 0xF1, 0x90, 0x41]);
        resolve_identity(Vin, ecu(1, 1), &table(), &mut host3).unwrap();
        // A VIN the ECU refuses keeps its code, as the byte reads do; the restart still reads it
        // as not decoded.
        let mut host4 = Fake::answering(&[0x7F, 0x22, 0x22]);
        let refused = resolve_identity(Vin, ecu(1, 1), &table(), &mut host4).unwrap();
        assert_eq!(refused, Reading::CannotBeDecoded);
        assert_eq!(host4.sent.len(), 1, "one request");
        let mut failed = Fake {
            responses: vec![Err(HostError::NoResponse)],
            ..Fake::answering(&[])
        };
        assert!(read_identity_bytes(HardwarePartNumber, ecu(1, 1), &table(), &mut failed).is_err());
        assert_eq!(
            [
                host.noted,
                host2.noted,
                host3.noted,
                host4.noted,
                failed.noted
            ]
            .concat(),
            [
                (
                    Vin,
                    format!("{:?}", diag_ir::IdentityRead::Value(b"WVWZZZ1JZXW000001"))
                ),
                (
                    HardwarePartNumber,
                    format!("{:?}", diag_ir::IdentityRead::Negative(0x31))
                ),
                (Vin, format!("{:?}", diag_ir::IdentityRead::Unreadable)),
                (Vin, format!("{:?}", diag_ir::IdentityRead::Negative(0x22))),
            ]
        );
    }

    #[test]
    fn field_bytes_are_the_raw_field_of_a_decodable_answer() {
        let read = |source, response: &[u8]| {
            let mut host = Fake::answering(response);
            let bytes = read_field_bytes(source, &table(), &mut host).unwrap();
            (bytes, host.sent)
        };
        let (bytes, sent) = read(ecu(2, 1), &[0x62, 0xF1, 0x00, 0x01, 0x02]);
        assert_eq!(bytes, FieldBytes::Field(vec![0x01, 0x02]));
        assert_eq!(sent, [(0x22, vec![0xF1, 0x00])]);
        let (bytes, _) = read(ecu(1, 1), &vin_response(b"WVWZZZ1JZXW000001"));
        assert_eq!(bytes, FieldBytes::Field(b"WVWZZZ1JZXW000001".to_vec()));
        // A negative response keeps its code; one for another service does not count.
        assert_eq!(
            read(ecu(1, 1), &[0x7F, 0x22, 0x31]).0,
            FieldBytes::Negative(0x31)
        );
        assert_eq!(
            read(ecu(1, 1), &[0x7F, 0x2E, 0x31]).0,
            FieldBytes::Unreadable
        );
        // A response that does not read as a value gives no bytes.
        for response in [
            vin_response(b"                 "),
            vin_response(b"WVWZZZ1JZXW00000"),
            vec![0x7F, 0x22, 0x31, 0x00],
        ] {
            assert_eq!(read(ecu(1, 1), &response).0, FieldBytes::Unreadable);
        }
        // Nothing is sent for a source without a request.
        let (bytes, sent) = read(ecu(9, 1), &[]);
        assert_eq!(bytes, FieldBytes::Unreadable);
        assert!(sent.is_empty());
        let (bytes, sent) = read(
            Source::RuntimeInput(RuntimeInput::SupplyVoltageMillivolts),
            &[],
        );
        assert_eq!(bytes, FieldBytes::Unreadable);
        assert!(sent.is_empty());
    }

    #[test]
    fn an_integer_field_is_read_big_endian() {
        let mut host = Fake::answering(&[0x62, 0xF1, 0x00, 0x01, 0x02]);
        assert_eq!(resolve(ecu(2, 1), &mut host), Reading::Value(0x0102));
        assert_eq!(host.sent, [(0x22, vec![0xF1, 0x00])]);
    }

    #[test]
    fn an_ascii_field_is_read_as_text() {
        let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW000001"));
        assert_eq!(
            resolve(ecu(1, 1), &mut host),
            Reading::Text("WVWZZZ1JZXW000001".to_owned())
        );
        assert_eq!(host.sent, [(0x22, vec![0xF1, 0x90])]);
    }

    #[test]
    fn an_unknown_source_is_not_sent_and_cannot_be_decoded() {
        let mut host = Fake::answering(&[]);
        assert_eq!(resolve(ecu(9, 1), &mut host), Reading::CannotBeDecoded);
        assert_eq!(resolve(ecu(1, 2), &mut host), Reading::CannotBeDecoded);
        assert!(host.sent.is_empty());
    }

    #[test]
    fn a_negative_response_cannot_be_decoded() {
        let mut host = Fake::answering(&[0x7F, 0x22, 0x31]);
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
    }

    #[test]
    fn a_short_response_cannot_be_decoded() {
        let mut host = Fake::answering(&[0x62, 0xF1, 0x90, b'W', b'V']);
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
        let mut host = Fake::answering(&[]);
        assert_eq!(resolve(ecu(2, 1), &mut host), Reading::CannotBeDecoded);
    }

    #[test]
    fn trailing_bytes_cannot_be_decoded() {
        let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW000001X"));
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
    }

    #[test]
    fn a_response_for_another_identifier_cannot_be_decoded() {
        let mut response = vec![0x62, 0xF1, 0x86];
        response.extend_from_slice(b"WVWZZZ1JZXW000001");
        let mut host = Fake::answering(&response);
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
    }

    #[test]
    fn a_request_reading_several_identifiers_is_refused() {
        for offset in [2, 4] {
            let field = ServiceField {
                request: vec![0x22, 0xF1, 0x90, 0xF1, 0x86],
                offset,
                ..vin_field()
            };
            assert!(matches!(
                ServiceSources::new(vec![field]),
                Err(TableError::Unsupported { .. })
            ));
        }
    }

    #[test]
    fn non_ascii_bytes_cannot_be_decoded() {
        let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW00000\xE9"));
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
    }

    #[test]
    fn an_all_space_text_cannot_be_decoded_but_padding_is_kept() {
        let mut host = Fake::answering(&vin_response(&[b' '; 17]));
        assert_eq!(resolve(ecu(1, 1), &mut host), Reading::CannotBeDecoded);
        let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW0000  "));
        assert_eq!(
            resolve(ecu(1, 1), &mut host),
            Reading::Text("WVWZZZ1JZXW0000  ".to_owned())
        );
    }

    #[test]
    fn a_transport_failure_is_an_error() {
        let mut host = Fake {
            responses: vec![Err(HostError::NoResponse)],
            sent: Vec::new(),
            inputs: FixedInputs::new(),
            noted: Vec::new(),
        };
        let got = resolve_source(ecu(1, 1), &table(), &mut host);
        assert!(matches!(got, Err(HostError::NoResponse)));
    }

    #[test]
    fn a_runtime_input_source_goes_to_the_inputs() {
        let mut host = Fake::answering(&[]);
        host.inputs = FixedInputs::new().with(RuntimeInput::IgnitionOn, Reading::Value(1));
        let table = table();
        assert_eq!(
            resolve_source(
                Source::RuntimeInput(RuntimeInput::IgnitionOn),
                &table,
                &mut host
            )
            .unwrap(),
            Reading::Value(1)
        );
        assert_eq!(
            resolve_source(
                Source::RuntimeInput(RuntimeInput::EngineRunning),
                &table,
                &mut host
            )
            .unwrap(),
            Reading::CannotBeEstablished
        );
        assert!(host.sent.is_empty());
    }

    #[test]
    fn a_table_survives_json() {
        let json = serde_json::to_string(&table()).unwrap();
        assert_eq!(
            serde_json::from_str::<ServiceSources>(&json).unwrap(),
            table()
        );
    }

    #[test]
    fn a_table_with_a_duplicate_is_refused() {
        let err = ServiceSources::new(vec![vin_field(), vin_field()]).unwrap_err();
        assert_eq!(
            err,
            TableError::Duplicate {
                service_id: 1,
                field_id: 1
            }
        );
        let json = serde_json::to_string(&vec![vin_field(), vin_field()]).unwrap();
        assert!(serde_json::from_str::<ServiceSources>(&json).is_err());
    }

    #[test]
    fn only_a_single_identifier_read_is_accepted() {
        for (request, offset) in [
            (vec![], 0),
            (vec![0x3E, 0x00], 1),
            (vec![0x19, 0x01, 0xFF], 2),
            (vec![0x31, 0x01, 0x02, 0x03], 3),
            (vec![0x3F, 0xF1, 0x90], 2),
            (vec![0x22, 0xF1], 1),
            (vec![0x22, 0xF1, 0x90], 0),
            (vec![0x22, 0xF1, 0x90], 3),
        ] {
            let field = ServiceField {
                request,
                offset,
                ..vin_field()
            };
            assert!(matches!(
                ServiceSources::new(vec![field]),
                Err(TableError::Unsupported { .. })
            ));
        }
        assert!(ServiceSources::new(vec![vin_field()]).is_ok());
    }

    #[test]
    fn impossible_ids_and_widths_are_refused() {
        let integer = |length| ServiceField {
            length,
            encoding: Encoding::UnsignedBigEndian,
            ..vin_field()
        };
        for field in [
            ServiceField {
                service_id: 0,
                ..vin_field()
            },
            ServiceField {
                field_id: 0,
                ..vin_field()
            },
            ServiceField {
                length: 0,
                ..vin_field()
            },
            integer(9),
        ] {
            assert!(matches!(
                ServiceSources::new(vec![field]),
                Err(TableError::BadField { .. })
            ));
        }
        assert!(ServiceSources::new(vec![integer(8)]).is_ok());
    }

    #[test]
    fn integer_fields_that_do_not_fit_are_refused() {
        let field = |length| ServiceField {
            service_id: 1,
            field_id: 1,
            request: vec![0x22],
            offset: 0,
            length,
            encoding: Encoding::UnsignedBigEndian,
        };
        let response = |len| [vec![0x62], vec![0xFF; len]].concat();
        assert_eq!(
            decode_field(&field(9), 0x22, &response(9)),
            Reading::CannotBeDecoded
        );
        assert_eq!(
            decode_field(&field(0), 0x22, &response(0)),
            Reading::CannotBeDecoded
        );
        assert_eq!(
            decode_field(&field(8), 0x22, &response(8)),
            Reading::CannotBeDecoded
        );
        assert_eq!(
            decode_field(&field(7), 0x22, &response(7)),
            Reading::Value(0x00FF_FFFF_FFFF_FFFF)
        );
    }

    // ------------------------------------------------ a table from the job's document

    mod from_document {
        use super::*;
        use crate::document::tests::{document_of, empty_program, read_service};
        use diag_ir::declaration::{ByteOrder, DataKind, LengthKind, ServiceT};
        use diag_ir::{IdentitySources, Precondition, Satisfied};

        fn ecu(service_id: u32) -> Source {
            Source::EcuService {
                service_id,
                field_id: 1,
            }
        }

        fn document(services: Vec<ServiceT>) -> JobDocument {
            JobDocument::new(document_of(services, None)).unwrap()
        }

        /// A program that reads its VIN through service 1, the hardware part number through
        /// service 2, and an engine precondition's programming source through service 3.
        fn named() -> Program {
            let mut program = empty_program();
            program.identity = IdentitySources {
                vin: Some(ecu(1)),
                hardware_part_number: Some(ecu(2)),
                software_version: None,
            };
            program.preconditions.engine = Some(Precondition {
                satisfied: Satisfied { lower: 0, upper: 0 },
                default_session: Some(Source::RuntimeInput(RuntimeInput::EngineRunning)),
                programming_session: Some(ecu(3)),
            });
            program
        }

        fn services() -> Vec<ServiceT> {
            vec![
                read_service(1, "readVin", [0xF1, 0x90], DataKind::Ascii, 17),
                read_service(2, "readHardware", [0xF1, 0x91], DataKind::Ascii, 8),
                read_service(3, "readCounter", [0xF1, 0x00], DataKind::Uint, 2),
            ]
        }

        fn refused(services: Vec<ServiceT>) -> TableError {
            ServiceSources::from_document(&document(services), &named()).unwrap_err()
        }

        /// ADR-399: the VIN entry of the table code, which an identification plan's check shares
        /// with `from_document`: the declaration of service 1 gives the table's entry, and a source
        /// of another shape is refused with the reason `from_document` reports.
        #[test]
        fn the_vin_entry_is_the_one_from_document_builds() {
            let document = document(services());
            let variant = document.variant();
            let field = vin_source_field(&variant, 1, 1).unwrap();
            let table = ServiceSources::from_document(&document, &named()).unwrap();
            assert_eq!(table.get(1, 1), Some(&field));
            // The hardware part number (8 bytes) is a table entry but not a VIN source.
            assert!(vin_source_field(&variant, 2, 1).is_err());
            assert!(vin_source_field(&variant, 3, 1).is_err());
            assert!(vin_source_field(&variant, 9, 1).is_err(), "no such service");
            assert!(vin_source_field(&variant, 1, 9).is_err(), "no such field");
        }

        #[test]
        fn fixed_length_reads_give_a_table_that_resolves_the_sources() {
            let table = ServiceSources::from_document(&document(services()), &named()).unwrap();
            let expected = ServiceSources::new(vec![
                ServiceField {
                    service_id: 1,
                    field_id: 1,
                    request: vec![0x22, 0xF1, 0x90],
                    offset: 2,
                    length: 17,
                    encoding: Encoding::Ascii,
                },
                ServiceField {
                    service_id: 2,
                    field_id: 1,
                    request: vec![0x22, 0xF1, 0x91],
                    offset: 2,
                    length: 8,
                    encoding: Encoding::Ascii,
                },
                ServiceField {
                    service_id: 3,
                    field_id: 1,
                    request: vec![0x22, 0xF1, 0x00],
                    offset: 2,
                    length: 2,
                    encoding: Encoding::UnsignedBigEndian,
                },
            ])
            .unwrap();
            assert_eq!(table, expected);
            let mut host = Fake::answering(&vin_response(b"WVWZZZ1JZXW000001"));
            assert_eq!(
                resolve_source(ecu(1), &table, &mut host).unwrap(),
                Reading::Text("WVWZZZ1JZXW000001".to_owned())
            );
            assert_eq!(host.sent, [(0x22, vec![0xF1, 0x90])]);
            let mut host = Fake::answering(&[0x62, 0xF1, 0x00, 0x01, 0x02]);
            assert_eq!(
                resolve_source(ecu(3), &table, &mut host).unwrap(),
                Reading::Value(0x0102)
            );
        }

        #[test]
        fn a_source_named_twice_is_one_entry_and_services_nobody_names_are_left_out() {
            let mut program = named();
            program.identity.software_version = Some(ecu(1));
            let mut services = services();
            // A service the program does not name may be of any shape.
            services[1].request.as_mut().unwrap().sid = 0x19;
            program.identity.hardware_part_number = None;
            let table = ServiceSources::from_document(&document(services), &program).unwrap();
            assert!(table.get(1, 1).is_some() && table.get(3, 1).is_some());
            assert!(table.get(2, 1).is_none());
            // Nothing named: an empty table.
            assert_eq!(
                ServiceSources::from_document(&document(Vec::new()), &empty_program()),
                Ok(ServiceSources::default())
            );
        }

        #[test]
        fn a_source_the_document_does_not_declare_is_refused() {
            let mut services = services();
            services.pop();
            assert!(matches!(
                refused(services),
                TableError::Document {
                    service_id: 3,
                    field_id: 1,
                    ..
                }
            ));
            // A field the service does not have.
            let mut program = named();
            program.identity.vin = Some(Source::EcuService {
                service_id: 1,
                field_id: 2,
            });
            let got = ServiceSources::from_document(&document(self::services()), &program);
            assert!(
                matches!(got, Err(TableError::Document { field_id: 2, .. })),
                "{got:?}"
            );
        }

        #[test]
        fn a_read_of_several_identifiers_or_another_service_is_refused_by_the_table() {
            let mut multi = services();
            let two = Some(vec![0xF1, 0x90, 0xF1, 0x86]);
            multi[0].request.as_mut().unwrap().prefix = two.clone();
            multi[0].positive.as_mut().unwrap().prefix = two;
            assert!(matches!(
                refused(multi),
                TableError::Unsupported { service_id: 1, .. }
            ));
            let mut other = services();
            other[0].request.as_mut().unwrap().sid = 0x19;
            other[0].positive.as_mut().unwrap().sid = 0x59;
            assert!(matches!(
                refused(other),
                TableError::Unsupported { service_id: 1, .. }
            ));
        }

        #[test]
        fn a_field_that_is_not_fixed_aligned_and_plain_is_refused() {
            let edit = |change: &dyn Fn(&mut diag_ir::declaration::FieldT)| {
                let mut services = services();
                let positive = services[0].positive.as_mut().unwrap();
                change(&mut positive.fields.as_mut().unwrap()[0]);
                refused(services)
            };
            let is_document = |error: TableError| matches!(error, TableError::Document { .. });
            // Terminated or led by a length: the length is not known before the response.
            assert!(is_document(edit(
                &|f| f.length_kind = LengthKind::Terminated
            )));
            assert!(is_document(edit(
                &|f| f.length_kind = LengthKind::LeadingLength
            )));
            // Not on a byte edge, or not a whole number of bytes.
            assert!(is_document(edit(&|f| f.bit_offset = 4)));
            assert!(is_document(edit(&|f| f.bit_length = 17 * 8 - 4)));
            // Not text or an unsigned integer.
            assert!(is_document(edit(&|f| f.kind = DataKind::Bytes)));
            assert!(is_document(edit(&|f| f.kind = DataKind::Int)));
            assert!(is_document(edit(&|f| {
                f.kind = DataKind::Uint;
                f.byte_order = ByteOrder::Little;
            })));
            // An integer field is at most 7 bytes, the most the agent's integer holds (the
            // hardware part number may be unsigned); the table's own limit of 8 is never reached.
            let mut wide = services();
            let field = &mut wide[1].positive.as_mut().unwrap().fields.as_mut().unwrap()[0];
            field.kind = DataKind::Uint;
            field.bit_length = 9 * 8;
            assert!(matches!(refused(wide), TableError::Document { .. }));
        }

        #[test]
        fn a_field_with_a_conversion_is_read_raw() {
            use diag_ir::declaration::{CompuLinearT, CompuT};
            let mut linear = CompuLinearT::default();
            linear.numerator = 1.0;
            linear.denominator = 10.0;
            linear.offset = 5.0;
            let mut services = services();
            let positive = services[2].positive.as_mut().unwrap();
            positive.fields.as_mut().unwrap()[0].compu = CompuT::CompuLinear(Box::new(linear));
            let table = ServiceSources::from_document(&document(services), &named()).unwrap();
            let field = table.get(3, 1).unwrap();
            assert_eq!(field.encoding, Encoding::UnsignedBigEndian);
            assert_eq!((field.offset, field.length), (2, 2));
            // The raw bytes are the value; the conversion is not applied.
            let mut host = Fake::answering(&[0x62, 0xF1, 0x00, 0x01, 0x02]);
            assert_eq!(
                resolve_source(ecu(3), &table, &mut host).unwrap(),
                Reading::Value(0x0102)
            );
        }

        #[test]
        fn a_source_must_suit_the_role_that_reads_it() {
            let is_document = |got: Result<ServiceSources, TableError>, service_id: u32| {
                assert!(
                    matches!(&got, Err(TableError::Document { service_id: s, .. }) if *s == service_id),
                    "{got:?}"
                );
            };
            let build = |services: Vec<ServiceT>, program: &Program| {
                ServiceSources::from_document(&document(services), program)
            };
            // A VIN of 16 bytes, or an unsigned VIN.
            let mut short = services();
            short[0].positive.as_mut().unwrap().fields.as_mut().unwrap()[0].bit_length = 16 * 8;
            is_document(build(short, &named()), 1);
            let mut uint = services();
            uint[0].positive.as_mut().unwrap().fields.as_mut().unwrap()[0].kind = DataKind::Uint;
            uint[0].positive.as_mut().unwrap().fields.as_mut().unwrap()[0].bit_length = 17 * 8;
            is_document(build(uint, &named()), 1);
            // An ASCII precondition source, as the programming session's or the default's.
            let mut ascii = services();
            ascii[2].positive.as_mut().unwrap().fields.as_mut().unwrap()[0].kind = DataKind::Ascii;
            is_document(build(ascii.clone(), &named()), 3);
            let mut default_source = named();
            let engine = default_source.preconditions.engine.as_mut().unwrap();
            engine.default_session = Some(ecu(3));
            engine.programming_session = None;
            is_document(build(ascii, &default_source), 3);
            // A version may be ASCII or unsigned.
            let mut program = named();
            program.identity.hardware_part_number = Some(ecu(3));
            program.identity.software_version = Some(ecu(2));
            assert!(build(services(), &program).is_ok());
            // Shared between a precondition and a version: the version is ASCII, so the
            // precondition refuses it.
            let mut shared = named();
            shared.identity.software_version = Some(ecu(2));
            shared
                .preconditions
                .engine
                .as_mut()
                .unwrap()
                .programming_session = Some(ecu(2));
            is_document(build(services(), &shared), 2);
            // Shared and unsigned on both sides is fine.
            let mut both = named();
            both.identity.software_version = Some(ecu(3));
            assert!(build(services(), &both).is_ok());
            // The VIN may also be a version, but not a precondition.
            let mut vin_version = named();
            vin_version.identity.hardware_part_number = Some(ecu(1));
            assert!(build(services(), &vin_version).is_ok());
            let mut vin_precondition = named();
            vin_precondition
                .preconditions
                .engine
                .as_mut()
                .unwrap()
                .programming_session = Some(ecu(1));
            is_document(build(services(), &vin_precondition), 1);
            // An unsigned source of 8 bytes does not fit the agent's integer.
            let mut wide = services();
            wide[2].positive.as_mut().unwrap().fields.as_mut().unwrap()[0].bit_length = 8 * 8;
            is_document(build(wide, &named()), 3);
        }

        #[test]
        fn an_absolute_placement_is_read_at_its_byte_offset() {
            use diag_ir::declaration::Placement;
            let with_offset = |bit_offset: u32| {
                let mut services = services();
                let field = &mut services[0]
                    .positive
                    .as_mut()
                    .unwrap()
                    .fields
                    .as_mut()
                    .unwrap()[0];
                field.placement = Placement::Absolute;
                field.bit_offset = bit_offset;
                ServiceSources::from_document(&document(services), &named())
            };
            // SID, then the two identifier bytes, then the field: bit 24.
            let table = with_offset(24).unwrap();
            let field = table.get(1, 1).unwrap();
            assert_eq!((field.offset, field.length), (2, 17));
            // Any other place is not the table's shape.
            assert!(matches!(
                with_offset(32),
                Err(TableError::Unsupported { service_id: 1, .. })
            ));
            assert!(matches!(
                with_offset(16),
                Err(TableError::Unsupported { service_id: 1, .. })
            ));
        }

        /// The example ECU as the converter emits it: its VIN, part number and software
        /// version sources are read from their DID services at absolute places.
        #[test]
        fn the_converted_example_gives_the_three_identity_reads() {
            use diag_frontend::convert::{lower_procedures, to_ir};
            use diag_frontend::tables::{read_files, resolve};
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../diag-frontend/examples/example-ecu");
            let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
                .expect("the example directory")
                .map(|entry| {
                    let entry = entry.expect("an entry");
                    let name = entry.file_name().into_string().expect("a UTF-8 name");
                    // A Windows checkout may have CRLF line ends.
                    let text = std::fs::read_to_string(entry.path())
                        .expect("a text file")
                        .replace("\r\n", "\n");
                    (name, text.into_bytes())
                })
                .collect();
            files.push((
                "main.star".to_owned(),
                b"def main():\n    diag.log(1, \"a\")\n".to_vec(),
            ));
            files.sort();
            let definition = resolve(read_files(files).expect("reads")).expect("resolves");
            let procedures = lower_procedures(&definition).expect("lowers");
            let converted = to_ir(&definition, &procedures);
            assert!(!converted.documents.is_empty());
            for converted in converted.documents {
                let document = JobDocument::new(converted.bytes).expect("a document");
                let program = document.program().expect("a program");
                let table = ServiceSources::from_document(&document, &program).expect("a table");
                // All three identity rows of the example are fixed-length ASCII reads.
                let identity = &program.identity;
                for (source, did, length) in [
                    (identity.vin, 0xF190_u16, 17),
                    (identity.hardware_part_number, 0xF187, 11),
                    (identity.software_version, 0xF189, 5),
                ] {
                    let source = source.expect("the example names the source");
                    let Source::EcuService {
                        service_id,
                        field_id,
                    } = source
                    else {
                        panic!("{source:?}");
                    };
                    let field = table.get(service_id, field_id).expect("the entry");
                    let [hi, lo] = did.to_be_bytes();
                    assert_eq!(field.request, [0x22, hi, lo]);
                    assert_eq!((field.offset, field.length), (2, length));
                    assert_eq!(field.encoding, Encoding::Ascii);
                }
            }
        }

        #[test]
        fn a_response_that_has_more_than_the_field_or_does_not_echo_is_refused() {
            // The field is not the last of the response.
            let mut services = services();
            let positive = services[0].positive.as_mut().unwrap();
            let mut more = positive.fields.as_ref().unwrap()[0].clone();
            more.id = 2;
            positive.fields.as_mut().unwrap().push(more);
            assert!(matches!(
                refused(services),
                TableError::Document { service_id: 1, .. }
            ));
            // The response prefix is not the request's identifier.
            let mut services = self::services();
            services[0].positive.as_mut().unwrap().prefix = Some(vec![0xF1, 0x86]);
            assert!(matches!(
                refused(services),
                TableError::Document { service_id: 1, .. }
            ));
            // Two fields of the source's id: the VM decodes the first, so a later one that
            // would fit the table is not taken in its place.
            let mut services = self::services();
            let fields = services[0]
                .positive
                .as_mut()
                .unwrap()
                .fields
                .as_mut()
                .unwrap();
            let mut later = fields[0].clone();
            later.placement = diag_ir::declaration::Placement::Absolute;
            later.bit_offset = 24;
            fields[0].bit_length = 8;
            fields.push(later);
            assert!(matches!(
                refused(services),
                TableError::Document { service_id: 1, .. }
            ));
            // A request that takes a parameter has no value to send.
            let mut services = self::services();
            let mut param = diag_ir::declaration::FieldT::default();
            param.id = 1;
            param.short_name = Some("p".into());
            param.bit_length = 8;
            services[0].request.as_mut().unwrap().params = Some(vec![param]);
            assert!(matches!(
                refused(services),
                TableError::Document { service_id: 1, .. }
            ));
        }
    }
}
