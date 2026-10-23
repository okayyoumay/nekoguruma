//! Variant identification against the ECU, as a server runs it before it sends a job (8.9.1):
//! each candidate variant's identification services are sent to the ECU by a program of their
//! own ([`identification_document`]), and [`identified`] decodes the answers through each
//! variant's own services and picks the variant whose patterns they match
//! ([`crate::decode::select_variant`], ADR-306, ADR-315).

use std::collections::HashMap;

use thiserror::Error;

use crate::declaration::{LoadError, encode_ecu_document, load_ecu_document};
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

/// The identification run of a variant's `document`: the document with its procedure replaced
/// by a program that sends the request of each service its variant patterns name, through the
/// generic send-receive, and leaves the answers on the stack. Returns the document and the
/// services in the order their answers are stacked. Every identification request is constant
/// (a pattern names a service without arguments).
pub fn identification_document(document: &[u8]) -> Result<(Vec<u8>, Vec<u32>), IdentifyError> {
    let loaded = load_ecu_document(document)?;
    let variant = loaded.variant().ok_or(IdentifyError::NoVariant)?;
    let mut services: Vec<u32> = variant
        .patterns()
        .into_iter()
        .flatten()
        .map(|pattern| pattern.service_id())
        .collect();
    services.sort_unstable();
    services.dedup();
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
    for &id in &services {
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
        let constant = u32::try_from(program.constants.len()).expect("a few constants");
        program.constants.push(message);
        program.code.push(Op::PushBytes(constant));
        program.code.push(Op::SendReceive { service: id });
    }
    let mut unpacked = loaded.unpack();
    unpacked.procedure =
        Some(postcard::to_allocvec(&program).map_err(|_| IdentifyError::Serialize)?);
    Ok((encode_ecu_document(&unpacked), services))
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
            let positive = variant
                .services()
                .into_iter()
                .flatten()
                .find(|s| s.id() == *service)
                .and_then(|s| s.positive())
                .ok_or(IdentifyError::Incomplete(*service, "positive response"))?;
            if let Ok(fields) = decode_positive_response(&positive, variant.layouts(), answer) {
                decoded.insert((index, *service), fields);
            }
        }
    }
    Ok(select_variant(variants, |index, service| {
        decoded.get(&(index, service)).map(Vec::as_slice)
    }))
}
