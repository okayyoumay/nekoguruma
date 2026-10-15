//! The DTCs and flash sessions of a variant's document (`docs/csv-tables.md` 4.11 and 4.12).
//!
//! [`DtcsAndFlash`] converts the fields of each layout a DTC names, and gathers each flash
//! session's segments, once for the whole definition; [`DtcsAndFlash::variant`] then builds one
//! variant's part.

use std::collections::HashMap;

use diag_ir::declaration::{DtcDefinitionT, FieldT, FlashSegmentT, FlashSessionT, FlashStep};

use super::services::Index;
use crate::tables::Definition;
use crate::tables::rows::{self, FlashSegmentRow};

/// The DTC definitions and flash sessions of one variant's document.
pub(super) struct VariantPart {
    pub dtcs: Vec<DtcDefinitionT>,
    pub flash_sessions: Vec<FlashSessionT>,
}

/// The parts shared by every variant's document.
pub(super) struct DtcsAndFlash<'a> {
    definition: &'a Definition,
    index: &'a Index<'a>,
    /// The fields of each layout a DTC names, with their ids (4.11).
    layout_fields: HashMap<&'a str, Vec<FieldT>>,
    /// Each flash session's `flash_segments.csv` rows, in row order (4.12).
    segments: HashMap<&'a str, Vec<&'a FlashSegmentRow>>,
}

impl<'a> DtcsAndFlash<'a> {
    pub(super) fn new(definition: &'a Definition, index: &'a Index<'a>) -> Self {
        let tables = &definition.tables;
        let mut layout_fields = HashMap::new();
        for dtc in &tables.dtcs {
            for name in [
                Some(&dtc.status),
                dtc.snapshot.as_ref(),
                dtc.extended.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                layout_fields
                    .entry(name.as_str())
                    .or_insert_with(|| index.layout_fields(&rows::Container::Layout(name.clone())));
            }
        }
        let mut segments: HashMap<&str, Vec<&FlashSegmentRow>> = HashMap::new();
        for segment in &tables.flash_segments {
            segments
                .entry(segment.session.as_str())
                .or_default()
                .push(segment);
        }
        Self {
            definition,
            index,
            layout_fields,
            segments,
        }
    }

    /// The DTC definitions and flash sessions of the document of `variant`, an index into
    /// `variants.csv`, in row order.
    pub(super) fn variant(&self, variant: usize) -> VariantPart {
        let tables = &self.definition.tables;
        let rows = &self.definition.variants[variant];
        let fields = |layout: Option<&String>| {
            Some(layout.map_or_else(Vec::new, |name| self.layout_fields[name.as_str()].clone()))
        };
        let dtcs = rows
            .dtcs
            .iter()
            .map(|&i| {
                let row = &tables.dtcs[i];
                let mut dtc = DtcDefinitionT::default();
                dtc.code = row.code;
                dtc.display_code = Some(row.display.clone());
                dtc.message_id = self.index.text_id(row.text.as_deref());
                dtc.status_fields = fields(Some(&row.status));
                dtc.snapshot_fields = fields(row.snapshot.as_ref());
                dtc.extended_fields = fields(row.extended.as_ref());
                dtc
            })
            .collect();
        let flash_sessions = rows
            .flash_sessions
            .iter()
            .map(|&i| {
                let row = &tables.flash_sessions[i];
                let mut session = FlashSessionT::default();
                session.id = row.id;
                session.short_name = Some(row.name.clone());
                session.steps = Some(row.steps.iter().copied().map(step).collect());
                session.segments = Some(
                    self.segments
                        .get(row.name.as_str())
                        .into_iter()
                        .flatten()
                        .map(|s| {
                            let mut segment = FlashSegmentT::default();
                            segment.short_name = Some(s.name.clone());
                            segment.start_address = s.start_address;
                            segment.length = s.length;
                            segment.artifact_offset = s.artifact_offset;
                            segment.block_size = s.block_size;
                            segment.checksum = Some(s.checksum.clone());
                            segment
                        })
                        .collect(),
                );
                session
            })
            .collect();
        VariantPart {
            dtcs,
            flash_sessions,
        }
    }
}

fn step(step: rows::FlashStep) -> FlashStep {
    match step {
        rows::FlashStep::EnterProgrammingSession => FlashStep::EnterProgrammingSession,
        rows::FlashStep::SecurityAccess => FlashStep::SecurityAccess,
        rows::FlashStep::EraseMemory => FlashStep::EraseMemory,
        rows::FlashStep::RequestDownload => FlashStep::RequestDownload,
        rows::FlashStep::TransferData => FlashStep::TransferData,
        rows::FlashStep::TransferExit => FlashStep::TransferExit,
        rows::FlashStep::CheckMemory => FlashStep::CheckMemory,
        rows::FlashStep::EcuReset => FlashStep::EcuReset,
    }
}
