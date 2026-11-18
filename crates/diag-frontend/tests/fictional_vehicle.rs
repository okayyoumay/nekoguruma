//! The fictional vehicle of `examples/fictional-vehicle` (its `README.md`): every ECU definition
//! converts with the shared `identify.star`, the ECUs keep their own CAN identifiers and agree on
//! the server's text and unit ids, and against `sim-ecu` configured from `sim/` each ECU is
//! identified as its expected variant and the procedure runs to its end. The engine controller's
//! coding DID is declared with its write conditions, and its simulator reads it, refuses a write
//! outside the extended session, without security access or without gateway authentication, and
//! writes it once all three hold.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use diag_frontend::convert::{Converted, lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::{EcuVariant, load_ecu_document};
use diag_ir::decode::{DecodedField, PhysicalValue, decode_positive_response, select_variant};
use diag_ir::dry_run::{DryRunEnd, DryRunEvent, DryRunReport, Script, dry_run};
use diag_ir::encode::{ParamValue, encode_request};
use diag_ir::{Failure, Interruptible, Op, Program};
use sim_ecu::{EcuConfig, Nrc, SimEcu, SimResponse};

const MAX_STEPS: u64 = 100_000;

/// Each ECU's directory, its physical request and response identifiers, and the variant its
/// `sim/` configuration is identified as.
const ECUS: [(&str, u32, u32, &str); 7] = [
    ("cgw", 0x710, 0x718, "cgwV1"),
    ("ecm", 0x7E0, 0x7E8, "ecmV1"),
    ("tcm", 0x7E1, 0x7E9, "tcmAt6V1"),
    ("abs", 0x760, 0x768, "absEscV1"),
    ("eps", 0x730, 0x738, "epsV1"),
    ("bcm", 0x740, 0x748, "bcmPremium"),
    ("ipc", 0x720, 0x728, "ipcV1"),
];

const FUNCTIONAL_ID: u32 = 0x7DF;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

fn vehicle() -> PathBuf {
    examples().join("fictional-vehicle")
}

/// A text file with its line endings made `\n`, so a Windows checkout reads the same bytes.
fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The definition in `dir`, with `procedure` (a file of the vehicle's directory) when given.
fn convert(dir: &Path, procedure: Option<&str>) -> Converted {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, read_text(&entry.path()).into_bytes())
        })
        .collect();
    if let Some(procedure) = procedure {
        files.push((
            procedure.to_owned(),
            read_text(&vehicle().join(procedure)).into_bytes(),
        ));
    }
    files.sort();
    let tables = read_files(files).unwrap_or_else(|e| panic!("{}: {e:?}", dir.display()));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("{}: {e:?}", dir.display()));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("{}: {e:?}", dir.display()));
    to_ir(&definition, &procedures)
}

fn convert_ecu(ecu: &str) -> Converted {
    convert(&vehicle().join(ecu), Some("identify.star"))
}

fn sim_config(ecu: &str) -> EcuConfig {
    let path = vehicle().join("sim").join(format!("{ecu}.json"));
    let config: EcuConfig = serde_json::from_str(&read_text(&path))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    config
        .validate()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    config
}

/// The positive response of `ecu` to `request`; panics on any other answer.
fn answer(ecu: &mut SimEcu, request: &[u8]) -> Vec<u8> {
    let response = ecu.request(request);
    match response.to_bytes() {
        Some(bytes) if bytes.first() == Some(&(request[0] + 0x40)) => bytes,
        _ => panic!("{request:02X?} was answered with {response:?}"),
    }
}

/// The index of the variant that `ecu` is identified as among `documents`: each variant's
/// pattern services are sent to it and decoded through that variant's own declarations, as the
/// server's identification run does (design 8.9.1, `diag_ir::identify`).
fn identify(documents: &[&[u8]], ecu: &mut SimEcu) -> Option<usize> {
    let loaded: Vec<_> = documents
        .iter()
        .map(|bytes| load_ecu_document(bytes).expect("the document loads"))
        .collect();
    let variants: Vec<EcuVariant<'_>> = loaded
        .iter()
        .map(|document| document.variant().expect("a variant"))
        .collect();
    let mut decoded: HashMap<(usize, u32), Vec<DecodedField>> = HashMap::new();
    for (index, variant) in variants.iter().enumerate() {
        let services: HashSet<u32> = variant
            .patterns()
            .into_iter()
            .flatten()
            .map(|pattern| pattern.service_id())
            .collect();
        for service in variant.services().into_iter().flatten() {
            if !services.contains(&service.id()) {
                continue;
            }
            let request = service.request().expect("a request");
            let mut message = vec![request.sid()];
            message.extend(encode_request(&request, &HashMap::new()).expect("a constant request"));
            let bytes = answer(ecu, &message);
            let positive = service.positive().expect("a positive response");
            let fields = decode_positive_response(&positive, variant.layouts(), &bytes)
                .unwrap_or_else(|e| panic!("{message:02X?}: {e:?}"));
            decoded.insert((index, service.id()), fields);
        }
    }
    select_variant(variants, |index, service| {
        decoded.get(&(index, service)).map(Vec::as_slice)
    })
}

/// The documents of `converted` by variant name.
fn by_variant(converted: &Converted) -> BTreeMap<&str, &[u8]> {
    converted
        .documents
        .iter()
        .map(|document| (document.name.as_str(), document.bytes.as_slice()))
        .collect()
}

/// The name of the variant `config` is identified as among the documents of `converted`.
fn identified_name(converted: &Converted, config: EcuConfig) -> String {
    let documents: Vec<&[u8]> = converted
        .documents
        .iter()
        .map(|document| document.bytes.as_slice())
        .collect();
    let index = identify(&documents, &mut SimEcu::new(config)).expect("a variant is identified");
    converted.documents[index].name.clone()
}

#[test]
fn every_ecu_converts_with_the_identification_procedure() {
    for (ecu, ..) in ECUS {
        let converted = convert_ecu(ecu);
        assert!(!converted.documents.is_empty(), "{ecu}");
        for document in &converted.documents {
            assert_eq!(
                document.procedure.as_deref(),
                Some("identify.star"),
                "{ecu} {}",
                document.name
            );
        }
    }
}

#[test]
fn each_ecu_has_its_own_identifiers_and_variant_ids() {
    let mut can_ids = HashSet::new();
    let mut variant_ids = HashSet::new();
    for (ecu, tx, rx, _) in ECUS {
        let documents = convert_ecu(ecu).documents;
        let ids: HashSet<u32> = documents.iter().map(|document| document.id).collect();
        for id in ids {
            assert!(
                variant_ids.insert(id),
                "{ecu}: variant id {id} is used twice"
            );
        }
        for document in documents {
            let loaded = load_ecu_document(&document.bytes).expect("the document loads");
            let variant = loaded.variant().expect("a variant");
            let protocols: Vec<_> = variant.protocols().into_iter().flatten().collect();
            assert_eq!(protocols.len(), 1, "{ecu}");
            let protocol = protocols[0];
            assert_eq!(
                (protocol.tx_id(), protocol.rx_id(), protocol.functional_id()),
                (tx, rx, FUNCTIONAL_ID),
                "{ecu}"
            );
        }
        assert!(
            can_ids.insert(tx),
            "{ecu}: request id {tx:#X} is used twice"
        );
        assert!(
            can_ids.insert(rx),
            "{ecu}: response id {rx:#X} is used twice"
        );
    }
}

#[test]
fn the_ecus_agree_on_the_text_and_unit_ids() {
    // The ids are the server's, shared by every definition it holds (csv-tables.md 4.4), so one
    // id must give one text across the vehicle and the example ECU.
    let mut texts: BTreeMap<u32, (String, String)> = BTreeMap::new();
    let mut units: BTreeMap<u32, (String, String)> = BTreeMap::new();
    let mut definitions: Vec<(String, Converted)> = ECUS
        .iter()
        .map(|(ecu, ..)| (ecu.to_string(), convert_ecu(ecu)))
        .collect();
    definitions.push((
        "example-ecu".to_owned(),
        convert(&examples().join("example-ecu"), None),
    ));
    for (name, converted) in &definitions {
        for (catalog, entries) in [
            (&mut texts, &converted.texts),
            (&mut units, &converted.units),
        ] {
            for entry in entries {
                let (first, text) = catalog
                    .entry(entry.id)
                    .or_insert_with(|| (name.clone(), entry.text.clone()));
                assert_eq!(
                    text, &entry.text,
                    "id {} is {text:?} in {first} but {:?} in {name}",
                    entry.id, entry.text
                );
            }
        }
    }
}

#[test]
fn each_simulated_ecu_is_identified_as_its_variant() {
    for (ecu, _, _, expected) in ECUS {
        let converted = convert_ecu(ecu);
        assert_eq!(
            identified_name(&converted, sim_config(ecu)),
            expected,
            "{ecu}"
        );
    }
}

#[test]
fn the_other_variants_are_identified_too() {
    let ecm = convert_ecu("ecm");
    let mut config = sim_config("ecm");
    config.sw_version = "2.0.0".to_owned();
    assert_eq!(identified_name(&ecm, config), "ecmV2");

    let bcm = convert_ecu("bcm");
    let mut config = sim_config("bcm");
    config.part_number = "MK-BCM-0001".to_owned();
    assert_eq!(identified_name(&bcm, config), "bcmStandard");
}

#[test]
fn the_identification_procedure_runs_against_each_simulated_ecu() {
    for (ecu, _, _, variant) in ECUS {
        let config = sim_config(ecu);
        let stored: Vec<(u32, u8)> = config
            .dtcs
            .iter()
            .map(|record| (record.dtc, record.status))
            .collect();
        let vin = config.vin.clone();
        let mut sim = SimEcu::new(config);
        let script = Script::new()
            .response("activeSession.read", answer(&mut sim, &[0x22, 0xF1, 0x86]))
            .response(
                "sparePartNumber.read",
                answer(&mut sim, &[0x22, 0xF1, 0x87]),
            )
            .response(
                "softwareVersion.read",
                answer(&mut sim, &[0x22, 0xF1, 0x89]),
            )
            .response("vin.read", answer(&mut sim, &[0x22, 0xF1, 0x90]))
            .response("readDtcByStatusMask", answer(&mut sim, &[0x19, 0x02, 0x08]));

        let converted = convert_ecu(ecu);
        let document = by_variant(&converted)[variant];
        let loaded = load_ecu_document(document).expect("the document loads");
        let program = postcard::from_bytes(loaded.procedure().expect("a procedure").bytes())
            .expect("a program");
        let report = dry_run(
            &program,
            &loaded.variant().expect("a variant"),
            &script,
            MAX_STEPS,
        );
        assert_eq!(report.end, DryRunEnd::Finished, "{ecu}: {report:#?}");

        let requests: Vec<&str> = report
            .events
            .iter()
            .filter_map(|event| match event {
                DryRunEvent::Request { service, .. } => Some(service.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            requests,
            [
                "activeSession.read",
                "sparePartNumber.read",
                "softwareVersion.read",
                "vin.read",
                "readDtcByStatusMask",
            ],
            "{ecu}"
        );
        let decoded = |index: usize, name: &str| -> PhysicalValue {
            let Some(DryRunEvent::Request { decoded, .. }) = report
                .events
                .iter()
                .filter(|event| matches!(event, DryRunEvent::Request { .. }))
                .nth(index)
            else {
                panic!("{ecu}: no request {index}");
            };
            let fields = decoded
                .as_ref()
                .expect("a positive response is declared")
                .as_ref()
                .expect("the answer decodes");
            fields
                .iter()
                .find(|field| field.short_name == name)
                .unwrap_or_else(|| panic!("{ecu}: no field {name}"))
                .value
                .clone()
        };
        assert_eq!(decoded(3, "vin"), PhysicalValue::Text(vin), "{ecu}");
        // The confirmed DTCs (status bit 3) that the simulated ECU stores.
        let confirmed = stored
            .iter()
            .filter(|(_, status)| status & 0x08 != 0)
            .count();
        let PhysicalValue::Repeat(records) = decoded(4, "records") else {
            panic!("{ecu}: the DTC records are not a list");
        };
        assert_eq!(records.len(), confirmed, "{ecu}");
    }
}

// ------------------------------------------------------------------------- engine coding

/// The ECM's coding DID (README 6.3) and the key mask of README 7.
const CODING_DID: [u8; 2] = [0x01, 0x20];
const ECM_KEY_MASK: u32 = 0x5A5A_5A5A;

/// The ECM's DIDs the sim file declares besides the coding DID: the DID, the name of its
/// definition and the physical value its record decodes to.
const ECM_DIDS: [(u16, &str, f64); 7] = [
    (0x1000, "engineSpeed", 0.0),
    (0x1001, "coolantTemperature", 25.0),
    (0x1002, "intakeAirTemperature", 20.0),
    (0x1003, "vehicleSpeed", 0.0),
    (0x1004, "throttlePosition", 5.0 * 100.0 / 255.0),
    (0x1005, "batteryVoltage", 12.6),
    (0x1006, "engineRunning", 0.0),
];

fn write_coding(record: [u8; 4]) -> Vec<u8> {
    let mut request = vec![0x2E];
    request.extend_from_slice(&CODING_DID);
    request.extend_from_slice(&record);
    request
}

/// The NRC of `ecu`'s negative answer to `request`; panics on any other answer.
fn refusal(ecu: &mut SimEcu, request: &[u8]) -> Nrc {
    match ecu.request(request) {
        SimResponse::Negative { sid, nrc } => {
            assert_eq!(sid, request[0], "{request:02X?}");
            nrc
        }
        other => panic!("{request:02X?} was answered with {other:?}"),
    }
}

/// Opens the extended session and unlocks security level 1 with the ECM's mask.
fn unlock(ecu: &mut SimEcu) {
    unlock_in(ecu, 0x03);
}

/// Enters `session` and unlocks security access level 1 there.
fn unlock_in(ecu: &mut SimEcu, session: u8) {
    answer(ecu, &[0x10, session]);
    let seed = answer(ecu, &[0x27, 0x01]);
    assert_eq!(seed.len(), 6, "{seed:02X?}");
    let seed = u32::from_be_bytes(seed[2..6].try_into().expect("four bytes"));
    let mut request = vec![0x27, 0x02];
    request.extend_from_slice(&(seed ^ ECM_KEY_MASK).to_be_bytes());
    answer(ecu, &request);
}

/// A simulated ECM that the gateway has authenticated.
fn authenticated_ecm() -> SimEcu {
    let mut ecu = SimEcu::new(sim_config("ecm"));
    ecu.gateway_authenticated = true;
    ecu
}

fn read_coding(ecu: &mut SimEcu) -> Vec<u8> {
    let response = answer(ecu, &[0x22, 0x01, 0x20]);
    assert_eq!(response[..3], [0x62, 0x01, 0x20]);
    response[3..].to_vec()
}

/// The service `name` of the only variant of `document`, as its request and response.
fn load_service<'a>(
    loaded: &'a diag_ir::declaration::EcuDocument<'a>,
    name: &str,
) -> diag_ir::declaration::Service<'a> {
    loaded
        .variant()
        .expect("a variant")
        .services()
        .into_iter()
        .flatten()
        .find(|service| service.short_name() == Some(name))
        .unwrap_or_else(|| panic!("no service {name}"))
}

#[test]
fn both_ecm_variants_declare_the_coding_did_with_its_write_conditions() {
    let converted = convert_ecu("ecm");
    let documents = by_variant(&converted);
    assert_eq!(
        documents.keys().copied().collect::<Vec<_>>(),
        ["ecmV1", "ecmV2"]
    );
    for (variant, bytes) in documents {
        let loaded = load_ecu_document(bytes).expect("the document loads");
        let read = load_service(&loaded, "engineCoding.read");
        let write = load_service(&loaded, "engineCoding.write");
        // Readable in every session; writable in the extended session at security level 1.
        assert_eq!(
            (read.required_session(), read.required_security_level()),
            (0, 0),
            "{variant}"
        );
        assert_eq!(
            (write.required_session(), write.required_security_level()),
            (0x03, 0x01),
            "{variant}"
        );
        // The record: market, option bits, target idle speed (4 bytes, big endian).
        let values = HashMap::from([
            ("market", ParamValue::Int(1)),
            ("options", ParamValue::Int(3)),
            ("targetIdleSpeed", ParamValue::Int(900)),
        ]);
        let request = write.request().expect("a request");
        let mut message = vec![request.sid()];
        message.extend(encode_request(&request, &values).expect("the record encodes"));
        assert_eq!(message, write_coding([0x01, 0x03, 0x03, 0x84]), "{variant}");
        let positive = read.positive().expect("a positive response");
        let layouts = loaded.variant().expect("a variant").layouts();
        let fields = decode_positive_response(
            &positive,
            layouts,
            &[0x62, 0x01, 0x20, 0x00, 0x01, 0x03, 0x20],
        )
        .expect("the record decodes");
        let value = |name: &str| {
            fields
                .iter()
                .find(|field| field.short_name == name)
                .unwrap_or_else(|| panic!("{variant}: no field {name}"))
        };
        assert_eq!(
            value("market").value,
            PhysicalValue::Unsigned(0),
            "{variant}"
        );
        assert_ne!(value("market").text_id, 0, "{variant}: market has a text");
        assert_eq!(
            value("options").value,
            PhysicalValue::Unsigned(1),
            "{variant}"
        );
        assert_eq!(
            value("targetIdleSpeed").value,
            PhysicalValue::Unsigned(800),
            "{variant}"
        );
    }
}

#[test]
fn the_coding_did_reads_in_every_session_without_authentication() {
    let mut ecu = SimEcu::new(sim_config("ecm"));
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
    answer(&mut ecu, &[0x10, 0x03]);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
    answer(&mut ecu, &[0x10, 0x02]);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
}

#[test]
fn a_coding_write_is_refused_outside_the_extended_session() {
    let mut ecu = authenticated_ecm();
    let request = write_coding([0x01, 0x03, 0x03, 0x84]);
    assert_eq!(refusal(&mut ecu, &request), Nrc::RequestOutOfRange);
    answer(&mut ecu, &[0x10, 0x02]);
    assert_eq!(refusal(&mut ecu, &request), Nrc::RequestOutOfRange);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
    // Unlocked in the programming session, the write is still refused for its session, not for
    // security access.
    let mut ecu = authenticated_ecm();
    unlock_in(&mut ecu, 0x02);
    assert_eq!(refusal(&mut ecu, &request), Nrc::RequestOutOfRange);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
}

#[test]
fn a_coding_write_is_refused_in_the_extended_session_without_security_access() {
    let mut ecu = authenticated_ecm();
    answer(&mut ecu, &[0x10, 0x03]);
    let request = write_coding([0x01, 0x03, 0x03, 0x84]);
    assert_eq!(refusal(&mut ecu, &request), Nrc::SecurityAccessDenied);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
}

#[test]
fn a_coding_write_is_refused_without_gateway_authentication() {
    let mut ecu = SimEcu::new(sim_config("ecm"));
    unlock(&mut ecu);
    let request = write_coding([0x01, 0x03, 0x03, 0x84]);
    assert_eq!(refusal(&mut ecu, &request), Nrc::AuthenticationRequired);
    assert_eq!(read_coding(&mut ecu), [0x00, 0x01, 0x03, 0x20]);
    // The same request goes through once the gateway has authenticated.
    ecu.gateway_authenticated = true;
    answer(&mut ecu, &request);
}

#[test]
fn a_coding_write_succeeds_when_authenticated_unlocked_and_in_the_extended_session() {
    let mut ecu = authenticated_ecm();
    unlock(&mut ecu);
    assert_eq!(
        answer(&mut ecu, &write_coding([0x01, 0x03, 0x03, 0x84])),
        [0x6E, 0x01, 0x20]
    );
    assert_eq!(read_coding(&mut ecu), [0x01, 0x03, 0x03, 0x84]);
    // A record of the wrong length is refused and leaves the coding as written.
    assert_eq!(
        refusal(&mut ecu, &[0x2E, 0x01, 0x20, 0x00, 0x00, 0x00]),
        Nrc::IncorrectMessageLengthOrInvalidFormat
    );
    assert_eq!(read_coding(&mut ecu), [0x01, 0x03, 0x03, 0x84]);
}

#[test]
fn each_configured_ecm_did_reads_back_the_value_the_sim_file_declares() {
    let config = sim_config("ecm");
    // The file declares these DIDs and the coding DID, and no other (1010 is `ecmV2` only).
    let mut declared: Vec<u16> = config.dids.iter().map(|did| did.did).collect();
    declared.sort_unstable();
    let mut expected: Vec<u16> = ECM_DIDS.iter().map(|(did, ..)| *did).collect();
    expected.push(0x0120);
    expected.sort_unstable();
    assert_eq!(declared, expected);
    assert_eq!(config.security_key_mask, Some(ECM_KEY_MASK));
    assert!(config.require_gateway_auth);

    let converted = convert_ecu("ecm");
    let bytes = by_variant(&converted)["ecmV1"];
    let loaded = load_ecu_document(bytes).expect("the document loads");
    let layouts = loaded.variant().expect("a variant").layouts();
    let mut ecu = SimEcu::new(config.clone());
    for (did, name, physical) in ECM_DIDS {
        let did_bytes = did.to_be_bytes();
        let response = answer(&mut ecu, &[0x22, did_bytes[0], did_bytes[1]]);
        let declared = config
            .dids
            .iter()
            .find(|entry| entry.did == did)
            .expect("declared");
        assert_eq!(response[3..], declared.value[..], "{name}");
        let service = load_service(&loaded, &format!("{name}.read"));
        let fields = decode_positive_response(
            &service.positive().expect("a positive response"),
            layouts,
            &response,
        )
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let value = match &fields[0].value {
            PhysicalValue::Unsigned(v) => *v as f64,
            PhysicalValue::Float(v) => *v,
            other => panic!("{name}: {other:?}"),
        };
        assert!((value - physical).abs() < 1e-9, "{name}: {value}");
    }
}

// ------------------------------------------------------------------------- write-coding.star

/// The configuration-change procedure for the ECM (README 11), attached to the ECM here only.
const WRITE_CODING: &str = "write-coding.star";

fn convert_coding() -> Converted {
    convert(&vehicle().join("ecm"), Some(WRITE_CODING))
}

/// The ECM's coding record as the read service answers it: market 0, `options`, idle speed 800.
fn coding_read(options: u8) -> Vec<u8> {
    vec![0x62, 0x01, 0x20, 0x00, options, 0x03, 0x20]
}

/// The write service's request for the record `coding_read(options)` stands for.
fn coding_write(options: u8) -> Vec<u8> {
    write_coding([0x00, options, 0x03, 0x20])
}

const WRITE_OK: [u8; 3] = [0x6E, 0x01, 0x20];

/// A negative answer to the request with SID `sid`.
fn negative(sid: u8, nrc: u8) -> Vec<u8> {
    vec![0x7F, sid, nrc]
}

/// The answers a dry run of the procedure is scripted with: one list per service name, each
/// repeating its last answer. The three coding reads share `engineCoding.read` and the two writes
/// `engineCoding.write`, so the lists are in the order the procedure sends them.
struct Answers {
    reads: Vec<Vec<u8>>,
    writes: Vec<Vec<u8>>,
    session: Vec<u8>,
    seed: Vec<u8>,
    key: Vec<u8>,
}

impl Answers {
    fn new(reads: &[Vec<u8>], writes: &[Vec<u8>]) -> Self {
        Self {
            reads: reads.to_vec(),
            writes: writes.to_vec(),
            session: vec![0x50, 0x03, 0x00, 0x32, 0x01, 0xF4],
            seed: vec![0x67, 0x01, 0x12, 0x34, 0x56, 0x78],
            key: vec![0x67, 0x02],
        }
    }

    fn script(&self) -> Script {
        let mut script = Script::new()
            .response("extendedSession", self.session.clone())
            .response("requestSeed", self.seed.clone())
            .response("sendKey", self.key.clone());
        for read in &self.reads {
            script = script.response("engineCoding.read", read.clone());
        }
        for write in &self.writes {
            script = script.response("engineCoding.write", write.clone());
        }
        script
    }
}

/// The program of `variant` among `converted`'s documents.
fn coding_program(converted: &Converted, variant: &str) -> Program {
    let loaded = load_ecu_document(by_variant(converted)[variant]).expect("the document loads");
    postcard::from_bytes(loaded.procedure().expect("a procedure").bytes()).expect("a program")
}

/// The dry run of `variant`'s program with `answers`, within the step limit.
fn run_coding(variant: &str, answers: &Answers) -> DryRunReport {
    let converted = convert_coding();
    let loaded = load_ecu_document(by_variant(&converted)[variant]).expect("the document loads");
    let program: Program =
        postcard::from_bytes(loaded.procedure().expect("a procedure").bytes()).expect("a program");
    dry_run(
        &program,
        &loaded.variant().expect("a variant"),
        &answers.script(),
        MAX_STEPS,
    )
}

/// Every request the run sent, whole.
fn sent(report: &DryRunReport) -> Vec<Vec<u8>> {
    report
        .events
        .iter()
        .filter_map(|event| match event {
            DryRunEvent::Request { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn failed_with(report: &DryRunReport) -> &str {
    match &report.end {
        DryRunEnd::Failed(Failure { code, .. }) => code,
        other => panic!("the run did not fail: {other:?}\n{report:#?}"),
    }
}

/// The requests of a run that unlocks and writes: the first read, the session, the seed and the
/// key (the seed of `Answers::new` xor the mask), in the order the procedure sends them.
fn unlock_requests() -> Vec<Vec<u8>> {
    vec![
        vec![0x22, 0x01, 0x20],
        vec![0x10, 0x03],
        vec![0x27, 0x01],
        vec![0x27, 0x02, 0x48, 0x6E, 0x0C, 0x22],
    ]
}

fn with(mut head: Vec<Vec<u8>>, tail: &[Vec<u8>]) -> Vec<Vec<u8>> {
    head.extend_from_slice(tail);
    head
}

fn read_request() -> Vec<u8> {
    vec![0x22, 0x01, 0x20]
}

/// Both ECM variants, since the one procedure is lowered for each.
const ECM_VARIANTS: [&str; 2] = ["ecmV1", "ecmV2"];

#[test]
fn the_coding_procedure_converts_for_both_ecm_variants() {
    let converted = convert_coding();
    assert_eq!(
        by_variant(&converted).keys().copied().collect::<Vec<_>>(),
        ECM_VARIANTS
    );
    for document in &converted.documents {
        assert_eq!(
            document.procedure.as_deref(),
            Some(WRITE_CODING),
            "{}",
            document.name
        );
    }
}

/// Per instruction, whether it lies in a section that holds a cancel.
fn held(program: &Program) -> Vec<bool> {
    let mut held = vec![false; program.code.len()];
    for section in &program.sections {
        if matches!(section.interruptible, Interruptible::Yes) {
            continue;
        }
        for at in held
            .iter_mut()
            .take(section.end_pc as usize)
            .skip(section.start_pc as usize)
        {
            *at = true;
        }
    }
    held
}

#[test]
fn the_coding_program_writes_and_decides_inside_one_uninterruptible_section() {
    let converted = convert_coding();
    for variant in ECM_VARIANTS {
        let program = coding_program(&converted, variant);
        let held = held(&program);
        let (mut writes, mut post_write_fails, mut calls) = (0, 0, 0);
        let mut first_write = None;
        for (pc, op) in program.code.iter().enumerate() {
            match op {
                Op::SecurityAccess { .. } => panic!("{variant}: the program uses SecurityAccess"),
                Op::ServiceRequest { service } => {
                    let sid = *service;
                    if sid == 0x2E {
                        writes += 1;
                        first_write.get_or_insert(pc);
                        assert!(held[pc], "{variant}: the write at {pc} is interruptible");
                    }
                }
                Op::Fail { code } => {
                    let code = std::str::from_utf8(&program.constants[*code as usize])
                        .expect("a UTF-8 code");
                    if matches!(
                        code,
                        "WRITE_REFUSED"
                            | "CODING_ROLLED_BACK"
                            | "ROLLBACK_REFUSED"
                            | "ROLLBACK_UNCONFIRMED"
                    ) {
                        post_write_fails += 1;
                        assert!(held[pc], "{variant}: the fail {code} is interruptible");
                    }
                }
                Op::Call(target) => {
                    // The call into the section: the one whose target is held.
                    if held[*target as usize] && !held[pc] {
                        calls += 1;
                        // Main's last statement: a return follows it at once.
                        let next = program.code.get(pc + 1);
                        assert!(
                            matches!(next, Some(Op::Ret)),
                            "{variant}: the section call is followed by {next:?}"
                        );
                    }
                }
                _ => {}
            }
        }
        // The write and the roll-back, four outcomes that fail after a write, one entry.
        assert_eq!(writes, 2, "{variant}");
        assert_eq!(post_write_fails, 4, "{variant}");
        assert_eq!(calls, 1, "{variant}");
    }
}

#[test]
fn the_ecm_tables_declare_the_seed_and_key_services() {
    let converted = convert_coding();
    for variant in ECM_VARIANTS {
        let loaded = load_ecu_document(by_variant(&converted)[variant]).expect("loads");
        let layouts = loaded.variant().expect("a variant").layouts();
        let seed = load_service(&loaded, "requestSeed");
        let key = load_service(&loaded, "sendKey");
        let values = HashMap::from([("key", ParamValue::Int(0x486E_0C22))]);
        let mut message = vec![0x27];
        message.extend(encode_request(&key.request().expect("a request"), &values).expect("key"));
        assert_eq!(message, [0x27, 0x02, 0x48, 0x6E, 0x0C, 0x22], "{variant}");
        let fields = decode_positive_response(
            &seed.positive().expect("a positive response"),
            layouts,
            &[0x67, 0x01, 0x12, 0x34, 0x56, 0x78],
        )
        .expect("the seed decodes");
        let seed_field = fields
            .iter()
            .find(|f| f.short_name == "seed")
            .expect("seed");
        assert_eq!(seed_field.value, PhysicalValue::Unsigned(0x1234_5678));
    }
}

// P rows: before the write.

#[test]
fn p1_a_coding_with_cruise_control_finishes_with_only_the_read_sent() {
    for variant in ECM_VARIANTS {
        // Bit 1 set, among other bits.
        for options in [0x02, 0x03, 0x07] {
            let answers = Answers::new(&[coding_read(options)], &[]);
            let report = run_coding(variant, &answers);
            assert_eq!(report.end, DryRunEnd::Finished, "{variant}: {report:#?}");
            assert_eq!(sent(&report), [read_request()], "{variant} {options:#X}");
        }
    }
}

#[test]
fn p2_a_refused_first_read_fails_with_read_coding_and_sends_nothing_more() {
    for variant in ECM_VARIANTS {
        let answers = Answers::new(&[negative(0x22, 0x31)], &[]);
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "READ_CODING", "{variant}");
        assert_eq!(sent(&report), [read_request()], "{variant}");
    }
}

#[test]
fn p3_a_first_read_of_the_wrong_length_fails_with_read_coding() {
    for variant in ECM_VARIANTS {
        let long = {
            let mut read = coding_read(0x01);
            read.push(0x00);
            read
        };
        for read in [
            coding_read(0x01)[..5].to_vec(),
            vec![0x62, 0x01, 0x20],
            long,
        ] {
            let answers = Answers::new(std::slice::from_ref(&read), &[]);
            let report = run_coding(variant, &answers);
            assert_eq!(failed_with(&report), "READ_CODING", "{variant} {read:02X?}");
            assert_eq!(sent(&report), [read_request()], "{variant} {read:02X?}");
        }
    }
}

#[test]
fn p4_a_refused_extended_session_fails_with_session_before_any_security_request() {
    for variant in ECM_VARIANTS {
        let mut answers = Answers::new(&[coding_read(0x01)], &[]);
        answers.session = negative(0x10, 0x12);
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "SESSION", "{variant}");
        assert_eq!(sent(&report), unlock_requests()[..2], "{variant}");
    }
}

#[test]
fn p5_a_refused_seed_request_fails_with_security_and_sends_no_key() {
    for variant in ECM_VARIANTS {
        // Not unlocking in this session, and the attempt delay still running.
        for nrc in [0x22, 0x37] {
            let mut answers = Answers::new(&[coding_read(0x01)], &[]);
            answers.seed = negative(0x27, nrc);
            let report = run_coding(variant, &answers);
            assert_eq!(failed_with(&report), "SECURITY", "{variant} {nrc:#X}");
            assert_eq!(sent(&report), unlock_requests()[..3], "{variant} {nrc:#X}");
        }
    }
}

#[test]
fn p6_a_refused_key_fails_with_security_and_writes_nothing() {
    for variant in ECM_VARIANTS {
        for nrc in [0x35, 0x36] {
            let mut answers = Answers::new(&[coding_read(0x01)], &[]);
            answers.key = negative(0x27, nrc);
            let report = run_coding(variant, &answers);
            assert_eq!(failed_with(&report), "SECURITY", "{variant} {nrc:#X}");
            assert_eq!(sent(&report), unlock_requests(), "{variant} {nrc:#X}");
        }
    }
}

#[test]
fn p7_a_zero_seed_means_unlocked_and_no_key_is_sent() {
    for variant in ECM_VARIANTS {
        let mut answers = Answers::new(
            &[coding_read(0x01), coding_read(0x03), coding_read(0x03)],
            &[WRITE_OK.to_vec()],
        );
        answers.seed = vec![0x67, 0x01, 0x00, 0x00, 0x00, 0x00];
        let report = run_coding(variant, &answers);
        assert_eq!(report.end, DryRunEnd::Finished, "{variant}: {report:#?}");
        assert_eq!(
            sent(&report),
            [
                read_request(),
                vec![0x10, 0x03],
                vec![0x27, 0x01],
                coding_write(0x03),
                read_request(),
            ],
            "{variant}"
        );
    }
}

#[test]
fn p8_the_key_is_the_seed_xor_the_mask_and_only_the_cruise_bit_changes() {
    for variant in ECM_VARIANTS {
        for seed in [0x1234_5678_u32, 0xFFFF_FFFF, 0x5A5A_5A5A, 0x0000_0001] {
            let mut answers = Answers::new(
                &[coding_read(0x05), coding_read(0x07)],
                &[WRITE_OK.to_vec()],
            );
            answers.seed = [vec![0x67, 0x01], seed.to_be_bytes().to_vec()].concat();
            let report = run_coding(variant, &answers);
            assert_eq!(report.end, DryRunEnd::Finished, "{variant}: {report:#?}");
            let requests = sent(&report);
            let mut key = vec![0x27, 0x02];
            key.extend_from_slice(&(seed ^ ECM_KEY_MASK).to_be_bytes());
            assert_eq!(requests[3], key, "{variant} {seed:#X}");
            // Options 0x05 become 0x07; the market and the idle speed stay.
            assert_eq!(requests[4], coding_write(0x07), "{variant}");
        }
    }
}

// W rows: after the first write.

#[test]
fn w1_a_write_that_reads_back_finishes_after_one_write_and_one_read() {
    for variant in ECM_VARIANTS {
        let answers = Answers::new(
            &[coding_read(0x01), coding_read(0x03)],
            &[WRITE_OK.to_vec()],
        );
        let report = run_coding(variant, &answers);
        assert_eq!(report.end, DryRunEnd::Finished, "{variant}: {report:#?}");
        assert_eq!(
            sent(&report),
            with(unlock_requests(), &[coding_write(0x03), read_request()]),
            "{variant}"
        );
    }
}

/// The requests of a run that wrote the new coding, read it back and then rolled back.
fn rollback_requests(confirm: bool) -> Vec<Vec<u8>> {
    let mut requests = with(
        unlock_requests(),
        &[coding_write(0x03), read_request(), coding_write(0x01)],
    );
    if confirm {
        requests.push(read_request());
    }
    requests
}

#[test]
fn w2_a_read_back_that_differs_is_rolled_back_and_confirmed() {
    for variant in ECM_VARIANTS {
        // Each field of the record differing from what was written, and the old coding.
        let differing = [
            coding_read(0x01),
            coding_read(0x02),
            coding_read(0x07),
            vec![0x62, 0x01, 0x20, 0x01, 0x03, 0x03, 0x20],
            vec![0x62, 0x01, 0x20, 0x00, 0x03, 0x04, 0x20],
        ];
        for back in differing {
            let answers = Answers::new(
                &[coding_read(0x01), back.clone(), coding_read(0x01)],
                &[WRITE_OK.to_vec()],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(
                failed_with(&report),
                "CODING_ROLLED_BACK",
                "{variant} {back:02X?}"
            );
            assert_eq!(sent(&report), rollback_requests(true), "{variant}");
        }
    }
}

#[test]
fn w3_a_refused_read_back_is_rolled_back() {
    for variant in ECM_VARIANTS {
        let answers = Answers::new(
            &[coding_read(0x01), negative(0x22, 0x31), coding_read(0x01)],
            &[WRITE_OK.to_vec()],
        );
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "CODING_ROLLED_BACK", "{variant}");
        assert_eq!(sent(&report), rollback_requests(true), "{variant}");
    }
}

#[test]
fn w4_a_rolled_back_coding_is_the_saved_one_with_the_other_fields_as_read() {
    for variant in ECM_VARIANTS {
        // Saved coding: other option bits, another idle speed.
        let saved = vec![0x62, 0x01, 0x20, 0x00, 0x05, 0x04, 0xB0];
        let answers = Answers::new(
            &[saved.clone(), coding_read(0x05), saved],
            &[WRITE_OK.to_vec()],
        );
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "CODING_ROLLED_BACK", "{variant}");
        let requests = sent(&report);
        assert_eq!(
            requests[4],
            write_coding([0x00, 0x07, 0x04, 0xB0]),
            "{variant}"
        );
        assert_eq!(
            requests[6],
            write_coding([0x00, 0x05, 0x04, 0xB0]),
            "{variant}"
        );
    }
}

#[test]
fn w5_a_read_back_of_the_wrong_length_is_rolled_back() {
    for variant in ECM_VARIANTS {
        // A record cut short, even when the bytes it has match what was written.
        let short = coding_read(0x03)[..5].to_vec();
        let mut long = coding_read(0x03);
        long.push(0x00);
        for back in [short, long] {
            let answers = Answers::new(
                &[coding_read(0x01), back.clone(), coding_read(0x01)],
                &[WRITE_OK.to_vec()],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(
                failed_with(&report),
                "CODING_ROLLED_BACK",
                "{variant} {back:02X?}: {report:#?}"
            );
            assert_eq!(sent(&report), rollback_requests(true), "{variant}");
        }
    }
}

#[test]
fn w6_a_refused_roll_back_fails_with_rollback_refused_and_reads_nothing_more() {
    for variant in ECM_VARIANTS {
        let answers = Answers::new(
            &[coding_read(0x01), coding_read(0x02)],
            &[WRITE_OK.to_vec(), negative(0x2E, 0x22)],
        );
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "ROLLBACK_REFUSED", "{variant}");
        assert_eq!(sent(&report), rollback_requests(false), "{variant}");
    }
}

#[test]
fn w7_a_roll_back_that_does_not_read_back_fails_with_rollback_unconfirmed() {
    for variant in ECM_VARIANTS {
        let differing = [
            coding_read(0x02),
            negative(0x22, 0x31),
            coding_read(0x01)[..5].to_vec(),
        ];
        for confirm in differing {
            let answers = Answers::new(
                &[coding_read(0x01), coding_read(0x02), confirm.clone()],
                &[WRITE_OK.to_vec()],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(
                failed_with(&report),
                "ROLLBACK_UNCONFIRMED",
                "{variant} {confirm:02X?}"
            );
            assert_eq!(sent(&report), rollback_requests(true), "{variant}");
        }
    }
}

/// The requests of a run whose first write was refused: it then reads before deciding.
fn refused_write_requests(rolled_back: bool) -> Vec<Vec<u8>> {
    let mut requests = with(unlock_requests(), &[coding_write(0x03), read_request()]);
    if rolled_back {
        requests.extend([coding_write(0x01), read_request()]);
    }
    requests
}

#[test]
fn w8_a_refused_write_with_the_coding_unchanged_fails_with_write_refused() {
    for variant in ECM_VARIANTS {
        // The refusals the ECM gives for a session, security, authentication and a bad length.
        for nrc in [0x7F, 0x33, 0x34, 0x13, 0x22] {
            let answers = Answers::new(
                &[coding_read(0x01), coding_read(0x01)],
                &[negative(0x2E, nrc)],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(failed_with(&report), "WRITE_REFUSED", "{variant} {nrc:#X}");
            assert_eq!(sent(&report), refused_write_requests(false), "{variant}");
        }
    }
}

#[test]
fn w9_a_refused_write_that_changed_the_coding_is_rolled_back() {
    for variant in ECM_VARIANTS {
        // Partly or wholly applied, and left as something that is neither value.
        for read in [coding_read(0x03), coding_read(0x07)] {
            let answers = Answers::new(
                &[coding_read(0x01), read.clone(), coding_read(0x01)],
                &[negative(0x2E, 0x22), WRITE_OK.to_vec()],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(
                failed_with(&report),
                "CODING_ROLLED_BACK",
                "{variant} {read:02X?}"
            );
            assert_eq!(sent(&report), refused_write_requests(true), "{variant}");
        }
    }
}

#[test]
fn w10_a_refused_write_whose_coding_cannot_be_read_is_rolled_back() {
    for variant in ECM_VARIANTS {
        for read in [negative(0x22, 0x31), coding_read(0x01)[..5].to_vec()] {
            let answers = Answers::new(
                &[coding_read(0x01), read.clone(), coding_read(0x01)],
                &[negative(0x2E, 0x22), WRITE_OK.to_vec()],
            );
            let report = run_coding(variant, &answers);
            assert_eq!(
                failed_with(&report),
                "CODING_ROLLED_BACK",
                "{variant} {read:02X?}"
            );
            assert_eq!(sent(&report), refused_write_requests(true), "{variant}");
        }
        // A refused write and a roll-back that is refused too.
        let answers = Answers::new(
            &[coding_read(0x01), negative(0x22, 0x31)],
            &[negative(0x2E, 0x22)],
        );
        let report = run_coding(variant, &answers);
        assert_eq!(failed_with(&report), "ROLLBACK_REFUSED", "{variant}");
        assert_eq!(
            sent(&report),
            refused_write_requests(true)[..7],
            "{variant}"
        );
    }
}
