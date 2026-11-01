//! Variant identification against the ECU, as a server runs it before it sends a job (8.9.1):
//! each candidate variant's identification services are sent to the ECU by a program of their
//! own ([`identification_document`]), and [`identified`] decodes the answers through each
//! variant's own services and picks the variant whose patterns they match
//! ([`crate::decode::select_variant`], ADR-306, ADR-315).

use std::collections::HashMap;

use thiserror::Error;

use crate::declaration::{EcuVariant, LoadError, encode_ecu_document, load_ecu_document};
use crate::decode::{DecodedField, decode_positive_response, select_variant};
use crate::encode::{EncodeError, encode_request};
use crate::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};

/// Why an identification run could not be built or read.
#[derive(Debug, Error, PartialEq)]
pub enum IdentifyError {
    #[error("the document does not load: {0}")]
    Load(#[from] LoadError),
    #[error("the document has no variant")]
    NoVariant,
    /// A service a variant pattern names has no request, or no positive response, in the
    /// variant's declarations.
    #[error("identification service {0} has no {1}")]
    Incomplete(u32, &'static str),
    #[error("identification service {service}: {error}")]
    Encode { service: u32, error: EncodeError },
    #[error("the program does not serialize")]
    Serialize,
    /// A run's final stack does not hold one byte answer per identification service.
    #[error("the run of document {0} left {1} values for {2} identification services")]
    Answers(usize, usize, usize),
    #[error("the runs are for {runs} documents, not {documents}")]
    Runs { runs: usize, documents: usize },
}

/// The identification requests of `variant`: for each service its patterns name, in rising
/// service order without repeats, the whole request (the SID, then the encoded request), which
/// is constant because a pattern names a service without arguments. A service with no request
/// in the variant's declarations, or a request that needs values, is an error. The server's
/// identification program ([`identification_document`]) and the agent's identification job
/// (8.8.2) build their requests so.
pub fn identification_requests(
    variant: &EcuVariant<'_>,
) -> Result<Vec<(u32, Vec<u8>)>, IdentifyError> {
    let mut services: Vec<u32> = variant
        .patterns()
        .into_iter()
        .flatten()
        .map(|pattern| pattern.service_id())
        .collect();
    services.sort_unstable();
    services.dedup();
    services
        .into_iter()
        .map(|id| Ok((id, identification_request_of(variant, id)?)))
        .collect()
}

/// The whole constant request (the SID, then the encoded request) of service `id` of `variant`,
/// built without values: the request of a service a variant pattern names, or of a service a field
/// rule reads (8.8.2). A service with no request in the variant's declarations, or a request that
/// needs values, is an error. [`identification_requests`] builds each of its requests so.
pub fn identification_request_of(
    variant: &EcuVariant<'_>,
    id: u32,
) -> Result<Vec<u8>, IdentifyError> {
    let request = variant
        .services()
        .into_iter()
        .flatten()
        .find(|service| service.id() == id)
        .and_then(|service| service.request())
        .ok_or(IdentifyError::Incomplete(id, "request"))?;
    let mut message = vec![request.sid()];
    message.extend(
        encode_request(&request, &HashMap::new())
            .map_err(|error| IdentifyError::Encode { service: id, error })?,
    );
    Ok(message)
}

/// The identification run of a variant's `document`: the document with its procedure replaced
/// by a program that sends the request of each service its variant patterns name, through the
/// generic send-receive, and leaves the answers on the stack. Returns the document and the
/// services in the order their answers are stacked. Every identification request is constant
/// (a pattern names a service without arguments).
pub fn identification_document(document: &[u8]) -> Result<(Vec<u8>, Vec<u32>), IdentifyError> {
    let loaded = load_ecu_document(document)?;
    let variant = loaded.variant().ok_or(IdentifyError::NoVariant)?;
    let requests = identification_requests(&variant)?;
    let mut program = Program {
        schema_version: IR_SCHEMA_VERSION,
        code: Vec::new(),
        constants: Vec::new(),
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    };
    let mut services = Vec::with_capacity(requests.len());
    for (id, message) in requests {
        let constant = u32::try_from(program.constants.len()).expect("a few constants");
        program.constants.push(message);
        program.code.push(Op::PushBytes(constant));
        program.code.push(Op::SendReceive { service: id });
        services.push(id);
    }
    let mut unpacked = loaded.unpack();
    unpacked.procedure =
        Some(postcard::to_allocvec(&program).map_err(|_| IdentifyError::Serialize)?);
    Ok((encode_ecu_document(&unpacked), services))
}

/// Decodes `answer`, the response to the identification request of `service`, through the
/// variant's positive response for that service. `Ok(None)` when it does not decode (a negative
/// response, a short answer or another service's), which matches no pattern; an error when the
/// variant declares no positive response for the service.
pub fn decode_identification_answer(
    variant: &EcuVariant<'_>,
    service: u32,
    answer: &[u8],
) -> Result<Option<Vec<DecodedField>>, IdentifyError> {
    let positive = variant
        .services()
        .into_iter()
        .flatten()
        .find(|s| s.id() == service)
        .and_then(|s| s.positive())
        .ok_or(IdentifyError::Incomplete(service, "positive response"))?;
    Ok(decode_positive_response(&positive, variant.layouts(), answer).ok())
}

/// The index in `documents` of the variant the identification runs select, or `None` when none
/// matches. `runs` gives, for each document in order, the services of its
/// [`identification_document`] and the final state of its run. An answer that does not decode
/// through its variant's service, such as a negative response, matches no pattern.
pub fn identified(
    documents: &[&[u8]],
    runs: &[(Vec<u32>, VmState)],
) -> Result<Option<usize>, IdentifyError> {
    if runs.len() != documents.len() {
        return Err(IdentifyError::Runs {
            runs: runs.len(),
            documents: documents.len(),
        });
    }
    let loaded = documents
        .iter()
        .map(|bytes| load_ecu_document(bytes))
        .collect::<Result<Vec<_>, _>>()?;
    let mut variants = Vec::with_capacity(loaded.len());
    let mut decoded: HashMap<(usize, u32), Vec<DecodedField>> = HashMap::new();
    for (index, ((services, state), document)) in runs.iter().zip(&loaded).enumerate() {
        let variant = document.variant().ok_or(IdentifyError::NoVariant)?;
        variants.push(variant);
        let answers: Vec<&[u8]> = state
            .stack
            .iter()
            .filter_map(|value| match value {
                Value::Bytes(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        if answers.len() != services.len() || state.stack.len() != services.len() {
            return Err(IdentifyError::Answers(
                index,
                state.stack.len(),
                services.len(),
            ));
        }
        for (service, answer) in services.iter().zip(answers) {
            if let Some(fields) = decode_identification_answer(&variant, *service, answer)? {
                decoded.insert((index, *service), fields);
            }
        }
    }
    Ok(select_variant(variants, |index, service| {
        decoded.get(&(index, service)).map(Vec::as_slice)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::{
        DataKind, EcuDocumentT, EcuVariantT, FieldT, Placement, PositiveResponseT, RequestT,
        ServiceT, VariantPatternT,
    };

    fn read_service(id: u32, did: [u8; 2], bytes: u32) -> ServiceT {
        ServiceT {
            id,
            short_name: Some(format!("read{id}")),
            request: Some(Box::new(RequestT {
                sid: 0x22,
                prefix: Some(did.to_vec()),
                params: None,
            })),
            positive: Some(Box::new(PositiveResponseT {
                sid: 0x62,
                prefix: Some(did.to_vec()),
                fields: Some(vec![FieldT {
                    id: 1,
                    short_name: Some("value".into()),
                    placement: Placement::AfterPrevious,
                    bit_length: bytes * 8,
                    kind: DataKind::Ascii,
                    ..Default::default()
                }]),
            })),
            ..Default::default()
        }
    }

    fn pattern(service_id: u32) -> VariantPatternT {
        VariantPatternT {
            service_id,
            field_id: 1,
            expected: Some(b"P".to_vec()),
            pattern: 1,
        }
    }

    fn document(services: Vec<ServiceT>, patterns: &[u32]) -> Vec<u8> {
        encode_ecu_document(&EcuDocumentT {
            schema_version: IR_SCHEMA_VERSION,
            definition_id: None,
            source_format: Some("csv-starlark".into()),
            source_digest: Some("00".into()),
            variant: Some(Box::new(EcuVariantT {
                id: 1,
                short_name: Some("v".into()),
                services: Some(services),
                patterns: Some(patterns.iter().map(|&s| pattern(s)).collect()),
                ..Default::default()
            })),
            procedure: None,
        })
    }

    #[test]
    fn identification_requests_are_sorted_and_deduplicated_with_their_service_identifier() {
        let bytes = document(
            vec![
                read_service(2, [0xF1, 0x89], 2),
                read_service(1, [0xF1, 0x87], 2),
            ],
            &[2, 1, 2],
        );
        let loaded = load_ecu_document(&bytes).unwrap();
        let requests = identification_requests(&loaded.variant().unwrap()).unwrap();
        assert_eq!(
            requests,
            vec![(1, vec![0x22, 0xF1, 0x87]), (2, vec![0x22, 0xF1, 0x89])]
        );
        // The server's program names the same services in the same order.
        let (_, services) = identification_document(&bytes).unwrap();
        assert_eq!(services, vec![1, 2]);
    }

    #[test]
    fn the_request_of_one_service_is_the_entry_identification_requests_builds() {
        let bytes = document(
            vec![
                read_service(2, [0xF1, 0x89], 2),
                read_service(1, [0xF1, 0x87], 2),
                read_service(3, [0xF1, 0x90], 2),
            ],
            &[2, 1],
        );
        let loaded = load_ecu_document(&bytes).unwrap();
        let variant = loaded.variant().unwrap();
        for (id, request) in identification_requests(&variant).unwrap() {
            assert_eq!(identification_request_of(&variant, id), Ok(request));
        }
        // A service no pattern names (a field rule's) builds its request too.
        assert_eq!(
            identification_request_of(&variant, 3),
            Ok(vec![0x22, 0xF1, 0x90])
        );
        assert_eq!(
            identification_request_of(&variant, 9),
            Err(IdentifyError::Incomplete(9, "request"))
        );
        let mut scalar = read_service(1, [0xF1, 0x87], 2);
        scalar.request.as_mut().unwrap().params = Some(vec![FieldT {
            id: 9,
            short_name: Some("arg".into()),
            placement: Placement::AfterPrevious,
            bit_length: 8,
            kind: DataKind::Uint,
            ..Default::default()
        }]);
        let bytes = document(vec![scalar], &[1]);
        let loaded = load_ecu_document(&bytes).unwrap();
        assert!(matches!(
            identification_request_of(&loaded.variant().unwrap(), 1),
            Err(IdentifyError::Encode { service: 1, .. })
        ));
    }

    #[test]
    fn a_pattern_service_without_a_request_or_with_parameters_is_an_error() {
        let mut without = read_service(1, [0xF1, 0x87], 2);
        without.request = None;
        let bytes = document(vec![without], &[1]);
        let loaded = load_ecu_document(&bytes).unwrap();
        assert_eq!(
            identification_requests(&loaded.variant().unwrap()),
            Err(IdentifyError::Incomplete(1, "request"))
        );
        let mut scalar = read_service(1, [0xF1, 0x87], 2);
        scalar.request.as_mut().unwrap().params = Some(vec![FieldT {
            id: 9,
            short_name: Some("arg".into()),
            placement: Placement::AfterPrevious,
            bit_length: 8,
            kind: DataKind::Uint,
            ..Default::default()
        }]);
        let bytes = document(vec![scalar], &[1]);
        let loaded = load_ecu_document(&bytes).unwrap();
        assert!(matches!(
            identification_requests(&loaded.variant().unwrap()),
            Err(IdentifyError::Encode { service: 1, .. })
        ));
    }

    #[test]
    fn an_answer_decodes_through_the_services_positive_response_or_not_at_all() {
        let bytes = document(vec![read_service(1, [0xF1, 0x87], 2)], &[1]);
        let loaded = load_ecu_document(&bytes).unwrap();
        let variant = loaded.variant().unwrap();
        let fields = decode_identification_answer(&variant, 1, &[0x62, 0xF1, 0x87, b'P', b'Q'])
            .unwrap()
            .expect("decodes");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].id, 1);
        // A negative response, and a response too short for the field, do not decode.
        assert_eq!(
            decode_identification_answer(&variant, 1, &[0x7F, 0x22, 0x31]),
            Ok(None)
        );
        assert_eq!(
            decode_identification_answer(&variant, 1, &[0x62, 0xF1, 0x87, b'P']),
            Ok(None)
        );
        // A service with no positive response is an error, not a non-match.
        assert_eq!(
            decode_identification_answer(&variant, 7, &[0x62]),
            Err(IdentifyError::Incomplete(7, "positive response"))
        );
    }
}
