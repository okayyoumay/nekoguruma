//! Converting a resolved CSV definition into the IR declaration part (`docs/csv-tables.md`,
//! ADR-295).
//!
//! [`to_ir`] turns the [`Definition`] that [`resolve`](crate::tables::resolve) returns, and the
//! procedures [`lower_procedures`] lowered from its `.star` files, into `EcuDocument`s and the
//! text and unit catalogs of 4.4: one document per variant and procedure, or one per variant
//! when the directory has no procedure (design 8.2.6, ADR-307). `resolve` and `lower_procedures`
//! have checked every rule the conversion relies on, so the conversion itself cannot fail. The
//! documents are the same, byte for byte, for the same input (section 6).
//!
//! Each document carries its header (`schema_version`, `source_format`, `source_digest`), the
//! variant's own data (4.1), its variant patterns (4.2), its protocols with their ComParams
//! (4.3), and its services with their fields and NRCs and the nested layouts those fields reach
//! (4.5 to 4.10, section 5; [`services`]), and its DTCs and flash sessions (4.11, 4.12;
//! [`dtcs_and_flash`]), and the program of its procedure, if it has one.

mod dtcs_and_flash;
mod services;

use std::collections::HashMap;
use std::fmt::Write as _;

use diag_ir::declaration::{
    ComParamT, EcuDocumentT, EcuVariant, EcuVariantT, NegativeShapeT, ProtocolT,
    encode_ecu_document, load_ecu_document,
};
use diag_ir::dry_run::{DryRunEnd, DryRunEvent, DryRunReport, Script, dry_run};
use diag_ir::{DEFAULT_MAX_STEPS, IR_SCHEMA_VERSION, Program, VmError};
use sha2::{Digest, Sha256};

use crate::star::{self, RequestIndex, StarError};
use crate::tables::Definition;
use crate::tables::rows::CatalogRow;

/// The `source_format` of a document converted from CSV tables and Starlark (ADR-254).
pub const SOURCE_FORMAT: &str = "csv-starlark";

/// What a definition converts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// The documents, by row of `variants.csv` in its order, and within a variant by procedure
    /// in byte order of the file names: one per variant and procedure, or one per variant when
    /// there is no procedure (ADR-307).
    pub documents: Vec<VariantDocument>,
    /// The text catalog: each `texts.csv` id with its default text, sorted by id (4.4).
    pub texts: Vec<CatalogEntry>,
    /// The unit catalog: each `units.csv` id with its symbol (empty when none is given), sorted
    /// by id (4.4).
    pub units: Vec<CatalogEntry>,
}

/// The document of one variant, with one procedure or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDocument {
    /// `EcuVariant.id`.
    pub id: u32,
    /// `EcuVariant.short_name`.
    pub name: String,
    /// The file name of the procedure the document carries; `None` for a document without one.
    pub procedure: Option<String>,
    /// The encoded `EcuDocument`, with the `DIR1` file identifier.
    pub bytes: Vec<u8>,
}

/// One entry of the text or unit catalog. The ids are those of the server's tables (design 9.2),
/// so the server, not the converter, reports an id it already holds with another text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: u32,
    pub text: String,
}

/// A procedure file of a definition directory, checked once and lowered for each variant
/// (ADR-307, ADR-311).
#[derive(Debug, Clone)]
pub struct Procedure {
    /// The file name, `.star` included.
    pub file: String,
    /// The file's contents as stored, which the document's `source_digest` covers.
    pub source: Vec<u8>,
    /// The postcard encoding of the `diag_ir::Program` lowered for each variant, by row of
    /// `variants.csv` in its order: `EcuDocument.procedure` of that variant's document.
    pub programs: Vec<Vec<u8>>,
    /// The file's warnings (`starlark-subset.md` section 5), each given once (ADR-311).
    pub warnings: Vec<StarError>,
}

/// Why [`lower_procedures`] failed: the errors of every file that failed, and the warnings of
/// every file that lowered, refused by the dry run or not, which a rejected report carries too (`starlark-subset.md` section 5).
#[derive(Debug, Clone)]
pub struct ProceduresRejected {
    pub errors: Vec<StarError>,
    pub warnings: Vec<StarError>,
}

/// Checks each procedure file of `definition`, in byte order of their names, and lowers it for
/// each variant against that variant's declarations (ADR-311). Every file is checked and
/// lowered, so the errors of all of them are listed together; a file is lowered only when its
/// check passed, and an error that only some variants give names them.
pub fn lower_procedures(definition: &Definition) -> Result<Vec<Procedure>, ProceduresRejected> {
    let declarations: Vec<Vec<u8>> = declarations(definition)
        .iter()
        .map(encode_ecu_document)
        .collect();
    let index = services::Index::new(definition);
    let variants: Vec<(&str, EcuVariant<'_>, RequestIndex)> = definition
        .tables
        .variants
        .iter()
        .zip(&declarations)
        .enumerate()
        .map(|(at, (row, bytes))| {
            let variant = load_ecu_document(bytes)
                .expect("a converted declaration part loads")
                .variant()
                .expect("a converted document has its variant");
            (row.name.as_str(), variant, index.request_index(at))
        })
        .collect();
    let mut procedures = Vec::new();
    let mut errors = Vec::new();
    let mut refused_warnings = Vec::new();
    for (file, source) in &definition.tables.procedures {
        let checked = match star::check_file(file, source) {
            Ok(checked) => checked,
            Err(more) => {
                errors.extend(more);
                continue;
            }
        };
        // Lowering needs the source to be UTF-8, so nothing is replaced here.
        let text = String::from_utf8_lossy(source);
        let mut programs = Vec::with_capacity(variants.len());
        let mut file_errors = Vec::new();
        let mut file_warnings = Vec::new();
        for (name, variant, requests) in &variants {
            let name = *name;
            match star::lower(file, &checked, *variant, requests) {
                Ok(lowered) => {
                    let script = Script::built_defaults();
                    let report = dry_run(&lowered.program, variant, &script, DEFAULT_MAX_STEPS);
                    // The warnings stay when the dry run refuses the variant: they can explain
                    // the refusal, such as a step limit.
                    let verdict = dry_run_verdict(file, &text, &lowered.program, variant, &report);
                    file_warnings.push((name, lowered.warnings));
                    if let Err(error) = verdict {
                        file_errors.push((name, vec![error]));
                        continue;
                    }
                    programs.push(
                        postcard::to_allocvec(&lowered.program)
                            .expect("a program serializes into memory"),
                    );
                }
                Err(more) => file_errors.push((name, more)),
            }
        }
        let warnings = merge(file_warnings, variants.len());
        if file_errors.is_empty() {
            procedures.push(Procedure {
                file: file.clone(),
                source: source.clone(),
                programs,
                warnings,
            });
        } else {
            errors.extend(merge(file_errors, variants.len()));
            refused_warnings.extend(warnings);
        }
    }
    if errors.is_empty() {
        Ok(procedures)
    } else {
        Err(ProceduresRejected {
            errors,
            warnings: procedures
                .into_iter()
                .flat_map(|p| p.warnings)
                .chain(refused_warnings)
                .collect(),
        })
    }
}

/// Whether the dry run of a lowered procedure on built answers (`report`) ended in a way the
/// conversion accepts (ADR-391); the one place that decides. It accepts the procedure's own end
/// (`Finished`), every `Failed` (the author's `diag.fail` and the lowering's own failures, such
/// as a loop past its limit), and an `Undecodable` response that was built from the declaration
/// of the service named, since the answer built from a declaration need not decode into the
/// shape a procedure's later reads expect. Everything else is refused as `STAR_DRY_RUN_STOPPED`
/// at the source of the instruction the run stopped on (`report.pc`); a new `VmError` or
/// `DryRunError` variant is refused until this function is told otherwise.
fn dry_run_verdict(
    file: &str,
    source: &str,
    program: &Program,
    variant: &EcuVariant<'_>,
    report: &DryRunReport,
) -> Result<(), StarError> {
    let reason = match &report.end {
        DryRunEnd::Finished | DryRunEnd::Failed(_) => return Ok(()),
        DryRunEnd::Vm(VmError::Undecodable { service })
            if built_answer_undecodable(variant, report, *service) =>
        {
            return Ok(());
        }
        DryRunEnd::Vm(VmError::Undecodable { service }) => {
            let name = variant
                .services()
                .and_then(|services| services.iter().find(|s| s.id() == *service))
                .and_then(|s| s.short_name().map(str::to_owned))
                .unwrap_or_else(|| service.to_string());
            format!("the response does not decode as service {name}'s positive response")
        }
        DryRunEnd::Vm(error) => error.to_string(),
        DryRunEnd::Host(error) => error.to_string(),
        DryRunEnd::Invalid(error) => error.to_string(),
        DryRunEnd::Waiting(on) => {
            format!("the procedure waits on {on:?} although the host answers")
        }
        DryRunEnd::StepLimit => {
            format!("did not end within {DEFAULT_MAX_STEPS} instructions")
        }
    };
    // The entry covering `pc` is the last one at or before it.
    let at = program
        .source_map
        .iter()
        .take_while(|span| span.pc <= report.pc)
        .last();
    let (line, column) = at.map_or((1, 1), |span| (span.line, span.column));
    Err(star::at_position(
        star::StarCode::DryRunStopped,
        file,
        source,
        line,
        column,
        format!("the dry run on answers built from the declarations stopped: {reason}"),
    ))
}

/// Whether the report holds a built answer to service `service` (by id) that did not decode.
fn built_answer_undecodable(variant: &EcuVariant<'_>, report: &DryRunReport, service: u32) -> bool {
    let Some(name) = variant
        .services()
        .and_then(|services| services.iter().find(|s| s.id() == service))
        .and_then(|s| s.short_name())
    else {
        return false;
    };
    report.events.iter().any(|event| {
        matches!(
            event,
            DryRunEvent::Request { service: s, built: true, decoded: Some(Err(_)), .. }
                if s == name
        )
    })
}

/// The errors or warnings that lowering one file for each of `variants` variants gave, by the
/// name of the variant: each distinct one once, in source order (ADR-311). One that not every
/// variant gave names the variants that did at the end of its hint.
fn merge(per_variant: Vec<(&str, Vec<StarError>)>, variants: usize) -> Vec<StarError> {
    // A file can give many errors (one per annotation, up to the bounds of subset 2.8), so each
    // is found by hash rather than by comparing it with every earlier one.
    let mut merged: Vec<(StarError, Vec<&str>)> = Vec::new();
    let mut at: HashMap<StarError, usize> = HashMap::new();
    for (name, found) in per_variant {
        for error in found {
            match at.get(&error) {
                // One variant giving the same error twice still counts once.
                Some(&i) if merged[i].1.last() == Some(&name) => {}
                Some(&i) => merged[i].1.push(name),
                None => {
                    at.insert(error.clone(), merged.len());
                    merged.push((error, vec![name]));
                }
            }
        }
    }
    // Stable, so errors at one position keep the order the lowering gave them.
    merged.sort_by_key(|(error, _)| (error.line, error.column));
    merged
        .into_iter()
        .map(|(mut error, names)| {
            if names.len() < variants {
                let noun = if names.len() == 1 {
                    "variant"
                } else {
                    "variants"
                };
                error.hint = format!("{} (in {noun} {})", error.hint, names.join(", "));
            }
            error
        })
        .collect()
}

/// Each variant's document without a procedure, by row of `variants.csv` in its order: the
/// header, with an empty `source_digest` that [`to_ir`] fills in, and the variant's whole
/// declaration part. [`lower_procedures`] lowers against these, and [`to_ir`] adds a procedure
/// and its digest to each.
fn declarations(definition: &Definition) -> Vec<EcuDocumentT> {
    let tables = &definition.tables;
    let unit_ids: HashMap<&str, u32> = tables
        .units
        .iter()
        .map(|row| (row.key.as_str(), row.id))
        .collect();
    let index = services::Index::new(definition);
    let dtcs_and_flash = dtcs_and_flash::DtcsAndFlash::new(definition, &index);
    let mut documents = Vec::new();
    for (at, (variant, rows)) in tables.variants.iter().zip(&definition.variants).enumerate() {
        let mut protocols: Vec<ProtocolT> = rows
            .protocols
            .iter()
            .map(|&i| {
                let row = &tables.protocols[i];
                let mut protocol = ProtocolT::default();
                protocol.short_name = Some(row.name.clone());
                protocol.baud_rate = row.baud_rate;
                protocol.tx_id = row.tx_id.into();
                protocol.rx_id = row.rx_id.into();
                protocol.functional_id = row.functional_id.into();
                protocol.extended_addressing = row.extended_addressing;
                protocol.params = Some(Vec::new());
                // The layer names a shipped profile (4.3), whose shape and pending codes the
                // agent reads answers with (ADR-309 item 4).
                let profile = crate::profile::get(&row.application_layer)
                    .expect("resolving checks that a profile ships for every protocol's layer");
                protocol.application_layer = Some(row.application_layer.clone());
                let mut shape = NegativeShapeT::default();
                shape.marker = profile.negative.marker;
                shape.echoes_sid = profile.negative.echoes_sid;
                shape.code_at = profile.negative.code_at;
                shape.pending = Some(profile.pending.to_vec());
                protocol.negative_shape = Some(Box::new(shape));
                protocol
            })
            .collect();
        // A variant's protocol names are unique (section 3), and every ComParam names one of
        // them (4.3), so each is appended to exactly one protocol, in row order.
        let by_name: HashMap<&str, usize> = rows
            .protocols
            .iter()
            .enumerate()
            .map(|(at, &i)| (tables.protocols[i].name.as_str(), at))
            .collect();
        for &i in &rows.comparams {
            let row = &tables.comparams[i];
            let mut param = ComParamT::default();
            param.short_name = Some(row.name.clone());
            param.value = row.value;
            param.unit_id = row.unit.as_deref().map_or(0, |key| unit_ids[key]);
            protocols[by_name[row.protocol.as_str()]]
                .params
                .get_or_insert_with(Vec::new)
                .push(param);
        }
        let mut ecu = EcuVariantT::default();
        ecu.id = variant.id;
        ecu.short_name = Some(variant.name.clone());
        ecu.part_numbers = Some(variant.part_numbers.clone());
        ecu.sw_versions = Some(variant.sw_versions.clone());
        ecu.protocols = Some(protocols);
        let part = index.variant(at);
        ecu.patterns = Some(part.patterns);
        ecu.services = Some(part.services);
        ecu.layouts = Some(part.layouts);
        let part = dtcs_and_flash.variant(at);
        ecu.dtcs = Some(part.dtcs);
        ecu.flash_sessions = Some(part.flash_sessions);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some(SOURCE_FORMAT.to_owned());
        document.source_digest = Some(String::new());
        document.definition_id = tables.definition.first().map(|row| row.id.clone());
        document.variant = Some(Box::new(ecu));
        documents.push(document);
    }
    documents
}

/// Converts `definition`, which [`resolve`](crate::tables::resolve) returned, with the
/// `procedures` [`lower_procedures`] returned for it, into documents and the text and unit
/// catalogs. Every procedure goes with every variant, with the program lowered for that variant
/// (ADR-307, ADR-311).
pub fn to_ir(definition: &Definition, procedures: &[Procedure]) -> Converted {
    let tables = &definition.tables;
    let carried: Vec<Option<&Procedure>> = if procedures.is_empty() {
        vec![None]
    } else {
        procedures.iter().map(Some).collect()
    };
    // Each document's digest covers the tables and the one procedure file it carries (section
    // 6), so it depends on the procedure alone and is taken once per procedure.
    let digests: Vec<String> = carried
        .iter()
        .map(|procedure| {
            let file = procedure.map(|p| (p.file.as_str(), p.source.as_slice()));
            source_digest(
                tables
                    .sources
                    .iter()
                    .map(|(name, contents)| (name.as_str(), contents.as_slice()))
                    .chain(file),
            )
        })
        .collect();
    let mut documents = Vec::new();
    for (at, (variant, mut document)) in tables
        .variants
        .iter()
        .zip(declarations(definition))
        .enumerate()
    {
        for (procedure, digest) in carried.iter().zip(&digests) {
            document.source_digest = Some(digest.clone());
            document.procedure = procedure.map(|p| p.programs[at].clone());
            documents.push(VariantDocument {
                id: variant.id,
                name: variant.name.clone(),
                procedure: procedure.map(|p| p.file.clone()),
                bytes: encode_ecu_document(&document),
            });
        }
    }
    Converted {
        documents,
        texts: catalog(&tables.texts),
        units: catalog(&tables.units),
    }
}

/// The lowercase hexadecimal SHA-256 of `files`, given by name and contents (section 6). The
/// files are taken in byte order of their names; each contributes its name, a zero byte, its
/// length as a big-endian 64-bit integer and its contents. The names of a definition's files
/// are unique; should a name repeat, its files are taken in byte order of their contents, so
/// the digest still does not depend on the order `files` gives them in.
pub fn source_digest<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut files: Vec<(&str, &[u8])> = files.into_iter().collect();
    files.sort_unstable();
    let mut hasher = Sha256::new();
    for (name, contents) in files {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        // A `usize` length always fits in 64 bits on the targets Rust supports.
        hasher.update((contents.len() as u64).to_be_bytes());
        hasher.update(contents);
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A catalog's entries, sorted by id. Ids are unique within a table (section 3).
fn catalog(rows: &[CatalogRow]) -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = rows
        .iter()
        .map(|row| CatalogEntry {
            id: row.id,
            text: row.text.clone(),
        })
        .collect();
    entries.sort_unstable_by_key(|entry| entry.id);
    entries
}

#[cfg(test)]
mod tests;
