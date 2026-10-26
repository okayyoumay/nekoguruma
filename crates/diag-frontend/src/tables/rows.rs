//! The typed rows of each table (`docs/csv-tables.md` sections 4 and 5), read by the rules that
//! need no other table: required cells, value types and ranges, and the cells a row's own kind
//! does not use. Rules that compare rows or tables are not checked here.

use diag_ir::{IdentityKind, PreconditionKind, RuntimeInput};

use super::error::{ErrorCode, Errors};
use super::sheet::{Row, Sheet, Spec};
use super::values::{self, Parsed, ServiceName};

/// A row's line, counting the header as line 1.
pub type Line = u32;

/// The variants a scoped row belongs to: empty means every variant.
pub type Scope = Vec<String>;

fn unsigned(min: i128, max: i128) -> impl Fn(&str) -> Parsed<i128> {
    move |cell| values::integer(cell, false, min, max)
}

fn as_u8(min: u8, max: u8) -> impl Fn(&str) -> Parsed<u8> {
    move |cell| values::integer(cell, false, min.into(), max.into()).map(|v| v as u8)
}

fn as_u16(max: u16) -> impl Fn(&str) -> Parsed<u16> {
    move |cell| values::integer(cell, false, 0, max.into()).map(|v| v as u16)
}

fn as_u32(min: u32) -> impl Fn(&str) -> Parsed<u32> {
    move |cell| values::integer(cell, false, min.into(), u32::MAX.into()).map(|v| v as u32)
}

fn as_u64(min: u64) -> impl Fn(&str) -> Parsed<u64> {
    move |cell| values::integer(cell, false, min.into(), u64::MAX.into()).map(|v| v as u64)
}

fn names(cell: &str) -> Parsed<Vec<String>> {
    values::list(cell, true, values::name)
}

fn scope(row: &mut Row<'_>, errors: &mut Errors) -> Option<Scope> {
    row.or(errors, "variants", Vec::new(), names)
}

// ---------------------------------------------------------------- 4.1 to 4.4

/// A `variants.csv` row (4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct VariantRow {
    pub line: Line,
    pub id: u32,
    pub name: String,
    pub part_numbers: Vec<String>,
    pub sw_versions: Vec<String>,
}

pub(crate) const VARIANTS: Spec = Spec {
    file: "variants.csv",
    scoped: false,
    columns: &[
        ("id", true),
        ("name", true),
        ("part_numbers", false),
        ("sw_versions", false),
    ],
};

pub(crate) fn variants(sheet: &Sheet, errors: &mut Errors) -> Vec<VariantRow> {
    let texts = |cell: &str| values::list(cell, true, values::text);
    sheet.rows(errors, |row, e| {
        let id = row.required(e, "id", as_u32(1));
        let name = row.required(e, "name", values::name);
        let part_numbers = row.or(e, "part_numbers", Vec::new(), texts);
        let sw_versions = row.or(e, "sw_versions", Vec::new(), texts);
        Some(VariantRow {
            line: row.line,
            id: id?,
            name: name?,
            part_numbers: part_numbers?,
            sw_versions: sw_versions?,
        })
    })
}

/// A `variant_patterns.csv` row (4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct VariantPatternRow {
    pub line: Line,
    pub variant: String,
    pub service: ServiceName,
    pub field: String,
    pub expected: Vec<u8>,
    /// The pattern the row belongs to (ADR-315); rows of one variant with the same number form
    /// one pattern.
    pub pattern: u16,
}

pub(crate) const VARIANT_PATTERNS: Spec = Spec {
    file: "variant_patterns.csv",
    scoped: false,
    columns: &[
        ("variant", true),
        ("service", true),
        ("field", true),
        ("expected", true),
        ("pattern", false),
    ],
};

pub(crate) fn variant_patterns(sheet: &Sheet, errors: &mut Errors) -> Vec<VariantPatternRow> {
    sheet.rows(errors, |row, e| {
        let variant = row.required(e, "variant", values::name);
        let service = row.required(e, "service", values::service_name);
        let field = row.required(e, "field", values::name);
        let expected = row.required(e, "expected", values::bytes);
        let pattern = row.or(e, "pattern", 1, |cell| {
            values::integer(cell, false, 1, u16::MAX.into()).map(|v| v as u16)
        });
        Some(VariantPatternRow {
            line: row.line,
            variant: variant?,
            service: service?,
            field: field?,
            expected: expected?,
            pattern: pattern?,
        })
    })
}

/// A `protocols.csv` row (4.3). Identifiers are 11-bit CAN identifiers.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolRow {
    pub line: Line,
    pub variants: Scope,
    pub name: String,
    pub baud_rate: u32,
    pub tx_id: u16,
    pub rx_id: u16,
    pub functional_id: u16,
    pub extended_addressing: bool,
    /// The application layer the protocol carries, which names the profile the variant's
    /// procedures are lowered through (ADR-309 item 3).
    pub application_layer: String,
}

pub(crate) const PROTOCOLS: Spec = Spec {
    file: "protocols.csv",
    scoped: true,
    columns: &[
        ("name", true),
        ("baud_rate", true),
        ("tx_id", true),
        ("rx_id", true),
        ("functional_id", false),
        ("extended_addressing", false),
        ("application_layer", false),
    ],
};

pub(crate) fn protocols(sheet: &Sheet, errors: &mut Errors) -> Vec<ProtocolRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let name = row.required(e, "name", values::text);
        let baud_rate = row.required(e, "baud_rate", as_u32(1));
        let tx_id = row.required(e, "tx_id", as_u16(0x7FF));
        let rx_id = row.required(e, "rx_id", as_u16(0x7FF));
        let functional_id = row.or(e, "functional_id", 0, as_u16(0x7FF));
        let extended_addressing = row.or(e, "extended_addressing", false, values::boolean);
        let application_layer = row.or(
            e,
            "application_layer",
            crate::profile::UDS.to_owned(),
            values::text,
        );
        Some(ProtocolRow {
            line: row.line,
            variants: variants?,
            name: name?,
            baud_rate: baud_rate?,
            tx_id: tx_id?,
            rx_id: rx_id?,
            functional_id: functional_id?,
            extended_addressing: extended_addressing?,
            application_layer: application_layer?,
        })
    })
}

/// A `comparams.csv` row (4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct ComParamRow {
    pub line: Line,
    pub variants: Scope,
    pub protocol: String,
    pub name: String,
    pub value: i64,
    pub unit: Option<String>,
}

pub(crate) const COMPARAMS: Spec = Spec {
    file: "comparams.csv",
    scoped: true,
    columns: &[
        ("protocol", true),
        ("name", true),
        ("value", true),
        ("unit", false),
    ],
};

pub(crate) fn comparams(sheet: &Sheet, errors: &mut Errors) -> Vec<ComParamRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let protocol = row.required(e, "protocol", values::text);
        let name = row.required(e, "name", values::text);
        let value = row.required(e, "value", |c| {
            values::integer(c, true, i64::MIN.into(), i64::MAX.into()).map(|v| v as i64)
        });
        let unit = row.optional(e, "unit", values::key);
        Some(ComParamRow {
            line: row.line,
            variants: variants?,
            protocol: protocol?,
            name: name?,
            value: value?,
            unit: unit.ok()?,
        })
    })
}

/// A `texts.csv` or `units.csv` row (4.4): a key, the server's id for it, and its default text
/// or unit symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRow {
    pub line: Line,
    pub key: String,
    pub id: u32,
    pub text: String,
}

pub(crate) const TEXTS: Spec = Spec {
    file: "texts.csv",
    scoped: false,
    columns: &[("key", true), ("id", true), ("text", true)],
};

pub(crate) const UNITS: Spec = Spec {
    file: "units.csv",
    scoped: false,
    columns: &[("key", true), ("id", true), ("symbol", false)],
};

pub(crate) fn catalog(sheet: &Sheet, errors: &mut Errors, text_column: &str) -> Vec<CatalogRow> {
    let required = text_column == "text";
    sheet.rows(errors, |row, e| {
        let key = row.required(e, "key", values::key);
        let id = row.required(e, "id", as_u32(1));
        let text = if required {
            row.required(e, text_column, values::text)
        } else {
            row.or(e, text_column, String::new(), values::text)
        };
        Some(CatalogRow {
            line: row.line,
            key: key?,
            id: id?,
            text: text?,
        })
    })
}

// ---------------------------------------------------------------- 4.5 Conversions

/// The kind of a conversion (4.5), with the values of its `conversions.csv` row.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversionKind {
    Identity,
    Linear {
        numerator: f64,
        denominator: f64,
        offset: f64,
    },
    ScaleLinear,
    TextTable {
        default_text: Option<String>,
    },
    Interpolation,
    Rational {
        numerator_coeffs: Vec<f64>,
        denominator_coeffs: Vec<f64>,
    },
}

impl ConversionKind {
    /// The keyword `conversions.csv` writes for the kind.
    pub fn keyword(&self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Linear { .. } => "linear",
            Self::ScaleLinear => "scale-linear",
            Self::TextTable { .. } => "text-table",
            Self::Interpolation => "interpolation",
            Self::Rational { .. } => "rational",
        }
    }
}

/// A `conversions.csv` row (4.5).
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionRow {
    pub line: Line,
    pub name: String,
    pub kind: ConversionKind,
}

pub(crate) const CONVERSIONS: Spec = Spec {
    file: "conversions.csv",
    scoped: false,
    columns: &[
        ("name", true),
        ("kind", true),
        ("numerator", false),
        ("denominator", false),
        ("offset", false),
        ("numerator_coeffs", false),
        ("denominator_coeffs", false),
        ("default_text", false),
    ],
};

#[derive(Clone, Copy)]
enum Kind {
    Identity,
    Linear,
    ScaleLinear,
    TextTable,
    Interpolation,
    Rational,
}

fn nonzero_float(cell: &str) -> Parsed<f64> {
    match values::float(cell)? {
        0.0 => Err("the denominator must not be 0".into()),
        value => Ok(value),
    }
}

fn floats(cell: &str) -> Parsed<Vec<f64>> {
    values::list(cell, false, values::float)
}

pub(crate) fn conversions(sheet: &Sheet, errors: &mut Errors) -> Vec<ConversionRow> {
    sheet.rows(errors, |row, e| {
        let name = row.required(e, "name", values::name);
        let kind = row.required(e, "kind", |c| {
            values::keyword(
                c,
                &[
                    ("identity", Kind::Identity),
                    ("linear", Kind::Linear),
                    ("scale-linear", Kind::ScaleLinear),
                    ("text-table", Kind::TextTable),
                    ("interpolation", Kind::Interpolation),
                    ("rational", Kind::Rational),
                ],
            )
        });
        let Some(kind) = kind else {
            // With no valid kind, each cell is still checked for its own syntax.
            for column in ["numerator", "offset"] {
                row.optional(e, column, values::float).ok();
            }
            row.optional(e, "denominator", nonzero_float).ok();
            for column in ["numerator_coeffs", "denominator_coeffs"] {
                row.optional(e, column, floats).ok();
            }
            row.optional(e, "default_text", values::key).ok();
            return None;
        };
        let kind = match kind {
            Kind::Linear => {
                let numerator = row.or(e, "numerator", 1.0, values::float);
                let denominator = row.or(e, "denominator", 1.0, nonzero_float);
                let offset = row.or(e, "offset", 0.0, values::float);
                for column in ["numerator_coeffs", "denominator_coeffs", "default_text"] {
                    row.unused(e, column, "a linear conversion");
                }
                ConversionKind::Linear {
                    numerator: numerator?,
                    denominator: denominator?,
                    offset: offset?,
                }
            }
            Kind::Rational => {
                let numerator_coeffs = row.required(e, "numerator_coeffs", floats);
                let denominator_coeffs = row.required(e, "denominator_coeffs", |c| {
                    let coeffs = floats(c)?;
                    if coeffs.iter().all(|&d| d == 0.0) {
                        return Err("the denominator coefficients are all 0".into());
                    }
                    Ok(coeffs)
                });
                for column in ["numerator", "denominator", "offset", "default_text"] {
                    row.unused(e, column, "a rational conversion");
                }
                ConversionKind::Rational {
                    numerator_coeffs: numerator_coeffs?,
                    denominator_coeffs: denominator_coeffs?,
                }
            }
            Kind::TextTable => {
                let default_text = row.optional(e, "default_text", values::key);
                for column in [
                    "numerator",
                    "denominator",
                    "offset",
                    "numerator_coeffs",
                    "denominator_coeffs",
                ] {
                    row.unused(e, column, "a text-table conversion");
                }
                ConversionKind::TextTable {
                    default_text: default_text.ok()?,
                }
            }
            kind @ (Kind::Identity | Kind::ScaleLinear | Kind::Interpolation) => {
                let (converted, label) = match kind {
                    Kind::Identity => (ConversionKind::Identity, "an identity conversion"),
                    Kind::ScaleLinear => (ConversionKind::ScaleLinear, "a scale-linear conversion"),
                    _ => (ConversionKind::Interpolation, "an interpolation"),
                };
                for column in [
                    "numerator",
                    "denominator",
                    "offset",
                    "numerator_coeffs",
                    "denominator_coeffs",
                    "default_text",
                ] {
                    row.unused(e, column, label);
                }
                converted
            }
        };
        Some(ConversionRow {
            line: row.line,
            name: name?,
            kind,
        })
    })
}

/// A `conversion_scales.csv` row (4.5). Which cells it may use, and whether `lower` and `upper`
/// are integers or floats, depends on its conversion's kind in `conversions.csv`, so they are
/// kept as written and checked with the conversion.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionScaleRow {
    pub line: Line,
    pub conversion: String,
    pub lower: Option<String>,
    pub upper: Option<String>,
    pub numerator: Option<f64>,
    pub denominator: Option<f64>,
    pub offset: Option<f64>,
    pub text: Option<String>,
    pub internal: Option<f64>,
    pub physical: Option<f64>,
}

pub(crate) const CONVERSION_SCALES: Spec = Spec {
    file: "conversion_scales.csv",
    scoped: false,
    columns: &[
        ("conversion", true),
        ("lower", false),
        ("upper", false),
        ("numerator", false),
        ("denominator", false),
        ("offset", false),
        ("text", false),
        ("internal", false),
        ("physical", false),
    ],
};

pub(crate) fn conversion_scales(sheet: &Sheet, errors: &mut Errors) -> Vec<ConversionScaleRow> {
    sheet.rows(errors, |row, e| {
        let conversion = row.required(e, "conversion", values::name);
        let lower = row.cell("lower").map(str::to_owned);
        let upper = row.cell("upper").map(str::to_owned);
        let numerator = row.optional(e, "numerator", values::float);
        let denominator = row.optional(e, "denominator", nonzero_float);
        let offset = row.optional(e, "offset", values::float);
        let text = row.optional(e, "text", values::key);
        let internal = row.optional(e, "internal", values::float);
        let physical = row.optional(e, "physical", values::float);
        Some(ConversionScaleRow {
            line: row.line,
            conversion: conversion?,
            lower,
            upper,
            numerator: numerator.ok()?,
            denominator: denominator.ok()?,
            offset: offset.ok()?,
            text: text.ok()?,
            internal: internal.ok()?,
            physical: physical.ok()?,
        })
    })
}

// ---------------------------------------------------------------- 4.6 to 4.9 Services

/// A service's timing (4.6), shared by DIDs and routines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub p2_ms: u32,
    pub p2_star_ms: u32,
}

fn timing(row: &mut Row<'_>, errors: &mut Errors) -> Option<Timing> {
    let p2_ms = row.or(errors, "p2_ms", 50, as_u32(1));
    let p2_star_ms = row.or(errors, "p2_star_ms", 5000, as_u32(1));
    Some(Timing {
        p2_ms: p2_ms?,
        p2_star_ms: p2_star_ms?,
    })
}

/// A `services.csv` row (4.6).
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRow {
    pub line: Line,
    pub variants: Scope,
    pub name: String,
    pub sid: u8,
    pub session: u8,
    pub security: u8,
    pub timing: Timing,
}

pub(crate) const SERVICES: Spec = Spec {
    file: "services.csv",
    scoped: true,
    columns: &[
        ("name", true),
        ("sid", true),
        ("session", false),
        ("security", false),
        ("p2_ms", false),
        ("p2_star_ms", false),
    ],
};

pub(crate) fn services(sheet: &Sheet, errors: &mut Errors) -> Vec<ServiceRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let name = row.required(e, "name", values::name);
        let sid = row.required(e, "sid", |c| {
            let sid = as_u8(0x00, 0xBF)(c)?;
            if sid == 0x3F {
                return Err("0x3F is not a request SID (0x7F is the negative response)".into());
            }
            Ok(sid)
        });
        let session = row.or(e, "session", 0, as_u8(0, 0xFF));
        let security = row.or(e, "security", 0, as_u8(0, 0xFF));
        let timing = timing(row, e);
        Some(ServiceRow {
            line: row.line,
            variants: variants?,
            name: name?,
            sid: sid?,
            session: session?,
            security: security?,
            timing: timing?,
        })
    })
}

/// A `dids.csv` row (4.7): the services it generates and their requirements.
#[derive(Debug, Clone, PartialEq)]
pub struct DidRow {
    pub line: Line,
    pub variants: Scope,
    pub name: String,
    pub did: u16,
    pub read: bool,
    pub write: bool,
    pub read_session: u8,
    pub read_security: u8,
    pub write_session: u8,
    pub write_security: u8,
    pub timing: Timing,
}

pub(crate) const DIDS: Spec = Spec {
    file: "dids.csv",
    scoped: true,
    columns: &[
        ("name", true),
        ("did", true),
        ("read", false),
        ("write", false),
        ("read_session", false),
        ("read_security", false),
        ("write_session", false),
        ("write_security", false),
        ("p2_ms", false),
        ("p2_star_ms", false),
    ],
};

pub(crate) fn dids(sheet: &Sheet, errors: &mut Errors) -> Vec<DidRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let name = row.required(e, "name", values::name);
        let did = row.required(e, "did", as_u16(0xFFFF));
        let read = row.or(e, "read", true, values::boolean);
        let write = row.or(e, "write", false, values::boolean);
        let byte = as_u8(0, 0xFF);
        let read_session = row.or(e, "read_session", 0, &byte);
        let read_security = row.or(e, "read_security", 0, &byte);
        let write_session = row.or(e, "write_session", 0, &byte);
        let write_security = row.or(e, "write_security", 0, &byte);
        let timing = timing(row, e);
        if read == Some(false) && write == Some(false) {
            row.error(
                e,
                ErrorCode::MissingColumn,
                "read",
                "the DID generates neither a read nor a write service",
            );
        }
        Some(DidRow {
            line: row.line,
            variants: variants?,
            name: name?,
            did: did?,
            read: read?,
            write: write?,
            read_session: read_session?,
            read_security: read_security?,
            write_session: write_session?,
            write_security: write_security?,
            timing: timing?,
        })
    })
}

/// A `routines.csv` row (4.8).
#[derive(Debug, Clone, PartialEq)]
pub struct RoutineRow {
    pub line: Line,
    pub variants: Scope,
    pub name: String,
    pub rid: u16,
    pub start: bool,
    pub stop: bool,
    pub results: bool,
    pub session: u8,
    pub security: u8,
    pub timing: Timing,
}

pub(crate) const ROUTINES: Spec = Spec {
    file: "routines.csv",
    scoped: true,
    columns: &[
        ("name", true),
        ("rid", true),
        ("start", false),
        ("stop", false),
        ("results", false),
        ("session", false),
        ("security", false),
        ("p2_ms", false),
        ("p2_star_ms", false),
    ],
};

pub(crate) fn routines(sheet: &Sheet, errors: &mut Errors) -> Vec<RoutineRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let name = row.required(e, "name", values::name);
        let rid = row.required(e, "rid", as_u16(0xFFFF));
        let start = row.or(e, "start", true, values::boolean);
        let stop = row.or(e, "stop", false, values::boolean);
        let results = row.or(e, "results", false, values::boolean);
        let session = row.or(e, "session", 0, as_u8(0, 0xFF));
        let security = row.or(e, "security", 0, as_u8(0, 0xFF));
        let timing = timing(row, e);
        if (start, stop, results) == (Some(false), Some(false), Some(false)) {
            row.error(
                e,
                ErrorCode::MissingColumn,
                "start",
                "the routine supports none of start, stop and results",
            );
        }
        Some(RoutineRow {
            line: row.line,
            variants: variants?,
            name: name?,
            rid: rid?,
            start: start?,
            stop: stop?,
            results: results?,
            session: session?,
            security: security?,
            timing: timing?,
        })
    })
}

/// A `nrcs.csv` row (4.9).
#[derive(Debug, Clone, PartialEq)]
pub struct NrcRow {
    pub line: Line,
    pub variants: Scope,
    /// `None`: every service of the variant's document.
    pub service: Option<ServiceName>,
    pub nrc: u8,
    pub text: Option<String>,
    pub retryable: bool,
}

pub(crate) const NRCS: Spec = Spec {
    file: "nrcs.csv",
    scoped: true,
    columns: &[
        ("service", false),
        ("nrc", true),
        ("text", false),
        ("retryable", false),
    ],
};

pub(crate) fn nrcs(sheet: &Sheet, errors: &mut Errors) -> Vec<NrcRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let service = row.optional(e, "service", values::service_name);
        let nrc = row.required(e, "nrc", as_u8(0x01, 0xFF));
        let text = row.optional(e, "text", values::key);
        let retryable = row.or(e, "retryable", false, values::boolean);
        Some(NrcRow {
            line: row.line,
            variants: variants?,
            service: service.ok()?,
            nrc: nrc?,
            text: text.ok()?,
            retryable: retryable?,
        })
    })
}

// ---------------------------------------------------------------- 4.10 to 4.12

/// A `layouts.csv` row (4.10).
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutRow {
    pub line: Line,
    pub name: String,
    pub byte_size: u32,
}

pub(crate) const LAYOUTS: Spec = Spec {
    file: "layouts.csv",
    scoped: false,
    columns: &[("name", true), ("byte_size", false)],
};

pub(crate) fn layouts(sheet: &Sheet, errors: &mut Errors) -> Vec<LayoutRow> {
    sheet.rows(errors, |row, e| {
        let name = row.required(e, "name", values::name);
        let byte_size = row.or(e, "byte_size", 0, as_u32(0));
        Some(LayoutRow {
            line: row.line,
            name: name?,
            byte_size: byte_size?,
        })
    })
}

/// A `dtcs.csv` row (4.11).
#[derive(Debug, Clone, PartialEq)]
pub struct DtcRow {
    pub line: Line,
    pub variants: Scope,
    pub code: u32,
    pub display: String,
    pub text: Option<String>,
    pub status: String,
    pub snapshot: Option<String>,
    pub extended: Option<String>,
}

pub(crate) const DTCS: Spec = Spec {
    file: "dtcs.csv",
    scoped: true,
    columns: &[
        ("code", true),
        ("display", true),
        ("text", false),
        ("status", true),
        ("snapshot", false),
        ("extended", false),
    ],
};

pub(crate) fn dtcs(sheet: &Sheet, errors: &mut Errors) -> Vec<DtcRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let code = row.required(e, "code", unsigned(0, 0xFF_FFFF));
        let display = row.required(e, "display", values::text);
        let text = row.optional(e, "text", values::key);
        let status = row.required(e, "status", values::name);
        let snapshot = row.optional(e, "snapshot", values::name);
        let extended = row.optional(e, "extended", values::name);
        Some(DtcRow {
            line: row.line,
            variants: variants?,
            code: code? as u32,
            display: display?,
            text: text.ok()?,
            status: status?,
            snapshot: snapshot.ok()?,
            extended: extended.ok()?,
        })
    })
}

/// A flash step (4.12, `ir.fbs` `FlashStep`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlashStep {
    EnterProgrammingSession,
    SecurityAccess,
    EraseMemory,
    RequestDownload,
    TransferData,
    TransferExit,
    CheckMemory,
    EcuReset,
}

/// A `flash_sessions.csv` row (4.12).
#[derive(Debug, Clone, PartialEq)]
pub struct FlashSessionRow {
    pub line: Line,
    pub variants: Scope,
    pub id: u32,
    pub name: String,
    pub steps: Vec<FlashStep>,
    /// The plan's recovery facts (ADR-336 item 1): `RecoveryTiming` and the plan's
    /// `version_read_retries` and `no_application`. A plan that names the session needs the
    /// first five; the table does not, since a session no plan names has no use for them.
    pub session_timeout_ms: Option<u32>,
    pub teardown_margin_ms: Option<u32>,
    pub ecu_startup_ms: Option<u32>,
    pub confirmation_window_ms: Option<u32>,
    pub version_read_retries: Option<u8>,
    pub no_application_nrc: Option<u8>,
}

pub(crate) const FLASH_SESSIONS: Spec = Spec {
    file: "flash_sessions.csv",
    scoped: true,
    columns: &[
        ("id", true),
        ("name", true),
        ("steps", true),
        ("session_timeout_ms", false),
        ("teardown_margin_ms", false),
        ("ecu_startup_ms", false),
        ("confirmation_window_ms", false),
        ("version_read_retries", false),
        ("no_application_nrc", false),
    ],
};

pub(crate) fn flash_sessions(sheet: &Sheet, errors: &mut Errors) -> Vec<FlashSessionRow> {
    let step = |cell: &str| {
        values::keyword(
            cell,
            &[
                (
                    "enter-programming-session",
                    FlashStep::EnterProgrammingSession,
                ),
                ("security-access", FlashStep::SecurityAccess),
                ("erase-memory", FlashStep::EraseMemory),
                ("request-download", FlashStep::RequestDownload),
                ("transfer-data", FlashStep::TransferData),
                ("transfer-exit", FlashStep::TransferExit),
                ("check-memory", FlashStep::CheckMemory),
                ("ecu-reset", FlashStep::EcuReset),
            ],
        )
    };
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let id = row.required(e, "id", as_u32(1));
        let name = row.required(e, "name", values::name);
        let steps = row.required(e, "steps", |c| values::list(c, true, step));
        let session_timeout_ms = row.optional(e, "session_timeout_ms", as_u32(1));
        let teardown_margin_ms = row.optional(e, "teardown_margin_ms", as_u32(0));
        let ecu_startup_ms = row.optional(e, "ecu_startup_ms", as_u32(0));
        let confirmation_window_ms = row.optional(e, "confirmation_window_ms", as_u32(0));
        let version_read_retries = row.optional(e, "version_read_retries", as_u8(0, 0xFF));
        // 0x78 is a response pending, which the worker absorbs, so it never means "no
        // application"; `Program::validate` refuses it as well.
        let no_application_nrc = row.optional(e, "no_application_nrc", |c| {
            let nrc = as_u8(0x01, 0xFF)(c)?;
            if nrc == 0x78 {
                return Err(
                    "0x78 is a response pending, which is not a no-application answer".into(),
                );
            }
            Ok(nrc)
        });
        Some(FlashSessionRow {
            line: row.line,
            variants: variants?,
            id: id?,
            name: name?,
            steps: steps?,
            session_timeout_ms: session_timeout_ms.ok()?,
            teardown_margin_ms: teardown_margin_ms.ok()?,
            ecu_startup_ms: ecu_startup_ms.ok()?,
            confirmation_window_ms: confirmation_window_ms.ok()?,
            version_read_retries: version_read_retries.ok()?,
            no_application_nrc: no_application_nrc.ok()?,
        })
    })
}

/// A `flash_segments.csv` row (4.12).
#[derive(Debug, Clone, PartialEq)]
pub struct FlashSegmentRow {
    pub line: Line,
    pub session: String,
    pub name: String,
    pub start_address: u64,
    pub length: u64,
    pub artifact_offset: u64,
    pub block_size: u32,
    pub checksum: Vec<u8>,
    /// The dataFormatIdentifier of the RequestDownload built from the segment (ADR-336 item 1).
    pub data_format: u8,
    /// The addressAndLengthFormatIdentifier of that RequestDownload.
    pub address_and_length_format: u8,
}

pub(crate) const FLASH_SEGMENTS: Spec = Spec {
    file: "flash_segments.csv",
    scoped: false,
    columns: &[
        ("session", true),
        ("name", true),
        ("start_address", true),
        ("length", true),
        ("artifact_offset", true),
        ("block_size", true),
        ("checksum", false),
        ("data_format", false),
        ("address_and_length_format", false),
    ],
};

pub(crate) fn flash_segments(sheet: &Sheet, errors: &mut Errors) -> Vec<FlashSegmentRow> {
    sheet.rows(errors, |row, e| {
        let session = row.required(e, "session", values::name);
        let name = row.required(e, "name", values::name);
        let start_address = row.required(e, "start_address", as_u64(0));
        let length = row.required(e, "length", as_u64(1));
        let artifact_offset = row.required(e, "artifact_offset", as_u64(0));
        let block_size = row.required(e, "block_size", as_u32(1));
        let checksum = row.or(e, "checksum", Vec::new(), values::bytes);
        let data_format = row.or(e, "data_format", 0x00, as_u8(0, 0xFF));
        let address_and_length_format =
            row.or(e, "address_and_length_format", 0x44, as_u8(0, 0xFF));
        // A RequestDownload needs a memory address and a memory size, so neither nibble of the
        // format may be 0 (ISO 14229-1 (2026) 14.2.2.3).
        if let Some(format) = address_and_length_format
            && (format & 0x0F == 0 || format >> 4 == 0)
        {
            row.error(
                e,
                ErrorCode::BadValue,
                "address_and_length_format",
                "a nibble of the format is 0: the request would carry no memory address or size",
            );
        }
        if let Some(length) = length {
            for (column, start) in [
                ("start_address", start_address),
                ("artifact_offset", artifact_offset),
            ] {
                if start.is_some_and(|s| u128::from(s) + u128::from(length) > 1 << 64) {
                    row.error(
                        e,
                        ErrorCode::BadValue,
                        column,
                        format!("{column} + length exceeds 2^64"),
                    );
                }
            }
        }
        Some(FlashSegmentRow {
            line: row.line,
            session: session?,
            name: name?,
            start_address: start_address?,
            length: length?,
            artifact_offset: artifact_offset?,
            block_size: block_size?,
            checksum: checksum?,
            data_format: data_format?,
            address_and_length_format: address_and_length_format?,
        })
    })
}

// ---------------------------------------------------------------- 4.13, 4.14 Restart

/// The name `identity.csv` gives an identity kind.
pub fn identity_name(kind: IdentityKind) -> &'static str {
    match kind {
        IdentityKind::Vin => "vin",
        IdentityKind::HardwarePartNumber => "hardware_part_number",
        IdentityKind::SoftwareVersion => "software_version",
    }
}

/// The name `preconditions.csv` gives a precondition kind.
pub fn precondition_name(kind: PreconditionKind) -> &'static str {
    match kind {
        PreconditionKind::Voltage => "voltage",
        PreconditionKind::ExternalSupply => "external_supply",
        PreconditionKind::Ignition => "ignition",
        PreconditionKind::Engine => "engine",
        PreconditionKind::VehicleSpeed => "vehicle_speed",
    }
}

/// An `identity.csv` row (4.13).
#[derive(Debug, Clone, PartialEq)]
pub struct IdentityRow {
    pub line: Line,
    pub variants: Scope,
    pub kind: IdentityKind,
    /// The service as written; resolving checks that it is a generated DID read.
    pub service: ServiceName,
    pub field: String,
}

pub(crate) const IDENTITY: Spec = Spec {
    file: "identity.csv",
    scoped: true,
    columns: &[("kind", true), ("service", true), ("field", true)],
};

pub(crate) fn identity(sheet: &Sheet, errors: &mut Errors) -> Vec<IdentityRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let kind = row.required(e, "kind", |c| {
            values::keyword(
                c,
                &[
                    ("vin", IdentityKind::Vin),
                    ("hardware_part_number", IdentityKind::HardwarePartNumber),
                    ("software_version", IdentityKind::SoftwareVersion),
                ],
            )
        });
        let service = row.required(e, "service", values::service_name);
        let field = row.required(e, "field", values::name);
        Some(IdentityRow {
            line: row.line,
            variants: variants?,
            kind: kind?,
            service: service?,
            field: field?,
        })
    })
}

/// Where a precondition's value is read (4.14).
#[derive(Debug, Clone, PartialEq)]
pub enum SourceRef {
    /// `<service>.<field>`: a field of a service's response.
    Service { service: ServiceName, field: String },
    /// `input:<name>`: a value the agent supplies.
    Input(RuntimeInput),
}

/// The inputs `input:<name>` can name, with the precondition each reports.
const INPUTS: [(&str, RuntimeInput); 5] = [
    ("supply_voltage_mv", RuntimeInput::SupplyVoltageMillivolts),
    ("external_supply", RuntimeInput::ExternalSupplyConnected),
    ("ignition_on", RuntimeInput::IgnitionOn),
    ("engine_running", RuntimeInput::EngineRunning),
    ("vehicle_speed_kmh", RuntimeInput::VehicleSpeedKmh),
];

fn source(cell: &str) -> Parsed<SourceRef> {
    if let Some(name) = cell.strip_prefix("input:") {
        return values::keyword(name, &INPUTS).map(SourceRef::Input);
    }
    // The service is everything before the last dot: `vin.read.vin` is service `vin.read`.
    let (service, field) = cell
        .rsplit_once('.')
        .ok_or_else(|| format!("{cell:?} is neither <service>.<field> nor input:<name>"))?;
    Ok(SourceRef::Service {
        service: values::service_name(service)?,
        field: values::name(field)?,
    })
}

/// A `preconditions.csv` row (4.14).
#[derive(Debug, Clone, PartialEq)]
pub struct PreconditionRow {
    pub line: Line,
    pub variants: Scope,
    pub kind: PreconditionKind,
    pub lower: i64,
    pub upper: i64,
    pub default_source: SourceRef,
    pub programming_source: Option<SourceRef>,
}

pub(crate) const PRECONDITIONS: Spec = Spec {
    file: "preconditions.csv",
    scoped: true,
    columns: &[
        ("kind", true),
        ("lower", true),
        ("upper", true),
        ("default_source", true),
        ("programming_source", false),
    ],
};

pub(crate) fn preconditions(sheet: &Sheet, errors: &mut Errors) -> Vec<PreconditionRow> {
    sheet.rows(errors, |row, e| {
        let variants = scope(row, e);
        let kind = row.required(e, "kind", |c| {
            values::keyword(
                c,
                &[
                    ("voltage", PreconditionKind::Voltage),
                    ("external_supply", PreconditionKind::ExternalSupply),
                    ("ignition", PreconditionKind::Ignition),
                    ("engine", PreconditionKind::Engine),
                    ("vehicle_speed", PreconditionKind::VehicleSpeed),
                ],
            )
        });
        // A flag is 0 or 1 (`Program::validate` refuses anything else).
        let flag = matches!(
            kind,
            Some(
                PreconditionKind::ExternalSupply
                    | PreconditionKind::Ignition
                    | PreconditionKind::Engine
            )
        );
        let (min, max) = if flag {
            (0, 1)
        } else {
            (i64::MIN.into(), i64::MAX.into())
        };
        let bound = |c: &str| values::integer(c, true, min, max).map(|v| v as i64);
        let lower = row.required(e, "lower", bound);
        let upper = row.required(e, "upper", bound);
        if let (Some(lower), Some(upper)) = (lower, upper)
            && lower > upper
        {
            row.error(
                e,
                ErrorCode::BadValue,
                "upper",
                format!("upper {upper} is below lower {lower}, which leaves no satisfying value"),
            );
        }
        let default_source = row.required(e, "default_source", source);
        let programming_source = row.optional(e, "programming_source", source);
        // `input:<name>` is the input of the row's own kind only.
        if let Some(kind) = kind {
            let sources = [
                ("default_source", default_source.as_ref()),
                (
                    "programming_source",
                    programming_source.as_ref().ok().and_then(Option::as_ref),
                ),
            ];
            for (column, found) in sources {
                if let Some(SourceRef::Input(input)) = found
                    && !input.reports(kind)
                {
                    let own = INPUTS
                        .iter()
                        .find(|(_, i)| i.reports(kind))
                        .map_or("", |(name, _)| *name);
                    row.error(
                        e,
                        ErrorCode::BadValue,
                        column,
                        format!(
                            "{} reports another fact than {}; use input:{own}",
                            INPUTS
                                .iter()
                                .find(|(_, i)| i == input)
                                .map_or("", |(name, _)| *name),
                            precondition_name(kind),
                        ),
                    );
                }
            }
        }
        Some(PreconditionRow {
            line: row.line,
            variants: variants?,
            kind: kind?,
            lower: lower?,
            upper: upper?,
            default_source: default_source?,
            programming_source: programming_source.ok()?,
        })
    })
}

// ---------------------------------------------------------------- 4.15 Definition

/// The `definition.csv` row (4.15): the identifier a vehicle definition names the ECU
/// definition by (ADR-363).
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionRow {
    pub line: Line,
    pub id: String,
}

pub(crate) const DEFINITION: Spec = Spec {
    file: "definition.csv",
    scoped: false,
    columns: &[("id", true)],
};

pub(crate) fn definition(sheet: &Sheet, errors: &mut Errors) -> Vec<DefinitionRow> {
    sheet.rows(errors, |row, e| {
        let id = row.required(e, "id", values::name);
        Some(DefinitionRow {
            line: row.line,
            id: id?,
        })
    })
}

// ---------------------------------------------------------------- 5 Fields

/// Which part of a routine a `routine:` container holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoutinePart {
    Start,
    Stop,
    Results,
}

/// A field container (5.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Container {
    /// `request:<service>`.
    Request(String),
    /// `response:<service>`.
    Response(String),
    /// `did:<did>`: the record of a DID.
    Did(String),
    /// `routine:<routine>:<part>:<request|response>`; `response` is true for the response.
    Routine {
        routine: String,
        part: RoutinePart,
        response: bool,
    },
    /// `layout:<layout>`.
    Layout(String),
}

impl Container {
    /// The generated constants of a container, whose names its fields may not take (4.7, 4.8).
    pub fn generated_constants(&self) -> &'static [&'static str] {
        match self {
            Self::Did(_) => &["did"],
            Self::Routine { .. } => &["sub", "rid"],
            _ => &[],
        }
    }
}

pub(crate) fn container(cell: &str) -> Parsed<Container> {
    let bad = || format!("{cell:?} is not a container");
    let parts: Vec<&str> = cell.split(':').collect();
    let name = |s: &str| values::name(s).map_err(|_| bad());
    match parts.as_slice() {
        ["request", n] => Ok(Container::Request(name(n)?)),
        ["response", n] => Ok(Container::Response(name(n)?)),
        ["did", n] => Ok(Container::Did(name(n)?)),
        ["layout", n] => Ok(Container::Layout(name(n)?)),
        ["routine", n, part, direction] => Ok(Container::Routine {
            routine: name(n)?,
            part: match *part {
                "start" => RoutinePart::Start,
                "stop" => RoutinePart::Stop,
                "results" => RoutinePart::Results,
                _ => return Err(bad()),
            },
            response: match *direction {
                "request" => false,
                "response" => true,
                _ => return Err(bad()),
            },
        }),
        _ => Err(bad()),
    }
}

/// A field's type (5.2, `ir.fbs` `DataKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    Uint,
    Int,
    Float,
    Ascii,
    Bytes,
    Bcd,
    Utf8,
    Unicode2,
}

/// How a field's length is found (5.2, `ir.fbs` `LengthKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Length {
    Fixed,
    Field,
    Leading,
    Terminated,
}

/// What ends a `terminated` field (5.2, `ir.fbs` `Termination`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Terminator {
    End,
    Zero,
    Ff,
}

/// A field's shape (5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    Group,
    Repeat,
    Select,
}

/// A `fields.csv` row (5.2). Only each cell's own syntax and range are checked here; the field
/// rules of section 5 (which cells a length kind or shape uses, widths per type, constants,
/// positions) compare cells with each other and with other tables. `const` is kept as written,
/// since whether it is an integer or bytes depends on the type.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRow {
    pub line: Line,
    pub container: Container,
    pub name: String,
    pub byte: Option<u32>,
    pub bit: u8,
    pub field_type: Option<FieldType>,
    pub bits: Option<u32>,
    /// `None`: the cell is empty, which means `fixed` except where a field takes no length
    /// at all (a shape, 5.4).
    pub length: Option<Length>,
    pub min_bytes: Option<u32>,
    pub max_bytes: Option<u32>,
    pub terminator: Option<Terminator>,
    pub big_endian: bool,
    pub conversion: Option<String>,
    pub unit: Option<String>,
    pub constant: Option<String>,
    pub shape: Option<Shape>,
    pub layout: Option<String>,
    pub reference: Option<String>,
}

pub(crate) const FIELDS: Spec = Spec {
    file: "fields.csv",
    scoped: false,
    columns: &[
        ("container", true),
        ("name", true),
        ("byte", false),
        ("bit", false),
        ("type", false),
        ("bits", false),
        ("length", false),
        ("min_bytes", false),
        ("max_bytes", false),
        ("terminator", false),
        ("byte_order", false),
        ("conversion", false),
        ("unit", false),
        ("const", false),
        ("shape", false),
        ("layout", false),
        ("ref", false),
    ],
};

pub(crate) fn fields(sheet: &Sheet, errors: &mut Errors) -> Vec<FieldRow> {
    sheet.rows(errors, |row, e| {
        let container = row.required(e, "container", container);
        let name = row.required(e, "name", values::name);
        let byte = row.optional(e, "byte", |c| {
            values::integer(c, false, 0, (1 << 29) - 1).map(|v| v as u32)
        });
        let bit = row.or(e, "bit", 0, as_u8(0, 7));
        let shape = row.optional(e, "shape", |c| {
            values::keyword(
                c,
                &[
                    ("group", Shape::Group),
                    ("repeat", Shape::Repeat),
                    ("select", Shape::Select),
                ],
            )
        });
        let field_type = row.optional(e, "type", |c| {
            values::keyword(
                c,
                &[
                    ("uint", FieldType::Uint),
                    ("int", FieldType::Int),
                    ("float", FieldType::Float),
                    ("ascii", FieldType::Ascii),
                    ("bytes", FieldType::Bytes),
                    ("bcd", FieldType::Bcd),
                    ("utf8", FieldType::Utf8),
                    ("unicode2", FieldType::Unicode2),
                ],
            )
        });
        if field_type == Ok(None) && matches!(shape, Ok(None)) {
            row.error(
                e,
                ErrorCode::MissingColumn,
                "type",
                "type is required for a field without a shape",
            );
        }
        let bits = row.optional(e, "bits", as_u32(1));
        let length = row.optional(e, "length", |c| {
            values::keyword(
                c,
                &[
                    ("fixed", Length::Fixed),
                    ("field", Length::Field),
                    ("leading", Length::Leading),
                    ("terminated", Length::Terminated),
                ],
            )
        });
        let min_bytes = row.optional(e, "min_bytes", as_u32(0));
        let max_bytes = row.optional(e, "max_bytes", as_u32(0));
        let terminator = row.optional(e, "terminator", |c| {
            values::keyword(
                c,
                &[
                    ("end", Terminator::End),
                    ("zero", Terminator::Zero),
                    ("ff", Terminator::Ff),
                ],
            )
        });
        let big_endian = row.or(e, "byte_order", true, |c| {
            values::keyword(c, &[("big", true), ("little", false)])
        });
        let conversion = row.optional(e, "conversion", values::name);
        let unit = row.optional(e, "unit", values::key);
        let constant = row.cell("const").map(str::to_owned);
        let layout = row.optional(e, "layout", values::name);
        let reference = row.optional(e, "ref", values::name);
        let container = container?;
        let name = name?;
        if container.generated_constants().contains(&name.as_str()) {
            row.error(
                e,
                ErrorCode::Duplicate,
                "name",
                format!("{name} is the name of a constant the container generates"),
            );
        }
        Some(FieldRow {
            line: row.line,
            container,
            name,
            byte: byte.ok()?,
            bit: bit?,
            field_type: field_type.ok()?,
            bits: bits.ok()?,
            length: length.ok()?,
            min_bytes: min_bytes.ok()?,
            max_bytes: max_bytes.ok()?,
            terminator: terminator.ok()?,
            big_endian: big_endian?,
            conversion: conversion.ok()?,
            unit: unit.ok()?,
            constant,
            shape: shape.ok()?,
            layout: layout.ok()?,
            reference: reference.ok()?,
        })
    })
}

/// How a repetition is counted (5.4, `ir.fbs` `RepeatCount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatCount {
    /// `fixed`: `items` items, `item_bytes` bytes apart.
    Fixed { items: u32, item_bytes: u32 },
    /// `field`: the count is the field in the `repeat` field's `ref`.
    Field,
    /// `marker`: until a `bits`-wide big-endian marker within `lower..=upper`.
    Marker { bits: u8, lower: u64, upper: u64 },
    /// `end`: until the end of the message.
    End,
}

/// A `repeats.csv` row (5.4).
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatRow {
    pub line: Line,
    pub container: Container,
    pub field: String,
    pub count: RepeatCount,
}

pub(crate) const REPEATS: Spec = Spec {
    file: "repeats.csv",
    scoped: false,
    columns: &[
        ("container", true),
        ("field", true),
        ("count", true),
        ("items", false),
        ("item_bytes", false),
        ("marker_bits", false),
        ("marker_lower", false),
        ("marker_upper", false),
    ],
};

#[derive(Clone, Copy)]
enum Count {
    Fixed,
    Field,
    Marker,
    End,
}

fn marker_bits(cell: &str) -> Parsed<u8> {
    let bits = as_u8(8, 64)(cell)?;
    if bits % 8 != 0 {
        return Err(format!("{bits} is not a multiple of 8"));
    }
    Ok(bits)
}

const FIXED_COLUMNS: [&str; 2] = ["items", "item_bytes"];
const MARKER_COLUMNS: [&str; 3] = ["marker_bits", "marker_lower", "marker_upper"];

pub(crate) fn repeats(sheet: &Sheet, errors: &mut Errors) -> Vec<RepeatRow> {
    sheet.rows(errors, |row, e| {
        let container = row.required(e, "container", container);
        let field = row.required(e, "field", values::name);
        let count = row.required(e, "count", |c| {
            values::keyword(
                c,
                &[
                    ("fixed", Count::Fixed),
                    ("field", Count::Field),
                    ("marker", Count::Marker),
                    ("end", Count::End),
                ],
            )
        });
        let Some(count) = count else {
            // With no valid count, each cell is still checked for its own syntax.
            row.optional(e, "items", as_u32(1)).ok();
            row.optional(e, "item_bytes", as_u32(1)).ok();
            row.optional(e, "marker_bits", marker_bits).ok();
            for column in ["marker_lower", "marker_upper"] {
                row.optional(e, column, as_u64(0)).ok();
            }
            return None;
        };
        let count = match count {
            Count::Fixed => {
                let items = row.required(e, "items", as_u32(1));
                let item_bytes = row.required(e, "item_bytes", as_u32(1));
                for column in MARKER_COLUMNS {
                    row.unused(e, column, "a fixed count");
                }
                RepeatCount::Fixed {
                    items: items?,
                    item_bytes: item_bytes?,
                }
            }
            Count::Marker => {
                for column in FIXED_COLUMNS {
                    row.unused(e, column, "a marker count");
                }
                let bits = row.required(e, "marker_bits", marker_bits);
                // Without a valid width the limits are still checked against the widest one.
                let max = match bits {
                    Some(bits) if bits < 64 => (1u64 << bits) - 1,
                    _ => u64::MAX,
                };
                let limit = |c: &str| values::integer(c, false, 0, max.into()).map(|v| v as u64);
                let lower = row.required(e, "marker_lower", limit);
                let upper = row.optional(e, "marker_upper", limit);
                // The limits are compared even when the width is wrong.
                let lower = lower?;
                let upper = upper.ok()?.unwrap_or(lower);
                if lower > upper {
                    row.error(
                        e,
                        ErrorCode::BadValue,
                        "marker_upper",
                        "marker_upper is below marker_lower",
                    );
                } else if lower < 1 << 63 && upper >= 1 << 63 {
                    row.error(
                        e,
                        ErrorCode::BadValue,
                        "marker_upper",
                        "the marker range crosses 2^63, so its limits would not be in order as int64",
                    );
                }
                RepeatCount::Marker {
                    bits: bits?,
                    lower,
                    upper,
                }
            }
            kind @ (Count::Field | Count::End) => {
                let label = if matches!(kind, Count::Field) {
                    "a count from a field"
                } else {
                    "a count to the end of the message"
                };
                for column in FIXED_COLUMNS.iter().chain(&MARKER_COLUMNS) {
                    row.unused(e, column, label);
                }
                if matches!(kind, Count::Field) {
                    RepeatCount::Field
                } else {
                    RepeatCount::End
                }
            }
        };
        Some(RepeatRow {
            line: row.line,
            container: container?,
            field: field?,
            count,
        })
    })
}

/// A `select_cases.csv` row (5.4). The limits are checked against the widest range a key can
/// have, -2^63 to 2^64 - 1; the key's own type and width narrow it with the field.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectCaseRow {
    pub line: Line,
    pub container: Container,
    pub field: String,
    pub case: String,
    /// `None` for the default case.
    pub range: Option<(i128, i128)>,
    pub layout: Option<String>,
}

pub(crate) const SELECT_CASES: Spec = Spec {
    file: "select_cases.csv",
    scoped: false,
    columns: &[
        ("container", true),
        ("field", true),
        ("case", true),
        ("lower", false),
        ("upper", false),
        ("layout", false),
    ],
};

pub(crate) fn select_cases(sheet: &Sheet, errors: &mut Errors) -> Vec<SelectCaseRow> {
    let limit = |c: &str| values::integer(c, true, i64::MIN.into(), u64::MAX.into());
    sheet.rows(errors, |row, e| {
        let container = row.required(e, "container", container);
        let field = row.required(e, "field", values::name);
        let case = row.required(e, "case", values::name);
        let lower = row.optional(e, "lower", limit);
        let upper = row.optional(e, "upper", limit);
        let layout = row.optional(e, "layout", values::name);
        // A limit without the other is checked on the cells, so that a bad `upper` does not hide
        // a missing `lower`.
        if row.cell("lower").is_none() && row.cell("upper").is_some() {
            row.error(
                e,
                ErrorCode::MissingColumn,
                "lower",
                "upper is set without lower",
            );
        }
        let range = match (lower.ok()?, upper.ok()?) {
            (None, None) => None,
            (None, Some(_)) => None,
            (Some(lower), upper) => {
                let upper = upper.unwrap_or(lower);
                if lower > upper {
                    row.error(e, ErrorCode::BadValue, "upper", "upper is below lower");
                }
                Some((lower, upper))
            }
        };
        Some(SelectCaseRow {
            line: row.line,
            container: container?,
            field: field?,
            case: case?,
            range,
            layout: layout.ok()?,
        })
    })
}
