//! The services of a variant's document, their fields, the nested layouts those fields reach and
//! the variant patterns (`docs/csv-tables.md` sections 3, 4.2 and 4.5 to 4.10, and section 5).
//!
//! [`Index`] assigns the ids section 3 defines over the whole definition, so a row has the same
//! id in every variant's document, and converts each conversion once; [`Index::variant`] then
//! builds one variant's part.

use std::collections::{BTreeMap, HashMap, HashSet};

use diag_ir::declaration::{
    ByteOrder, CompuLinearT, CompuRationalT, CompuScaleLinearT, CompuT, CompuTableInterpT,
    CompuTextTableT, DataKind, FieldT, GroupT, InterpPointT, LayoutT, LengthKind, LinearScaleT,
    NegativeResponseT, Placement, PositiveResponseT, RepeatCount, RepeatT, RequestT, SelectCaseT,
    SelectT, ServiceT, ShapeT, Termination, TextEntryT, VariantPatternT,
};

use crate::tables::rows::{
    self, ConversionKind, ConversionRow, FieldRow, FieldType, Length, RoutinePart, Terminator,
    Timing,
};
use crate::tables::{Constant, Definition, Generated, ServiceName, constant_value, values};

/// The services, layouts and variant patterns of one variant's document.
pub(super) struct VariantPart {
    pub services: Vec<ServiceT>,
    pub layouts: Vec<LayoutT>,
    pub patterns: Vec<VariantPatternT>,
}

type FieldKey<'a> = (&'a rows::Container, &'a str);

/// Ids and conversions shared by every variant's document.
pub(super) struct Index<'a> {
    definition: &'a Definition,
    /// The IR id of each `fields.csv` row; a constant has none (section 3).
    field_ids: Vec<Option<u32>>,
    /// `fields.csv` rows by container and name.
    fields_by_name: HashMap<FieldKey<'a>, usize>,
    /// `fields.csv` rows of each container, in row order.
    fields_by_container: HashMap<&'a rows::Container, Vec<usize>>,
    /// The `repeats.csv` row of each `repeat` field, with its marker's id (0 without a marker).
    repeats: HashMap<FieldKey<'a>, (usize, u32)>,
    /// The `select_cases.csv` rows of each `select` field, in row order.
    cases: HashMap<FieldKey<'a>, Vec<usize>>,
    layout_ids: HashMap<&'a str, u32>,
    compus: HashMap<&'a str, CompuT>,
    unit_ids: HashMap<&'a str, u32>,
    text_ids: HashMap<&'a str, u32>,
    service_ids: HashMap<ServiceName, u32>,
}

/// A variant's `nrcs.csv` rows: those naming a service, by service, and the variant-wide ones.
#[derive(Default)]
struct Nrcs<'a> {
    own: HashMap<&'a ServiceName, Vec<usize>>,
    wide: Vec<usize>,
}

/// One service of a document, as the tables define or generate it.
struct ServiceSpec<'a> {
    name: ServiceName,
    sid: u8,
    session: u8,
    security: u8,
    timing: &'a Timing,
    /// The generated constants that open the request and response prefixes.
    generated_prefix: Vec<u8>,
    /// The containers of the request and response fields, if they have any.
    request: Option<rows::Container>,
    response: Option<rows::Container>,
}

impl<'a> Index<'a> {
    pub(super) fn new(definition: &'a Definition) -> Self {
        let tables = &definition.tables;
        let mut next = 0u32;
        let mut take = || {
            next += 1;
            next
        };
        let field_ids: Vec<Option<u32>> = tables
            .fields
            .iter()
            .map(|f| f.constant.is_none().then(&mut take))
            .collect();
        let mut fields_by_name = HashMap::new();
        let mut fields_by_container: HashMap<&rows::Container, Vec<usize>> = HashMap::new();
        for (i, f) in tables.fields.iter().enumerate() {
            fields_by_name.insert((&f.container, f.name.as_str()), i);
            fields_by_container.entry(&f.container).or_default().push(i);
        }
        // Marker fields follow the `fields.csv` rows, in `repeats.csv` row order.
        let repeats = tables
            .repeats
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let marker = match r.count {
                    rows::RepeatCount::Marker { .. } => take(),
                    _ => 0,
                };
                ((&r.container, r.field.as_str()), (i, marker))
            })
            .collect();
        let mut cases: HashMap<FieldKey, Vec<usize>> = HashMap::new();
        for (i, c) in tables.select_cases.iter().enumerate() {
            cases
                .entry((&c.container, c.field.as_str()))
                .or_default()
                .push(i);
        }
        let layout_ids = (1..)
            .zip(&tables.layouts)
            .map(|(id, l)| (l.name.as_str(), id))
            .collect();
        let unit_ids = tables
            .units
            .iter()
            .map(|u| (u.key.as_str(), u.id))
            .collect();
        let text_ids: HashMap<&str, u32> = tables
            .texts
            .iter()
            .map(|t| (t.key.as_str(), t.id))
            .collect();
        // Each conversion's scale rows, in row order, gathered in one pass.
        let mut scales: HashMap<&str, Vec<&rows::ConversionScaleRow>> = HashMap::new();
        for s in &tables.conversion_scales {
            scales.entry(s.conversion.as_str()).or_default().push(s);
        }
        let compus = tables
            .conversions
            .iter()
            .map(|c| {
                let rows = scales.get(c.name.as_str()).map_or(&[][..], Vec::as_slice);
                (c.name.as_str(), compu(c, rows, &text_ids))
            })
            .collect();
        // Services, then DID services (read before write), then routine services (start, stop,
        // results); only generated services take an id (section 3).
        let mut service_ids = HashMap::new();
        let mut next_service = 0u32;
        let mut add = |name: &str, generated: Option<Generated>| {
            next_service += 1;
            service_ids.insert(
                ServiceName {
                    name: name.to_owned(),
                    generated,
                },
                next_service,
            );
        };
        for s in &tables.services {
            add(&s.name, None);
        }
        for d in &tables.dids {
            for (on, generated) in [(d.read, Generated::Read), (d.write, Generated::Write)] {
                if on {
                    add(&d.name, Some(generated));
                }
            }
        }
        for r in &tables.routines {
            for (on, generated) in [
                (r.start, Generated::Start),
                (r.stop, Generated::Stop),
                (r.results, Generated::Results),
            ] {
                if on {
                    add(&r.name, Some(generated));
                }
            }
        }
        Self {
            definition,
            field_ids,
            fields_by_name,
            fields_by_container,
            repeats,
            cases,
            layout_ids,
            compus,
            unit_ids,
            text_ids,
            service_ids,
        }
    }

    /// The services, layouts and variant patterns of the document of `variant`, an index into
    /// `variants.csv`.
    pub(super) fn variant(&self, variant: usize) -> VariantPart {
        let tables = &self.definition.tables;
        let rows = &self.definition.variants[variant];
        let mut specs = Vec::new();
        for &i in &rows.services {
            let s = &tables.services[i];
            specs.push(ServiceSpec {
                name: ServiceName {
                    name: s.name.clone(),
                    generated: None,
                },
                sid: s.sid,
                session: s.session,
                security: s.security,
                timing: &s.timing,
                generated_prefix: Vec::new(),
                request: Some(rows::Container::Request(s.name.clone())),
                response: Some(rows::Container::Response(s.name.clone())),
            });
        }
        for &i in &rows.dids {
            let d = &tables.dids[i];
            let record = || Some(rows::Container::Did(d.name.clone()));
            let did = d.did.to_be_bytes().to_vec();
            let generated = |generated| ServiceName {
                name: d.name.clone(),
                generated: Some(generated),
            };
            if d.read {
                specs.push(ServiceSpec {
                    name: generated(Generated::Read),
                    sid: 0x22,
                    session: d.read_session,
                    security: d.read_security,
                    timing: &d.timing,
                    generated_prefix: did.clone(),
                    request: None,
                    response: record(),
                });
            }
            if d.write {
                specs.push(ServiceSpec {
                    name: generated(Generated::Write),
                    sid: 0x2E,
                    session: d.write_session,
                    security: d.write_security,
                    timing: &d.timing,
                    generated_prefix: did,
                    request: record(),
                    response: None,
                });
            }
        }
        for &i in &rows.routines {
            let r = &tables.routines[i];
            for (on, part, generated, sub) in [
                (r.start, RoutinePart::Start, Generated::Start, 0x01),
                (r.stop, RoutinePart::Stop, Generated::Stop, 0x02),
                (r.results, RoutinePart::Results, Generated::Results, 0x03),
            ] {
                if !on {
                    continue;
                }
                let container = |response| {
                    Some(rows::Container::Routine {
                        routine: r.name.clone(),
                        part,
                        response,
                    })
                };
                let [rid_high, rid_low] = r.rid.to_be_bytes();
                specs.push(ServiceSpec {
                    name: ServiceName {
                        name: r.name.clone(),
                        generated: Some(generated),
                    },
                    sid: 0x31,
                    session: r.session,
                    security: r.security,
                    timing: &r.timing,
                    generated_prefix: vec![sub, rid_high, rid_low],
                    request: container(false),
                    response: container(true),
                });
            }
        }
        // The variant's NRC rows, by the service they name and variant-wide, in row order.
        let mut nrcs = Nrcs::default();
        for &i in &rows.nrcs {
            match &tables.nrcs[i].service {
                Some(service) => nrcs.own.entry(service).or_default().push(i),
                None => nrcs.wide.push(i),
            }
        }
        let services = specs.iter().map(|spec| self.service(spec, &nrcs)).collect();
        let layouts = self.layouts(
            specs
                .iter()
                .flat_map(|spec| [spec.request.as_ref(), spec.response.as_ref()])
                .flatten(),
        );
        let name = &tables.variants[variant].name;
        let patterns = tables
            .variant_patterns
            .iter()
            .filter(|p| &p.variant == name)
            .map(|p| {
                let response = response_container(&p.service)
                    .expect("a variant pattern names a service whose response has fields");
                let mut pattern = VariantPatternT::default();
                pattern.service_id = self.service_ids[&p.service];
                pattern.field_id = self.field_id(&response, &p.field);
                pattern.expected = Some(p.expected.clone());
                pattern
            })
            .collect();
        VariantPart {
            services,
            layouts,
            patterns,
        }
    }

    fn service(&self, spec: &ServiceSpec, nrcs: &Nrcs) -> ServiceT {
        let part = |container: Option<&rows::Container>| {
            let mut prefix = spec.generated_prefix.clone();
            let mut fields = Vec::new();
            for &i in container
                .and_then(|c| self.fields_by_container.get(c))
                .into_iter()
                .flatten()
            {
                let f = &self.definition.tables.fields[i];
                match f.constant.as_deref() {
                    Some(value) => prefix.extend(constant_bytes(f, value)),
                    None => fields.push(self.field(i)),
                }
            }
            (prefix, fields)
        };
        let tables = &self.definition.tables;
        let (request_prefix, params) = part(spec.request.as_ref());
        let (response_prefix, fields) = part(spec.response.as_ref());
        let mut request = RequestT::default();
        request.sid = spec.sid;
        request.prefix = Some(request_prefix);
        request.params = Some(params);
        let mut positive = PositiveResponseT::default();
        positive.sid = spec.sid + 0x40;
        positive.prefix = Some(response_prefix);
        positive.fields = Some(fields);
        // The service's own rows, then the variant-wide rows whose NRC none of them replaces
        // (4.9). A variant holds each NRC once variant-wide, so the second list is short.
        let own = nrcs.own.get(&spec.name).map_or(&[][..], Vec::as_slice);
        let replaced: HashSet<u8> = own.iter().map(|&i| tables.nrcs[i].nrc).collect();
        let wide = nrcs
            .wide
            .iter()
            .filter(|&&i| !replaced.contains(&tables.nrcs[i].nrc));
        let negatives = own
            .iter()
            .chain(wide)
            .map(|&i| {
                let row = &tables.nrcs[i];
                let mut negative = NegativeResponseT::default();
                negative.nrc = row.nrc;
                negative.message_id = self.text_id(row.text.as_deref());
                negative.retryable = row.retryable;
                negative
            })
            .collect();
        let mut service = ServiceT::default();
        service.id = self.service_ids[&spec.name];
        service.short_name = Some(short_name(&spec.name));
        service.request = Some(Box::new(request));
        service.positive = Some(Box::new(positive));
        service.negatives = Some(negatives);
        service.required_session = spec.session;
        service.required_security_level = spec.security;
        service.p2_millis = spec.timing.p2_ms;
        service.p2_star_millis = spec.timing.p2_star_ms;
        service
    }

    /// The IR field of `fields.csv` row `row`, which is not a constant.
    fn field(&self, row: usize) -> FieldT {
        let f = &self.definition.tables.fields[row];
        let mut field = FieldT::default();
        field.id = self.field_ids[row].expect("a constant is not an IR field");
        field.short_name = Some(f.name.clone());
        match f.byte {
            Some(byte) => {
                // Bytes before the container's position 0 (5.2).
                let base: u64 = match f.container {
                    rows::Container::Request(_) | rows::Container::Response(_) => 1,
                    rows::Container::Did(_) => 3,
                    rows::Container::Routine { .. } => 4,
                    rows::Container::Layout(_) => 0,
                };
                let offset = (base + u64::from(byte)) * 8 + u64::from(f.bit);
                field.placement = Placement::Absolute;
                field.bit_offset =
                    u32::try_from(offset).expect("bit offsets beyond 32 bits are rejected");
            }
            None => {
                field.placement = Placement::AfterPrevious;
                field.bit_offset = f.bit.into();
            }
        }
        if let Some(shape) = f.shape {
            field.shape = self.shape(f, shape);
            return field;
        }
        let length = f.length.unwrap_or(Length::Fixed);
        field.length_kind = match length {
            Length::Fixed => LengthKind::Fixed,
            Length::Field => LengthKind::FromField,
            Length::Leading => LengthKind::LeadingLength,
            Length::Terminated => LengthKind::Terminated,
        };
        if matches!(length, Length::Fixed | Length::Leading) {
            field.bit_length = f.bits.expect("a fixed or leading field has a width");
        }
        field.kind = match f.field_type.expect("a scalar field has a type") {
            FieldType::Uint => DataKind::Uint,
            FieldType::Int => DataKind::Int,
            FieldType::Float => DataKind::Float,
            FieldType::Ascii => DataKind::Ascii,
            FieldType::Bytes => DataKind::Bytes,
            FieldType::Bcd => DataKind::Bcd,
            FieldType::Utf8 => DataKind::Utf8,
            FieldType::Unicode2 => DataKind::Unicode2,
        };
        field.byte_order = if f.big_endian {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        };
        field.compu = match &f.conversion {
            Some(name) => self.compus[name.as_str()].clone(),
            None => identity(),
        };
        field.unit_id = f.unit.as_deref().map_or(0, |key| self.unit_ids[key]);
        if length == Length::Field {
            field.length_field_id = self.reference_id(f);
        }
        if length == Length::Terminated {
            field.min_bytes = f.min_bytes.unwrap_or(0);
            field.max_bytes = f.max_bytes.unwrap_or(0);
            field.termination = match f.terminator.unwrap_or(Terminator::End) {
                Terminator::End => Termination::EndOfPdu,
                Terminator::Zero => Termination::Zero,
                Terminator::Ff => Termination::HexFF,
            };
        }
        field
    }

    fn shape(&self, f: &FieldRow, shape: rows::Shape) -> ShapeT {
        let layout_id = |name: Option<&String>| name.map_or(0, |n| self.layout_ids[n.as_str()]);
        let key = (&f.container, f.name.as_str());
        match shape {
            rows::Shape::Group => {
                let mut group = GroupT::default();
                group.layout_id = layout_id(f.layout.as_ref());
                ShapeT::Group(Box::new(group))
            }
            rows::Shape::Repeat => {
                let (row, marker_id) = self.repeats[&key];
                let mut repeat = RepeatT::default();
                repeat.layout_id = layout_id(f.layout.as_ref());
                match self.definition.tables.repeats[row].count {
                    rows::RepeatCount::Fixed { items, item_bytes } => {
                        repeat.count = RepeatCount::Fixed;
                        repeat.fixed_count = items;
                        repeat.item_byte_size = item_bytes;
                    }
                    rows::RepeatCount::Field => {
                        repeat.count = RepeatCount::FromField;
                        repeat.count_field_id = self.reference_id(f);
                    }
                    rows::RepeatCount::Marker { bits, lower, upper } => {
                        let mut marker = FieldT::default();
                        marker.id = marker_id;
                        marker.short_name = Some(format!("{}.marker", f.name));
                        marker.placement = Placement::Absolute;
                        marker.length_kind = LengthKind::Fixed;
                        marker.bit_length = bits.into();
                        marker.kind = DataKind::Uint;
                        marker.compu = identity();
                        repeat.count = RepeatCount::UntilMarker;
                        repeat.marker = Some(Box::new(marker));
                        // Stored bit for bit in int64 (5.4).
                        repeat.marker_lower = lower as i64;
                        repeat.marker_upper = upper as i64;
                    }
                    rows::RepeatCount::End => repeat.count = RepeatCount::UntilEndOfPdu,
                }
                ShapeT::Repeat(Box::new(repeat))
            }
            rows::Shape::Select => {
                let mut select = SelectT::default();
                select.key_field_id = self.reference_id(f);
                let mut cases = Vec::new();
                for &i in self.cases.get(&key).into_iter().flatten() {
                    let row = &self.definition.tables.select_cases[i];
                    match row.range {
                        None => {
                            select.has_default = true;
                            select.default_layout_id = layout_id(row.layout.as_ref());
                        }
                        Some((lower, upper)) => {
                            let mut case = SelectCaseT::default();
                            case.short_name = Some(row.case.clone());
                            // A limit of 2^63 or more is stored bit for bit in int64 (5.4); the
                            // low 64 bits of the i128 are exactly that.
                            case.lower = lower as i64;
                            case.upper = upper as i64;
                            case.layout_id = layout_id(row.layout.as_ref());
                            cases.push(case);
                        }
                    }
                }
                select.cases = Some(cases);
                ShapeT::Select(Box::new(select))
            }
        }
    }

    /// The layouts the fields of `containers` reach through `group`, `repeat` and `select`
    /// fields, directly or through other layouts, sorted by id (4.10).
    fn layouts<'c>(&self, containers: impl Iterator<Item = &'c rows::Container>) -> Vec<LayoutT> {
        let tables = &self.definition.tables;
        let mut reached: BTreeMap<u32, &str> = BTreeMap::new();
        let mut pending: Vec<rows::Container> = containers.cloned().collect();
        while let Some(container) = pending.pop() {
            for &i in self
                .fields_by_container
                .get(&container)
                .into_iter()
                .flatten()
            {
                let f = &tables.fields[i];
                let Some(shape) = f.shape else { continue };
                let targets: Vec<&str> = match shape {
                    rows::Shape::Group | rows::Shape::Repeat => {
                        f.layout.as_deref().into_iter().collect()
                    }
                    rows::Shape::Select => self
                        .cases
                        .get(&(&f.container, f.name.as_str()))
                        .into_iter()
                        .flatten()
                        .filter_map(|&c| tables.select_cases[c].layout.as_deref())
                        .collect(),
                };
                for name in targets {
                    if reached.insert(self.layout_ids[name], name).is_none() {
                        pending.push(rows::Container::Layout(name.to_owned()));
                    }
                }
            }
        }
        reached
            .into_iter()
            .map(|(id, name)| {
                let row = &tables.layouts[id as usize - 1];
                let container = rows::Container::Layout(name.to_owned());
                let mut layout = LayoutT::default();
                layout.id = id;
                layout.short_name = Some(name.to_owned());
                layout.byte_size = row.byte_size;
                layout.fields = Some(
                    self.fields_by_container
                        .get(&container)
                        .into_iter()
                        .flatten()
                        .map(|&i| self.field(i))
                        .collect(),
                );
                layout
            })
            .collect()
    }

    /// The id of the field `f`'s `ref` names, in `f`'s container (5.2).
    fn reference_id(&self, f: &FieldRow) -> u32 {
        let name = f.reference.as_deref().expect("the field has a ref");
        self.field_id(&f.container, name)
    }

    fn field_id(&self, container: &rows::Container, name: &str) -> u32 {
        self.field_ids[self.fields_by_name[&(container, name)]].expect("refers to an IR field")
    }

    fn text_id(&self, key: Option<&str>) -> u32 {
        key.map_or(0, |key| self.text_ids[key])
    }
}

/// The container of the fields of a service's positive response, if it can have any.
fn response_container(service: &ServiceName) -> Option<rows::Container> {
    let routine = |part| rows::Container::Routine {
        routine: service.name.clone(),
        part,
        response: true,
    };
    match service.generated {
        None => Some(rows::Container::Response(service.name.clone())),
        Some(Generated::Read) => Some(rows::Container::Did(service.name.clone())),
        Some(Generated::Write) => None,
        Some(Generated::Start) => Some(routine(RoutinePart::Start)),
        Some(Generated::Stop) => Some(routine(RoutinePart::Stop)),
        Some(Generated::Results) => Some(routine(RoutinePart::Results)),
    }
}

/// `Service.short_name`: the name, with a generated service's suffix (4.7, 4.8).
fn short_name(service: &ServiceName) -> String {
    let suffix = match service.generated {
        None => return service.name.clone(),
        Some(Generated::Read) => "read",
        Some(Generated::Write) => "write",
        Some(Generated::Start) => "start",
        Some(Generated::Stop) => "stop",
        Some(Generated::Results) => "results",
    };
    format!("{}.{suffix}", service.name)
}

/// A constant's bytes, big-endian (5.3).
fn constant_bytes(f: &FieldRow, value: &str) -> Vec<u8> {
    match constant_value(f, value).expect("a valid constant") {
        Constant::Integer(v) => {
            let width = (f.bits.expect("a constant has a width") / 8) as usize;
            v.to_be_bytes()[8 - width..].to_vec()
        }
        Constant::Bytes(bytes) => bytes,
    }
}

fn identity() -> CompuT {
    CompuT::CompuIdentity(Box::default())
}

/// The `Compu` of a conversion, with its `conversion_scales.csv` rows in row order (4.5).
fn compu(
    row: &ConversionRow,
    scale_rows: &[&rows::ConversionScaleRow],
    text_ids: &HashMap<&str, u32>,
) -> CompuT {
    let scales = || scale_rows.iter().copied();
    let limits = |s: &rows::ConversionScaleRow, parse: &dyn Fn(&str) -> f64| {
        let lower = parse(s.lower.as_deref().expect("a scale row has a lower limit"));
        let upper = s.upper.as_deref().map_or(lower, parse);
        (lower, upper)
    };
    match &row.kind {
        ConversionKind::Identity => identity(),
        ConversionKind::Linear {
            numerator,
            denominator,
            offset,
        } => {
            let mut linear = CompuLinearT::default();
            linear.numerator = *numerator;
            linear.denominator = *denominator;
            linear.offset = *offset;
            CompuT::CompuLinear(Box::new(linear))
        }
        ConversionKind::ScaleLinear => {
            let float = |c: &str| values::float(c).expect("a valid limit");
            let mut scale_linear = CompuScaleLinearT::default();
            scale_linear.scales = Some(
                scales()
                    .map(|s| {
                        let (lower, upper) = limits(s, &float);
                        LinearScaleT {
                            lower,
                            upper,
                            numerator: s.numerator.unwrap_or(1.0),
                            denominator: s.denominator.unwrap_or(1.0),
                            offset: s.offset.unwrap_or(0.0),
                        }
                    })
                    .collect(),
            );
            CompuT::CompuScaleLinear(Box::new(scale_linear))
        }
        ConversionKind::TextTable { default_text } => {
            let integer = |c: &str| -> i64 {
                let v = values::integer(c, true, i64::MIN.into(), i64::MAX.into())
                    .expect("a valid limit");
                i64::try_from(v).expect("checked to fit")
            };
            let mut table = CompuTextTableT::default();
            table.entries = Some(
                scales()
                    .map(|s| {
                        let lower = integer(s.lower.as_deref().expect("a lower limit"));
                        let upper = s.upper.as_deref().map_or(lower, integer);
                        let mut entry = TextEntryT::default();
                        entry.lower = lower;
                        entry.upper = upper;
                        entry.text_id = text_ids[s.text.as_deref().expect("a text")];
                        entry
                    })
                    .collect(),
            );
            table.default_text_id = default_text.as_deref().map_or(0, |key| text_ids[key]);
            CompuT::CompuTextTable(Box::new(table))
        }
        ConversionKind::Interpolation => {
            let mut interp = CompuTableInterpT::default();
            interp.points = Some(
                scales()
                    .map(|s| InterpPointT {
                        internal: s.internal.expect("an internal value"),
                        physical: s.physical.expect("a physical value"),
                    })
                    .collect(),
            );
            CompuT::CompuTableInterp(Box::new(interp))
        }
        ConversionKind::Rational {
            numerator_coeffs,
            denominator_coeffs,
        } => {
            let mut rational = CompuRationalT::default();
            rational.numerator_coeffs = Some(numerator_coeffs.clone());
            rational.denominator_coeffs = Some(denominator_coeffs.clone());
            CompuT::CompuRational(Box::new(rational))
        }
    }
}
