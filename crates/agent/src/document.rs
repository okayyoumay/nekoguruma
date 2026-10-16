//! The declaration part a job's program decodes responses with (`Op::DecodeField`, ADR-316,
//! design 8.2.4).
//!
//! A job runs one variant's document (design 8.2.6): its procedure is the program, and its
//! variant's declarations are what the VM decodes response fields through. The runner hands the
//! document to the VM at every step through its host ([`Declarations`]); the document is not
//! part of the VM state, so a restart passes the same document again, as it passes the same
//! program.

use std::sync::Arc;

use diag_ir::Program;
use diag_ir::declaration::{EcuVariant, LoadError, load_ecu_document};

/// A variant's document, checked once by [`load_ecu_document`] and shared by the runs of a
/// job.
#[derive(Debug, Clone)]
pub struct JobDocument {
    bytes: Arc<[u8]>,
}

/// Why a document cannot be run.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("not a variant document: {0}")]
    Load(#[from] LoadError),
    #[error("the document carries no procedure")]
    NoProcedure,
    #[error("the document's procedure is not an IR program: {0}")]
    Procedure(#[from] postcard::Error),
    #[error("the document's procedure has {0} bytes after its program")]
    TrailingBytes(usize),
}

impl JobDocument {
    /// Checks `bytes` as a variant document (ADR-296).
    pub fn new(bytes: Vec<u8>) -> Result<Self, DocumentError> {
        load_ecu_document(&bytes)?;
        Ok(Self {
            bytes: bytes.into(),
        })
    }

    /// The program the document carries as its procedure (design 8.2.6, ADR-307).
    pub fn program(&self) -> Result<Program, DocumentError> {
        let procedure = self
            .document()
            .procedure()
            .ok_or(DocumentError::NoProcedure)?;
        let (program, rest) = postcard::take_from_bytes(procedure.bytes())?;
        if !rest.is_empty() {
            return Err(DocumentError::TrailingBytes(rest.len()));
        }
        Ok(program)
    }

    /// The variant's declarations.
    pub fn variant(&self) -> EcuVariant<'_> {
        self.document()
            .variant()
            .expect("load_ecu_document refuses a document without its variant")
    }

    fn document(&self) -> diag_ir::declaration::EcuDocument<'_> {
        load_ecu_document(&self.bytes).expect("checked by JobDocument::new")
    }
}

/// The document a host's job decodes with, if it has one. A job without a document runs
/// programs that decode nothing; one that decodes fails with `VmError::NoDeclarations`.
pub trait Declarations {
    fn declarations(&self) -> Option<JobDocument> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use diag_ir::IR_SCHEMA_VERSION;
    use diag_ir::declaration::{
        DataKind, EcuDocumentT, EcuVariantT, FieldT, Placement, PositiveResponseT, ServiceT,
        encode_ecu_document,
    };

    use super::*;

    /// A document whose service 1 reads the VIN: its positive response is `62 F1 90` and then
    /// 17 ASCII bytes, field 1 (`vin`). `procedure` is the document's procedure, if any.
    pub(crate) fn vin_document(procedure: Option<&Program>) -> Vec<u8> {
        vin_document_with(procedure.map(|p| postcard::to_allocvec(p).unwrap()))
    }

    pub(crate) fn vin_document_with(procedure: Option<Vec<u8>>) -> Vec<u8> {
        let mut vin = FieldT::default();
        vin.id = 1;
        vin.short_name = Some("vin".into());
        vin.placement = Placement::AfterPrevious;
        vin.bit_length = 17 * 8;
        vin.kind = DataKind::Ascii;
        let mut positive = PositiveResponseT::default();
        positive.sid = 0x62;
        positive.prefix = Some(vec![0xF1, 0x90]);
        positive.fields = Some(vec![vin]);
        let mut service = ServiceT::default();
        service.id = 1;
        service.short_name = Some("readVin".into());
        service.positive = Some(Box::new(positive));
        let mut variant = EcuVariantT::default();
        variant.id = 1;
        variant.short_name = Some("v".into());
        variant.services = Some(vec![service]);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        document.procedure = procedure;
        encode_ecu_document(&document)
    }

    fn program() -> Program {
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code: Vec::new(),
            constants: Vec::new(),
            sections: Vec::new(),
            source_map: Vec::new(),
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Vec::new(),
        }
    }

    #[test]
    fn a_document_gives_its_procedure_and_declarations() {
        let mut program = program();
        program.code = vec![diag_ir::Op::PushI64(7)];
        let document = JobDocument::new(vin_document(Some(&program))).unwrap();
        assert_eq!(document.program().unwrap().code, program.code);
        assert_eq!(document.variant().id(), 1);
    }

    #[test]
    fn a_document_is_checked_and_must_carry_a_procedure() {
        assert!(matches!(
            JobDocument::new(b"not a document".to_vec()),
            Err(DocumentError::Load(_))
        ));
        let document = JobDocument::new(vin_document(None)).unwrap();
        assert!(matches!(
            document.program(),
            Err(DocumentError::NoProcedure)
        ));
        let document = JobDocument::new(vin_document_with(Some(vec![0xFF; 3]))).unwrap();
        assert!(matches!(
            document.program(),
            Err(DocumentError::Procedure(_))
        ));
        let mut procedure = postcard::to_allocvec(&program()).unwrap();
        procedure.push(0);
        let document = JobDocument::new(vin_document_with(Some(procedure))).unwrap();
        assert!(matches!(
            document.program(),
            Err(DocumentError::TrailingBytes(1))
        ));
    }
}
