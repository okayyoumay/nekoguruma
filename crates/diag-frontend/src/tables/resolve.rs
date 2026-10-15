//! Resolving the typed rows of a definition into a [`Definition`]: references between tables,
//! variant scoping, and the rules of `docs/csv-tables.md` sections 3 and 4 that compare rows.
//! The field rules of section 5 and the `ir.fbs` rules of section 6 are not checked here.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::Tables;
use super::error::{ErrorCode, Errors, Rejected};
use super::rows::*;
use super::values::{self, Generated, ServiceName};

/// The rows of each scoped table that one variant's document contains, as indices into the
/// tables of [`Definition::tables`], in row order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantRows {
    pub protocols: Vec<usize>,
    pub comparams: Vec<usize>,
    pub services: Vec<usize>,
    pub dids: Vec<usize>,
    pub routines: Vec<usize>,
    pub nrcs: Vec<usize>,
    pub dtcs: Vec<usize>,
    pub flash_sessions: Vec<usize>,
}

/// A definition whose references resolve and whose scoped rows are assigned to variants.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    /// The typed rows, as read.
    pub tables: Tables,
    /// For each row of `variants.csv`, in its order, the scoped rows its document contains.
    pub variants: Vec<VariantRows>,
}

impl Definition {
    /// The `nrcs.csv` rows that give `service`'s negative responses in the document of variant
    /// `variant` (an index into `variants.csv`), in order (4.9): the rows naming the service,
    /// then the variant-wide rows whose NRC none of those rows replaces.
    pub fn negative_responses(&self, variant: usize, service: &ServiceName) -> Vec<usize> {
        let rows = &self.variants[variant].nrcs;
        let nrcs = &self.tables.nrcs;
        let own: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|&i| nrcs[i].service.as_ref() == Some(service))
            .collect();
        let wide = rows.iter().copied().filter(|&i| {
            nrcs[i].service.is_none() && !own.iter().any(|&o| nrcs[o].nrc == nrcs[i].nrc)
        });
        own.iter().copied().chain(wide).collect()
    }
}

/// Resolves `tables`; every error found is reported.
pub fn resolve(tables: Tables) -> Result<Definition, Rejected> {
    let mut errors = Errors::default();
    let variants = Resolver::new(&tables, &mut errors).run();
    if errors.0.is_empty() {
        Ok(Definition { tables, variants })
    } else {
        Err(Rejected::new(errors.0))
    }
}

/// The set of variants (indices into `variants.csv`) a row belongs to.
type Variants = BTreeSet<usize>;

/// Where a service's positive response fields are.
#[derive(Clone)]
struct ServiceInfo {
    variants: Variants,
    /// The positive response's container; `None` when it holds only generated constants.
    response: Option<Container>,
    /// The row's `variants` list named an unknown variant, so `variants` may lack some.
    partial: bool,
}

struct Resolver<'a> {
    t: &'a Tables,
    e: &'a mut Errors,
    variant_index: HashMap<&'a str, usize>,
    all: Variants,
    /// The rows, as (file, line), whose `variants` list named an unknown variant. A check that
    /// would fail only because such a row lacks a variant is skipped, since the row's own error
    /// already explains it (section 7).
    partial: HashSet<(&'static str, Line)>,
}

impl<'a> Resolver<'a> {
    fn new(t: &'a Tables, e: &'a mut Errors) -> Self {
        let variant_index = t
            .variants
            .iter()
            .enumerate()
            .map(|(i, v)| (v.name.as_str(), i))
            .collect();
        let all = (0..t.variants.len()).collect();
        Self {
            t,
            e,
            variant_index,
            all,
            partial: HashSet::new(),
        }
    }

    fn error(&mut self, code: ErrorCode, file: &str, line: Line, column: &str, hint: String) {
        self.e.push(code, file, Some(line), Some(column), hint);
    }

    fn unknown(&mut self, file: &str, line: Line, column: &str, hint: String) {
        self.error(ErrorCode::UnknownReference, file, line, column, hint);
    }

    /// The variants of a scoped row: its `variants` list, or every variant when empty. Unknown
    /// names are reported and left out.
    fn scope(&mut self, file: &'static str, line: Line, scope: &Scope) -> Variants {
        if scope.is_empty() {
            return self.all.clone();
        }
        let mut variants = Variants::new();
        for name in scope {
            match self.variant_index.get(name.as_str()) {
                Some(&i) => {
                    variants.insert(i);
                }
                None => {
                    self.partial.insert((file, line));
                    self.unknown(
                        file,
                        line,
                        "variants",
                        format!("no variant named {name} in variants.csv"),
                    );
                }
            }
        }
        variants
    }

    fn run(mut self) -> Vec<VariantRows> {
        let t = self.t;
        let protocols: Vec<Variants> = t
            .protocols
            .iter()
            .map(|r| self.scope("protocols.csv", r.line, &r.variants))
            .collect();
        let comparams: Vec<Variants> = t
            .comparams
            .iter()
            .map(|r| self.scope("comparams.csv", r.line, &r.variants))
            .collect();
        let services: Vec<Variants> = t
            .services
            .iter()
            .map(|r| self.scope("services.csv", r.line, &r.variants))
            .collect();
        let dids: Vec<Variants> = t
            .dids
            .iter()
            .map(|r| self.scope("dids.csv", r.line, &r.variants))
            .collect();
        let routines: Vec<Variants> = t
            .routines
            .iter()
            .map(|r| self.scope("routines.csv", r.line, &r.variants))
            .collect();
        let nrcs: Vec<Variants> = t
            .nrcs
            .iter()
            .map(|r| self.scope("nrcs.csv", r.line, &r.variants))
            .collect();
        let dtcs: Vec<Variants> = t
            .dtcs
            .iter()
            .map(|r| self.scope("dtcs.csv", r.line, &r.variants))
            .collect();
        let flash_sessions: Vec<Variants> = t
            .flash_sessions
            .iter()
            .map(|r| self.scope("flash_sessions.csv", r.line, &r.variants))
            .collect();

        let catalog = |rows: &[CatalogRow]| -> HashSet<String> {
            rows.iter().map(|r| r.key.clone()).collect()
        };
        let texts = catalog(&t.texts);
        let units = catalog(&t.units);
        let layouts: HashSet<&str> = t.layouts.iter().map(|r| r.name.as_str()).collect();
        let conversions: HashMap<&str, &ConversionRow> =
            t.conversions.iter().map(|r| (r.name.as_str(), r)).collect();
        let service_map = self.services(&services, &dids, &routines);

        self.protocols_per_variant(&protocols);
        self.comparams(&comparams, &protocols, &units);
        self.variant_patterns(&service_map);
        self.nrcs(&nrcs, &service_map, &texts);
        self.conversions(&texts);
        self.conversion_scales(&conversions, &texts);
        self.dtcs(&dtcs, &layouts, &texts);
        self.flash(&flash_sessions);
        self.fields(&conversions, &units, &layouts);
        self.nested(&layouts);
        self.per_variant_duplicates(
            &protocols,
            &comparams,
            &dids,
            &routines,
            &nrcs,
            &dtcs,
            &flash_sessions,
        );

        let members = |sets: &[Variants], v: usize| -> Vec<usize> {
            sets.iter()
                .enumerate()
                .filter(|(_, s)| s.contains(&v))
                .map(|(i, _)| i)
                .collect()
        };
        (0..t.variants.len())
            .map(|v| VariantRows {
                protocols: members(&protocols, v),
                comparams: members(&comparams, v),
                services: members(&services, v),
                dids: members(&dids, v),
                routines: members(&routines, v),
                nrcs: members(&nrcs, v),
                dtcs: members(&dtcs, v),
                flash_sessions: members(&flash_sessions, v),
            })
            .collect()
    }

    /// Every service a service name can mean, declared or generated (4.6 to 4.8).
    fn services(
        &mut self,
        services: &[Variants],
        dids: &[Variants],
        routines: &[Variants],
    ) -> HashMap<ServiceName, ServiceInfo> {
        let t = self.t;
        let mut map = HashMap::new();
        for (row, variants) in t.services.iter().zip(services) {
            map.insert(
                ServiceName {
                    name: row.name.clone(),
                    generated: None,
                },
                ServiceInfo {
                    variants: variants.clone(),
                    response: Some(Container::Response(row.name.clone())),
                    partial: self.partial.contains(&("services.csv", row.line)),
                },
            );
        }
        for (row, variants) in t.dids.iter().zip(dids) {
            for (on, generated) in [(row.read, Generated::Read), (row.write, Generated::Write)] {
                if on {
                    map.insert(
                        ServiceName {
                            name: row.name.clone(),
                            generated: Some(generated),
                        },
                        ServiceInfo {
                            variants: variants.clone(),
                            // A write's positive response echoes only the identifier (4.7).
                            response: (generated == Generated::Read)
                                .then(|| Container::Did(row.name.clone())),
                            partial: self.partial.contains(&("dids.csv", row.line)),
                        },
                    );
                }
            }
        }
        for (row, variants) in t.routines.iter().zip(routines) {
            for (on, generated, part) in [
                (row.start, Generated::Start, RoutinePart::Start),
                (row.stop, Generated::Stop, RoutinePart::Stop),
                (row.results, Generated::Results, RoutinePart::Results),
            ] {
                if on {
                    map.insert(
                        ServiceName {
                            name: row.name.clone(),
                            generated: Some(generated),
                        },
                        ServiceInfo {
                            variants: variants.clone(),
                            response: Some(Container::Routine {
                                routine: row.name.clone(),
                                part,
                                response: true,
                            }),
                            partial: self.partial.contains(&("routines.csv", row.line)),
                        },
                    );
                }
            }
        }
        map
    }

    /// Every variant needs a protocol (4.3).
    fn protocols_per_variant(&mut self, protocols: &[Variants]) {
        // A protocol whose scope names an unknown variant may have been meant for any of them.
        if self
            .t
            .protocols
            .iter()
            .any(|p| self.partial.contains(&("protocols.csv", p.line)))
        {
            return;
        }
        for (i, variant) in self.t.variants.iter().enumerate() {
            if !protocols.iter().any(|p| p.contains(&i)) {
                self.error(
                    ErrorCode::MissingColumn,
                    "variants.csv",
                    variant.line,
                    "name",
                    format!("variant {} has no protocol in protocols.csv", variant.name),
                );
            }
        }
    }

    /// A ComParam names a protocol in each of its variants' documents (4.3).
    fn comparams(
        &mut self,
        comparams: &[Variants],
        protocols: &[Variants],
        units: &HashSet<String>,
    ) {
        let t = self.t;
        for (row, variants) in t.comparams.iter().zip(comparams) {
            let missing: Vec<&str> = variants
                .iter()
                .filter(|&&v| {
                    !t.protocols
                        .iter()
                        .zip(protocols)
                        .any(|(p, pv)| p.name == row.protocol && pv.contains(&v))
                })
                .map(|&v| t.variants[v].name.as_str())
                .collect();
            let partial = t.protocols.iter().any(|p| {
                p.name == row.protocol && self.partial.contains(&("protocols.csv", p.line))
            });
            if !t.protocols.iter().any(|p| p.name == row.protocol) {
                // Reported whatever the row's own scope resolved to.
                self.unknown(
                    "comparams.csv",
                    row.line,
                    "protocol",
                    format!("no protocol {} in protocols.csv", row.protocol),
                );
            } else if !missing.is_empty() && !partial {
                self.unknown(
                    "comparams.csv",
                    row.line,
                    "protocol",
                    format!(
                        "no protocol {} in the document of {}",
                        row.protocol,
                        missing.join(", ")
                    ),
                );
            }
            if let Some(unit) = &row.unit
                && !units.contains(unit)
            {
                self.unknown(
                    "comparams.csv",
                    row.line,
                    "unit",
                    format!("no unit {unit} in units.csv"),
                );
            }
        }
    }

    /// A variant pattern names its variant, a service in that variant's document, and a field of
    /// that service's positive response that is neither a constant nor shaped (4.2).
    fn variant_patterns(&mut self, services: &HashMap<ServiceName, ServiceInfo>) {
        let t = self.t;
        for row in &t.variant_patterns {
            // Each column is checked on its own, so an unknown variant hides no other error.
            let variant = self.variant_index.get(row.variant.as_str()).copied();
            if variant.is_none() {
                self.unknown(
                    "variant_patterns.csv",
                    row.line,
                    "variant",
                    format!("no variant named {} in variants.csv", row.variant),
                );
            }
            let Some(service) = services.get(&row.service) else {
                self.unknown(
                    "variant_patterns.csv",
                    row.line,
                    "service",
                    format!("no service {}", show(&row.service)),
                );
                continue;
            };
            if let Some(variant) = variant
                && !service.variants.contains(&variant)
                && !service.partial
            {
                self.unknown(
                    "variant_patterns.csv",
                    row.line,
                    "service",
                    format!(
                        "service {} is not in the document of {}",
                        show(&row.service),
                        row.variant
                    ),
                );
            }
            let field = t
                .fields
                .iter()
                .find(|f| service.response.as_ref() == Some(&f.container) && f.name == row.field);
            match field {
                Some(f) if f.constant.is_none() && f.shape.is_none() => {}
                // A constant has no IR field to compare (5.3).
                Some(f) if f.constant.is_some() => self.unknown(
                    "variant_patterns.csv",
                    row.line,
                    "field",
                    format!("{} is a constant, not a field of the response", row.field),
                ),
                Some(_) => self.error(
                    ErrorCode::BadValue,
                    "variant_patterns.csv",
                    row.line,
                    "field",
                    format!("{} is a shaped field, not a scalar value", row.field),
                ),
                None => self.unknown(
                    "variant_patterns.csv",
                    row.line,
                    "field",
                    format!(
                        "the response of {} has no field {}",
                        show(&row.service),
                        row.field
                    ),
                ),
            }
        }
    }

    /// An NRC names a service in each of its variants' documents, and a text (4.9).
    fn nrcs(
        &mut self,
        nrcs: &[Variants],
        services: &HashMap<ServiceName, ServiceInfo>,
        texts: &HashSet<String>,
    ) {
        let t = self.t;
        for (row, variants) in t.nrcs.iter().zip(nrcs) {
            if let Some(name) = &row.service {
                match services.get(name) {
                    None => self.unknown(
                        "nrcs.csv",
                        row.line,
                        "service",
                        format!("no service {}", show(name)),
                    ),
                    Some(service) => {
                        let missing: Vec<&str> = variants
                            .difference(&service.variants)
                            .map(|&v| t.variants[v].name.as_str())
                            .collect();
                        if !missing.is_empty() && !service.partial {
                            self.unknown(
                                "nrcs.csv",
                                row.line,
                                "service",
                                format!(
                                    "service {} is not in the document of {}",
                                    show(name),
                                    missing.join(", ")
                                ),
                            );
                        }
                    }
                }
            }
            self.text("nrcs.csv", row.line, "text", row.text.as_deref(), texts);
        }
    }

    fn text(
        &mut self,
        file: &str,
        line: Line,
        column: &str,
        key: Option<&str>,
        texts: &HashSet<String>,
    ) {
        if let Some(key) = key
            && !texts.contains(key)
        {
            self.unknown(file, line, column, format!("no text {key} in texts.csv"));
        }
    }

    /// A text table's default text exists (4.5).
    fn conversions(&mut self, texts: &HashSet<String>) {
        for row in &self.t.conversions {
            if let ConversionKind::TextTable { default_text } = &row.kind {
                self.text(
                    "conversions.csv",
                    row.line,
                    "default_text",
                    default_text.as_deref(),
                    texts,
                );
            }
        }
    }

    /// Each `conversion_scales.csv` row by its conversion's kind, and each conversion's rows
    /// together (4.5).
    fn conversion_scales(
        &mut self,
        conversions: &HashMap<&str, &ConversionRow>,
        texts: &HashSet<String>,
    ) {
        const FILE: &str = "conversion_scales.csv";
        let t = self.t;
        // Per conversion: the number of its rows, the ranges of a scale-linear conversion and the
        // last internal value of an interpolation.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut ranges: HashMap<&str, Vec<(f64, f64, Line)>> = HashMap::new();
        let mut last_internal: HashMap<&str, f64> = HashMap::new();
        for row in &t.conversion_scales {
            let Some(conversion) = conversions.get(row.conversion.as_str()) else {
                self.unknown(
                    FILE,
                    row.line,
                    "conversion",
                    format!("no conversion {} in conversions.csv", row.conversion),
                );
                continue;
            };
            *counts.entry(conversion.name.as_str()).or_default() += 1;
            let present = [
                ("lower", row.lower.is_some()),
                ("upper", row.upper.is_some()),
                ("numerator", row.numerator.is_some()),
                ("denominator", row.denominator.is_some()),
                ("offset", row.offset.is_some()),
                ("text", row.text.is_some()),
                ("internal", row.internal.is_some()),
                ("physical", row.physical.is_some()),
            ];
            let used: &[&str] = match conversion.kind {
                ConversionKind::ScaleLinear => {
                    &["lower", "upper", "numerator", "denominator", "offset"]
                }
                ConversionKind::TextTable { .. } => &["lower", "upper", "text"],
                ConversionKind::Interpolation => &["internal", "physical"],
                _ => {
                    self.error(
                        ErrorCode::BadValue,
                        FILE,
                        row.line,
                        "conversion",
                        format!(
                            "a {} conversion takes no conversion_scales.csv rows",
                            conversion.kind.keyword()
                        ),
                    );
                    continue;
                }
            };
            for (column, set) in present {
                if set && !used.contains(&column) {
                    self.error(
                        ErrorCode::BadValue,
                        FILE,
                        row.line,
                        column,
                        format!(
                            "{column} is not used by a {} conversion and must be empty",
                            conversion.kind.keyword()
                        ),
                    );
                }
            }
            let name = conversion.name.as_str();
            match conversion.kind {
                ConversionKind::ScaleLinear => {
                    if let Some((lower, upper)) = self.limits(row, values::float, |a, b| a <= b) {
                        let earlier = ranges.entry(name).or_default();
                        if let Some(&(_, _, line)) =
                            earlier.iter().find(|(l, u, _)| lower <= *u && *l <= upper)
                        {
                            self.error(
                                ErrorCode::BadValue,
                                FILE,
                                row.line,
                                "lower",
                                format!("the range overlaps the scale on line {line}"),
                            );
                        }
                        earlier.push((lower, upper, row.line));
                    }
                }
                ConversionKind::TextTable { .. } => {
                    self.limits(
                        row,
                        |c| {
                            values::integer(c, true, i64::MIN.into(), i64::MAX.into())
                                .map(|v| v as i64)
                        },
                        |a, b| a <= b,
                    );
                    if row.text.is_none() {
                        self.error(
                            ErrorCode::MissingColumn,
                            FILE,
                            row.line,
                            "text",
                            "a text-table row needs a text".into(),
                        );
                    }
                    self.text(FILE, row.line, "text", row.text.as_deref(), texts);
                }
                ConversionKind::Interpolation => {
                    for (column, value) in [("internal", row.internal), ("physical", row.physical)]
                    {
                        if value.is_none() {
                            self.error(
                                ErrorCode::MissingColumn,
                                FILE,
                                row.line,
                                column,
                                format!("an interpolation row needs {column}"),
                            );
                        }
                    }
                    if let Some(internal) = row.internal {
                        if let Some(&last) = last_internal.get(name)
                            && internal <= last
                        {
                            self.error(
                                ErrorCode::BadValue,
                                FILE,
                                row.line,
                                "internal",
                                format!("internal values must ascend strictly (after {last})"),
                            );
                        }
                        last_internal.insert(name, internal);
                    }
                }
                _ => {}
            }
        }
        for row in &t.conversions {
            let needed = match row.kind {
                ConversionKind::ScaleLinear => 1,
                ConversionKind::Interpolation => 2,
                _ => 0,
            };
            let have = counts.get(row.name.as_str()).copied().unwrap_or(0);
            if have < needed {
                self.error(
                    ErrorCode::MissingColumn,
                    "conversions.csv",
                    row.line,
                    "kind",
                    format!(
                        "a {} conversion needs at least {needed} conversion_scales.csv row(s), {} has {have}",
                        row.kind.keyword(),
                        row.name
                    ),
                );
            }
        }
    }

    /// The `lower` and `upper` cells of a scale row, parsed by its conversion's kind; `upper`
    /// defaults to `lower`, and `lower` may not exceed it.
    fn limits<T: Copy>(
        &mut self,
        row: &ConversionScaleRow,
        parse: impl Fn(&str) -> values::Parsed<T>,
        in_order: impl Fn(T, T) -> bool,
    ) -> Option<(T, T)> {
        const FILE: &str = "conversion_scales.csv";
        if row.lower.is_none() {
            self.error(
                ErrorCode::MissingColumn,
                FILE,
                row.line,
                "lower",
                "the row needs a lower limit".into(),
            );
        }
        let mut cell = |column: &str, text: Option<&String>| -> Result<Option<T>, ()> {
            match text {
                None => Ok(None),
                Some(text) => parse(text).map(Some).map_err(|hint| {
                    self.error(ErrorCode::BadValue, FILE, row.line, column, hint);
                }),
            }
        };
        let lower = cell("lower", row.lower.as_ref());
        let upper = cell("upper", row.upper.as_ref());
        let (lower, upper) = (lower.ok()??, upper.ok()?);
        let upper = upper.unwrap_or(lower);
        if !in_order(lower, upper) {
            self.error(
                ErrorCode::BadValue,
                FILE,
                row.line,
                "upper",
                "upper is below lower".into(),
            );
            return None;
        }
        Some((lower, upper))
    }

    /// A DTC's text and layouts exist, and those layouts hold no shaped field (4.11).
    fn dtcs(&mut self, _dtcs: &[Variants], layouts: &HashSet<&str>, texts: &HashSet<String>) {
        let t = self.t;
        // A layout a DTC names has no shaped field, since a DTC definition holds no layouts; the
        // field is reported once, however many DTCs name its layout.
        let named: HashSet<&str> = t
            .dtcs
            .iter()
            .flat_map(|r| [Some(&r.status), r.snapshot.as_ref(), r.extended.as_ref()])
            .flatten()
            .map(String::as_str)
            .collect();
        for field in &t.fields {
            if let Container::Layout(layout) = &field.container
                && named.contains(layout.as_str())
                && field.shape.is_some()
            {
                self.error(
                    ErrorCode::BadValue,
                    "fields.csv",
                    field.line,
                    "shape",
                    format!("layout {layout} is named by a DTC and cannot hold a shaped field"),
                );
            }
        }
        for row in &t.dtcs {
            self.text("dtcs.csv", row.line, "text", row.text.as_deref(), texts);
            for (column, layout) in [
                ("status", Some(&row.status)),
                ("snapshot", row.snapshot.as_ref()),
                ("extended", row.extended.as_ref()),
            ] {
                if let Some(layout) = layout
                    && !layouts.contains(layout.as_str())
                {
                    self.unknown(
                        "dtcs.csv",
                        row.line,
                        column,
                        format!("no layout {layout} in layouts.csv"),
                    );
                }
            }
        }
    }

    /// Every flash session has a segment, every segment a session, and one session's segments
    /// do not overlap (4.12).
    fn flash(&mut self, _sessions: &[Variants]) {
        let t = self.t;
        let mut by_session: HashMap<&str, Vec<(u128, u128, Line)>> = HashMap::new();
        for row in &t.flash_segments {
            if !t.flash_sessions.iter().any(|s| s.name == row.session) {
                self.unknown(
                    "flash_segments.csv",
                    row.line,
                    "session",
                    format!("no flash session {} in flash_sessions.csv", row.session),
                );
                continue;
            }
            let start = u128::from(row.start_address);
            let end = start + u128::from(row.length);
            let earlier = by_session.entry(row.session.as_str()).or_default();
            if let Some(&(_, _, line)) = earlier.iter().find(|(s, e, _)| start < *e && *s < end) {
                self.error(
                    ErrorCode::Duplicate,
                    "flash_segments.csv",
                    row.line,
                    "start_address",
                    format!("the address range overlaps the segment on line {line}"),
                );
            }
            earlier.push((start, end, row.line));
        }
        for row in &t.flash_sessions {
            if !by_session.contains_key(row.name.as_str()) {
                self.error(
                    ErrorCode::MissingColumn,
                    "flash_sessions.csv",
                    row.line,
                    "name",
                    format!(
                        "flash session {} has no segment in flash_segments.csv",
                        row.name
                    ),
                );
            }
        }
    }

    /// A field's container belongs to a row that exists (a routine container to a sub-function
    /// the routine has), and its conversion, unit, layout and `ref` exist; `ref` names a field
    /// earlier in the same container (5.1, 5.2). A DID needs a field (4.7).
    fn fields(
        &mut self,
        conversions: &HashMap<&str, &ConversionRow>,
        units: &HashSet<String>,
        layouts: &HashSet<&str>,
    ) {
        const FILE: &str = "fields.csv";
        let t = self.t;
        let mut seen: HashMap<&Container, HashSet<&str>> = HashMap::new();
        for row in &t.fields {
            let owner = match &row.container {
                Container::Request(name) | Container::Response(name) => t
                    .services
                    .iter()
                    .any(|s| &s.name == name)
                    .then_some(())
                    .ok_or_else(|| format!("no service {name} in services.csv")),
                Container::Did(name) => t
                    .dids
                    .iter()
                    .any(|d| &d.name == name)
                    .then_some(())
                    .ok_or_else(|| format!("no DID {name} in dids.csv")),
                Container::Layout(name) => layouts
                    .contains(name.as_str())
                    .then_some(())
                    .ok_or_else(|| format!("no layout {name} in layouts.csv")),
                Container::Routine { routine, part, .. } => {
                    match t.routines.iter().find(|r| &r.name == routine) {
                        None => Err(format!("no routine {routine} in routines.csv")),
                        Some(r) => {
                            let has = match part {
                                RoutinePart::Start => r.start,
                                RoutinePart::Stop => r.stop,
                                RoutinePart::Results => r.results,
                            };
                            has.then_some(()).ok_or_else(|| {
                                format!("routine {routine} does not have that sub-function")
                            })
                        }
                    }
                }
            };
            if let Err(hint) = owner {
                self.unknown(FILE, row.line, "container", hint);
            }
            if let Some(conversion) = &row.conversion
                && !conversions.contains_key(conversion.as_str())
            {
                self.unknown(
                    FILE,
                    row.line,
                    "conversion",
                    format!("no conversion {conversion} in conversions.csv"),
                );
            }
            if let Some(unit) = &row.unit
                && !units.contains(unit)
            {
                self.unknown(
                    FILE,
                    row.line,
                    "unit",
                    format!("no unit {unit} in units.csv"),
                );
            }
            if let Some(layout) = &row.layout
                && !layouts.contains(layout.as_str())
            {
                self.unknown(
                    FILE,
                    row.line,
                    "layout",
                    format!("no layout {layout} in layouts.csv"),
                );
            }
            let earlier = seen.entry(&row.container).or_default();
            if let Some(reference) = &row.reference
                && !earlier.contains(reference.as_str())
            {
                let named = |f: &&FieldRow| f.name == *reference;
                if t.fields
                    .iter()
                    .filter(named)
                    .any(|f| f.container == row.container)
                {
                    self.error(
                        ErrorCode::BadValue,
                        FILE,
                        row.line,
                        "ref",
                        format!("{reference} comes after this field in the container"),
                    );
                } else if t.fields.iter().any(|f| named(&f)) {
                    // A field of another container cannot be tied to one occurrence (5.2).
                    self.error(
                        ErrorCode::Unsupported,
                        FILE,
                        row.line,
                        "ref",
                        format!("{reference} is a field of another container"),
                    );
                } else {
                    self.unknown(FILE, row.line, "ref", format!("no field {reference}"));
                }
            }
            earlier.insert(row.name.as_str());
        }
        for did in &t.dids {
            if !t
                .fields
                .iter()
                .any(|f| matches!(&f.container, Container::Did(name) if name == &did.name))
            {
                self.error(
                    ErrorCode::MissingColumn,
                    "dids.csv",
                    did.line,
                    "name",
                    format!("DID {} has no field in fields.csv", did.name),
                );
            }
        }
    }

    /// A `repeats.csv` or `select_cases.csv` row names a field of `fields.csv`, and a case's
    /// layout exists (5.4).
    fn nested(&mut self, layouts: &HashSet<&str>) {
        let t = self.t;
        let exists = |container: &Container, field: &str| {
            t.fields
                .iter()
                .any(|f| &f.container == container && f.name == field)
        };
        for row in &t.repeats {
            if !exists(&row.container, &row.field) {
                self.unknown(
                    "repeats.csv",
                    row.line,
                    "field",
                    format!("no field {} in that container of fields.csv", row.field),
                );
            }
        }
        for row in &t.select_cases {
            if !exists(&row.container, &row.field) {
                self.unknown(
                    "select_cases.csv",
                    row.line,
                    "field",
                    format!("no field {} in that container of fields.csv", row.field),
                );
            }
            if let Some(layout) = &row.layout
                && !layouts.contains(layout.as_str())
            {
                self.unknown(
                    "select_cases.csv",
                    row.line,
                    "layout",
                    format!("no layout {layout} in layouts.csv"),
                );
            }
        }
    }

    /// Two rows that would put two definitions of one thing into one variant's document (3,
    /// 4.3, 4.9): the later row is reported once.
    #[expect(clippy::too_many_arguments, reason = "one scope list per scoped table")]
    fn per_variant_duplicates(
        &mut self,
        protocols: &[Variants],
        comparams: &[Variants],
        dids: &[Variants],
        routines: &[Variants],
        nrcs: &[Variants],
        dtcs: &[Variants],
        flash_sessions: &[Variants],
    ) {
        let t = self.t;
        self.unique_per_variant(
            "protocols.csv",
            "name",
            t.protocols
                .iter()
                .zip(protocols)
                .map(|(r, v)| (r.line, v, r.name.clone(), format!("protocol {}", r.name))),
        );
        self.unique_per_variant(
            "comparams.csv",
            "name",
            t.comparams.iter().zip(comparams).map(|(r, v)| {
                (
                    r.line,
                    v,
                    (r.protocol.clone(), r.name.clone()),
                    format!("ComParam {} of {}", r.name, r.protocol),
                )
            }),
        );
        self.unique_per_variant(
            "dids.csv",
            "did",
            t.dids
                .iter()
                .zip(dids)
                .map(|(r, v)| (r.line, v, r.did, format!("DID 0x{:04X}", r.did))),
        );
        self.unique_per_variant(
            "routines.csv",
            "rid",
            t.routines
                .iter()
                .zip(routines)
                .map(|(r, v)| (r.line, v, r.rid, format!("routine 0x{:04X}", r.rid))),
        );
        self.unique_per_variant(
            "nrcs.csv",
            "nrc",
            t.nrcs.iter().zip(nrcs).map(|(r, v)| {
                let service = r
                    .service
                    .as_ref()
                    .map_or_else(|| "every service".to_owned(), show);
                (
                    r.line,
                    v,
                    (r.service.clone(), r.nrc),
                    format!("NRC 0x{:02X} for {service}", r.nrc),
                )
            }),
        );
        self.unique_per_variant(
            "dtcs.csv",
            "code",
            t.dtcs
                .iter()
                .zip(dtcs)
                .map(|(r, v)| (r.line, v, r.code, format!("DTC 0x{:06X}", r.code))),
        );
        self.unique_per_variant(
            "flash_sessions.csv",
            "id",
            t.flash_sessions
                .iter()
                .zip(flash_sessions)
                .map(|(r, v)| (r.line, v, r.id, format!("flash session {}", r.id))),
        );
    }

    fn unique_per_variant<'v, K: Eq + std::hash::Hash + Clone>(
        &mut self,
        file: &str,
        column: &str,
        rows: impl IntoIterator<Item = (Line, &'v Variants, K, String)>,
    ) {
        let mut first: HashMap<(usize, K), Line> = HashMap::new();
        for (line, variants, key, shown) in rows {
            let mut clash: Option<(Line, usize)> = None;
            for &v in variants {
                match first.get(&(v, key.clone())) {
                    Some(&earlier) => {
                        clash.get_or_insert((earlier, v));
                    }
                    None => {
                        first.insert((v, key.clone()), line);
                    }
                }
            }
            if let Some((earlier, v)) = clash {
                let variant = self.t.variants[v].name.clone();
                self.error(
                    ErrorCode::Duplicate,
                    file,
                    line,
                    column,
                    format!("{shown} is already defined for {variant} on line {earlier}"),
                );
            }
        }
    }
}

fn show(service: &ServiceName) -> String {
    match service.generated {
        None => service.name.clone(),
        Some(generated) => {
            let suffix = match generated {
                Generated::Read => "read",
                Generated::Write => "write",
                Generated::Start => "start",
                Generated::Stop => "stop",
                Generated::Results => "results",
            };
            format!("{}.{suffix}", service.name)
        }
    }
}
