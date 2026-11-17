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
use diag_ir::dry_run::{DryRunEnd, DryRunEvent, Script, dry_run};
use diag_ir::encode::{ParamValue, encode_request};
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
