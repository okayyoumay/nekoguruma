//! Converting a resolved CSV definition into the IR declaration part (`docs/csv-tables.md`,
//! ADR-295).
//!
//! [`to_ir`] turns the [`Definition`] that [`resolve`](crate::tables::resolve) returns into one
//! `EcuDocument` per variant (design 8.2.6) and the text and unit catalogs of 4.4. `resolve` has
//! checked every rule the conversion relies on, so the conversion itself cannot fail. The
//! documents are the same, byte for byte, for the same input (section 6).
//!
//! Each document carries its header (`schema_version`, `source_format`, `source_digest`), the
//! variant's own data (4.1), its variant patterns (4.2), its protocols with their ComParams
//! (4.3), and its services with their fields and NRCs and the nested layouts those fields reach
//! (4.5 to 4.10, section 5; [`services`]), and its DTCs and flash sessions (4.11, 4.12;
//! [`dtcs_and_flash`]). It carries no procedure: that is attached by the Starlark lowering.

mod dtcs_and_flash;
mod services;

use std::collections::HashMap;
use std::fmt::Write as _;

use diag_ir::IR_SCHEMA_VERSION;
use diag_ir::declaration::{ComParamT, EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
use sha2::{Digest, Sha256};

use crate::tables::Definition;
use crate::tables::rows::CatalogRow;

/// The `source_format` of a document converted from CSV tables and Starlark (ADR-254).
pub const SOURCE_FORMAT: &str = "csv-starlark";

/// What a definition converts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// One document per row of `variants.csv`, in its order.
    pub documents: Vec<VariantDocument>,
    /// The text catalog: each `texts.csv` id with its default text, sorted by id (4.4).
    pub texts: Vec<CatalogEntry>,
    /// The unit catalog: each `units.csv` id with its symbol (empty when none is given), sorted
    /// by id (4.4).
    pub units: Vec<CatalogEntry>,
}

/// The document of one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDocument {
    /// `EcuVariant.id`.
    pub id: u32,
    /// `EcuVariant.short_name`.
    pub name: String,
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

/// Converts `definition`, as [`resolve`](crate::tables::resolve) returned it, into one document
/// per variant and the text and unit catalogs.
pub fn to_ir(definition: &Definition) -> Converted {
    let tables = &definition.tables;
    let digest = source_digest(
        tables
            .sources
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.as_slice())),
    );
    let unit_ids: HashMap<&str, u32> = tables
        .units
        .iter()
        .map(|row| (row.key.as_str(), row.id))
        .collect();
    let index = services::Index::new(definition);
    let dtcs_and_flash = dtcs_and_flash::DtcsAndFlash::new(definition, &index);
    let documents = tables
        .variants
        .iter()
        .zip(&definition.variants)
        .enumerate()
        .map(|(at, (variant, rows))| {
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
            document.source_digest = Some(digest.clone());
            document.variant = Some(Box::new(ecu));
            VariantDocument {
                id: variant.id,
                name: variant.name.clone(),
                bytes: encode_ecu_document(&document),
            }
        })
        .collect();
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
