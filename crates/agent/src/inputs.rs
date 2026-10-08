//! What an interrupted-write restart reads from the vehicle (design 8.2.5, 8.9; ADR-229,
//! ADR-245 item 2).
//!
//! A procedure declares a [`Source`] for each identity value and each precondition. This module
//! turns a source into a [`Reading`]:
//!
//! - [`Source::RuntimeInput`]: a fact the agent supplies itself, through [`RuntimeInputs`].
//!   [`crate::WorkerHost`] answers the supply voltage from the VCI (`PDU_IOCTL_READ_VBATT`) and
//!   reports every other input as [`Reading::CannotBeEstablished`], since no current VCI offers
//!   a source for them (ADR-238). [`FixedInputs`] holds fixed readings, for tests and for later
//!   wiring.
//! - [`Source::EcuService`]: a field of a diagnostic response, located through a
//!   [`ServiceSources`] table. The table stands in until the declaration part has a decoder
//!   (ADR-245, consequences).
//!
//! "Cannot be established" and "cannot be decoded" are readings, not errors. A caller treats
//! either as a failed check (ADR-229). Only a failure of the transport is an error
//! ([`HostError`]).

use std::collections::HashMap;
use std::time::Duration;

use diag_ir::{DiagHost, RuntimeInput, Source};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use vci_service_interface::{
    DataItem, GetObjectIdRequest, IoCtlRequest, ObjectType, data_item, io_ctl_request,
};
use worker_host::client::WorkerClient;

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
    /// Reads `input`. An input without a source gives [`Reading::CannotBeEstablished`]; only
    /// a transport failure is an error.
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

/// Reads the supply voltage from the VCI of `link`. The IOCTL id is resolved by name on the
/// first call and kept in `cached_id`. When the J2534 worker does not know the name, the VCI
/// offers no voltage: [`Reading::CannotBeEstablished`]. A D-PDU worker reports an unknown name
/// as an internal error, which stays a [`HostError`] (it fails the check all the same). Blocks
/// on `handle`, like the host's other primitives.
pub(crate) fn read_supply_voltage(
    handle: &Handle,
    client: &mut WorkerClient,
    link: &Link,
    cached_id: &mut Option<u32>,
    deadline: Duration,
) -> Result<Reading, HostError> {
    let id = match *cached_id {
        Some(id) => id,
        None => {
            let request = GetObjectIdRequest {
                object_type: ObjectType::ObjtIoCtrl as i32,
                shortname: IOCTL_READ_VBATT_NAME.to_owned(),
            };
            let found = handle.block_on(unary(
                deadline,
                "GetObjectId",
                client.get_object_id(request),
            ));
            match found {
                Ok(response) => *cached_id.insert(response.pdu_object_id),
                Err(HostError::Rpc { status, .. })
                    if matches!(
                        status.code(),
                        tonic::Code::NotFound | tonic::Code::InvalidArgument
                    ) =>
                {
                    return Ok(Reading::CannotBeEstablished);
                }
                Err(error) => return Err(error),
            }
        }
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

    pub fn get(&self, service_id: u32, field_id: u32) -> Option<&ServiceField> {
        self.fields
            .iter()
            .find(|f| f.service_id == service_id && f.field_id == field_id)
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

/// Extracts `field` from `response` to the request with service `sid`.
fn decode_field(field: &ServiceField, sid: u8, response: &[u8]) -> Reading {
    // A positive response carries the SID plus 0x40 (ISO 14229-1:2026 clause 7.4); a negative
    // one starts with 0x7F.
    let Some((&first, rest)) = response.split_first() else {
        return Reading::CannotBeDecoded;
    };
    if first != sid.wrapping_add(0x40) {
        return Reading::CannotBeDecoded;
    }
    let params = &field.request[1..];
    // Exactly the echo, then the field: nothing missing, nothing extra.
    if field.offset != params.len()
        || field.offset.checked_add(field.length) != Some(rest.len())
        || rest[..field.offset] != *params
    {
        return Reading::CannotBeDecoded;
    }
    let bytes = &rest[field.offset..];
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
    }

    impl Fake {
        fn answering(response: &[u8]) -> Self {
            Self {
                responses: vec![Ok(response.to_vec())],
                sent: Vec::new(),
                inputs: FixedInputs::new(),
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
        fn log(&mut self, _: u8, _: &str) {}
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
    fn worker_host_resolves_any_source(host: &mut crate::WorkerHost, source: Source) {
        let _ = resolve_source(source, &ServiceSources::default(), host);
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
}
