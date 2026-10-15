//! The rules of single fields (`docs/csv-tables.md` 4.5, 4.6, 4.11, 5.2, 5.3, 5.5 and 6): the
//! third stage of reading a definition, which runs only on a definition whose references
//! resolve (ADR-298). The rules of nested layouts (5.4) are not checked here.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::error::{ErrorCode, Errors};
use super::resolve::Definition;
use super::rows::*;
use super::{Tables, values};

const FILE: &str = "fields.csv";

/// Checks the fields of `definition`, adding every error found to `errors`.
pub(super) fn check(definition: &Definition, errors: &mut Errors) {
    let mut checker = Checker {
        t: &definition.tables,
        e: errors,
        reported: HashSet::new(),
    };
    checker.fields();
    checker.containers();
    checker.status_layouts();
    checker.services(definition);
}

struct Checker<'a> {
    t: &'a Tables,
    e: &'a mut Errors,
    /// The cells already reported, as (file, line, column): one error per cell, the first rule
    /// that fails it.
    reported: HashSet<(&'static str, Line, &'static str)>,
}

/// Whether a type is bytes or a string, whose value may have a variable length.
fn string_like(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Ascii | FieldType::Bytes | FieldType::Utf8 | FieldType::Unicode2
    )
}

/// The number of bytes before a container's position 0 in the message (5.2).
fn base(container: &Container) -> u64 {
    match container {
        Container::Request(_) | Container::Response(_) => 1,
        Container::Did(_) => 3,
        Container::Routine { .. } => 4,
        Container::Layout(_) => 0,
    }
}

impl<'a> Checker<'a> {
    fn error(
        &mut self,
        code: ErrorCode,
        file: &'static str,
        line: Line,
        column: &'static str,
        hint: String,
    ) {
        if self.reported.insert((file, line, column)) {
            self.e.push(code, file, Some(line), Some(column), hint);
        }
    }

    fn field_error(&mut self, code: ErrorCode, f: &FieldRow, column: &'static str, hint: String) {
        self.error(code, FILE, f.line, column, hint);
    }

    fn conversion(&self, name: &str) -> Option<&'a ConversionKind> {
        self.t
            .conversions
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.kind)
    }

    /// The field `name` refers to: the earlier field of the same container (resolved by the
    /// second stage).
    fn referred(&self, f: &FieldRow, name: &str) -> Option<&'a FieldRow> {
        self.t
            .fields
            .iter()
            .take_while(|g| g.line != f.line)
            .filter(|g| g.container == f.container && g.name == name)
            .last()
    }

    /// The rules each field obeys on its own (4.5, 5.2, 6).
    fn fields(&mut self) {
        let t = self.t;
        for f in &t.fields {
            self.position(f);
            self.reference(f);
            if f.shape.is_none() {
                self.scalar(f);
            }
        }
    }

    /// The IR `bit_offset` of an `Absolute` field fits in 32 bits (5.2).
    fn position(&mut self, f: &FieldRow) {
        if let Some(byte) = f.byte {
            let offset = (base(&f.container) + u64::from(byte)) * 8 + u64::from(f.bit);
            if offset > u64::from(u32::MAX) {
                self.field_error(
                    ErrorCode::BadValue,
                    f,
                    "byte",
                    format!("the bit offset {offset} does not fit in 32 bits"),
                );
            }
        }
    }

    /// What a `ref` may name (5.2): never a constant, which has no IR field, nor a shaped field;
    /// for a `field` length, a fixed `uint`, `int` or `bcd` with an identity or linear
    /// conversion.
    fn reference(&mut self, f: &FieldRow) {
        let Some(name) = &f.reference else {
            return;
        };
        let Some(target) = self.referred(f, name) else {
            return;
        };
        if target.constant.is_some() {
            self.field_error(
                ErrorCode::UnknownReference,
                f,
                "ref",
                format!("{name} is a constant, which has no field to refer to"),
            );
        } else if target.shape.is_some() {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "ref",
                format!("{name} is a shaped field, not a value"),
            );
        } else if f.shape.is_none() && f.length == Some(Length::Field) {
            let numeric = matches!(
                target.field_type,
                Some(FieldType::Uint | FieldType::Int | FieldType::Bcd)
            );
            let fixed = matches!(target.length, None | Some(Length::Fixed));
            let conversion = match &target.conversion {
                None => true,
                Some(c) => matches!(
                    self.conversion(c),
                    Some(ConversionKind::Identity | ConversionKind::Linear { .. })
                ),
            };
            if !(numeric && fixed && conversion) {
                self.field_error(
                    ErrorCode::BadValue,
                    f,
                    "ref",
                    format!(
                        "the length field {name} must be a fixed uint, int or bcd with an \
                         identity or linear conversion"
                    ),
                );
            }
        }
    }

    /// The columns of an unshaped field by its type and length kind (4.5, 5.2, 6).
    fn scalar(&mut self, f: &FieldRow) {
        let Some(ty) = f.field_type else {
            return;
        };
        let length = f.length.unwrap_or(Length::Fixed);
        let strings = string_like(ty);
        match length {
            Length::Fixed => match f.bits {
                None => self.field_error(
                    ErrorCode::MissingColumn,
                    f,
                    "bits",
                    "a fixed field needs bits".into(),
                ),
                Some(bits) => self.width(f, ty, bits),
            },
            Length::Leading | Length::Terminated if !strings => self.field_error(
                ErrorCode::IrRule,
                f,
                "length",
                "only bytes and strings take a leading length or a terminator".into(),
            ),
            Length::Leading => match f.bits {
                None => self.field_error(
                    ErrorCode::MissingColumn,
                    f,
                    "bits",
                    "a leading length needs the width of its prefix in bits".into(),
                ),
                Some(bits) if bits > 32 => self.field_error(
                    ErrorCode::BadValue,
                    f,
                    "bits",
                    "a leading length prefix is 1 to 32 bits wide".into(),
                ),
                Some(_) => {}
            },
            Length::Field | Length::Terminated => {
                if f.bits.is_some() {
                    self.field_error(
                        ErrorCode::BadValue,
                        f,
                        "bits",
                        "bits must be empty for a field or terminated length".into(),
                    );
                }
            }
        }
        // Bytes and strings, and a terminated value in particular, start at a byte edge.
        if strings && f.bit != 0 {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "bit",
                "bytes and strings start at a byte edge (bit 0)".into(),
            );
        }
        if length == Length::Terminated {
            if let (Some(min), Some(max)) = (f.min_bytes, f.max_bytes)
                && max != 0
                && min > max
            {
                self.field_error(
                    ErrorCode::IrRule,
                    f,
                    "min_bytes",
                    format!("min_bytes {min} is above max_bytes {max}"),
                );
            }
        } else {
            for (column, set) in [
                ("min_bytes", f.min_bytes.is_some()),
                ("max_bytes", f.max_bytes.is_some()),
                ("terminator", f.terminator.is_some()),
            ] {
                if set {
                    self.field_error(
                        ErrorCode::BadValue,
                        f,
                        column,
                        format!("{column} is used only by a terminated length"),
                    );
                }
            }
        }
        if length != Length::Field && f.reference.is_some() {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "ref",
                "an unshaped field uses ref only for a field length".into(),
            );
        } else if length == Length::Field && f.reference.is_none() {
            self.field_error(
                ErrorCode::MissingColumn,
                f,
                "ref",
                "a field length needs the field that holds it".into(),
            );
        }
        if let Some(name) = &f.conversion {
            let kind = self.conversion(name).cloned();
            match kind {
                Some(ConversionKind::Identity) | None => {}
                Some(ConversionKind::TextTable { .. }) if ty == FieldType::Float => self
                    .field_error(
                        ErrorCode::IrRule,
                        f,
                        "conversion",
                        "a text-table conversion cannot apply to a float".into(),
                    ),
                Some(kind) if strings => self.field_error(
                    ErrorCode::BadValue,
                    f,
                    "conversion",
                    format!("a {} conversion needs a numeric field", kind.keyword()),
                ),
                Some(_) => {}
            }
        }
    }

    /// The widths a fixed field of each type accepts (5.2).
    fn width(&mut self, f: &FieldRow, ty: FieldType, bits: u32) {
        let (ok, rule) = match ty {
            FieldType::Uint | FieldType::Int => ((1..=64).contains(&bits), "1 to 64 bits"),
            FieldType::Float => (bits == 32 || bits == 64, "32 or 64 bits"),
            FieldType::Bcd => (
                bits.is_multiple_of(4) && bits <= 64,
                "a multiple of 4 bits, up to 64",
            ),
            FieldType::Bytes | FieldType::Ascii | FieldType::Utf8 => {
                (bits.is_multiple_of(8), "a multiple of 8 bits")
            }
            FieldType::Unicode2 => (bits.is_multiple_of(16), "a multiple of 16 bits"),
        };
        if !ok {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "bits",
                format!("a fixed field of this type is {rule} wide"),
            );
        }
        if ty == FieldType::Float && f.bit != 0 {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "bit",
                "a float starts at a byte edge (bit 0)".into(),
            );
        }
    }

    /// The fields of each container, in row order.
    fn by_container(&self) -> Vec<(&'a Container, Vec<&'a FieldRow>)> {
        let t = self.t;
        let mut index: HashMap<&Container, usize> = HashMap::new();
        let mut out: Vec<(&Container, Vec<&FieldRow>)> = Vec::new();
        for f in &t.fields {
            let i = *index.entry(&f.container).or_insert_with(|| {
                out.push((&f.container, Vec::new()));
                out.len() - 1
            });
            out[i].1.push(f);
        }
        out
    }

    /// Whether a container is sent: a request, or the record of a DID with a write service
    /// (5.5).
    fn sent(&self, container: &Container) -> bool {
        match container {
            Container::Request(_) => true,
            Container::Routine { response, .. } => !response,
            Container::Did(name) => self.t.dids.iter().any(|d| &d.name == name && d.write),
            Container::Response(_) | Container::Layout(_) => false,
        }
    }

    /// The constants (5.3) and, in a container that is sent, the request values (5.5).
    fn containers(&mut self) {
        for (container, fields) in self.by_container() {
            let end = self.constants(container, &fields);
            if self.sent(container) {
                self.request(&fields, end);
            }
        }
    }

    /// Checks a container's constants; returns the byte at which they end, when known.
    fn constants(&mut self, container: &Container, fields: &[&FieldRow]) -> Option<u64> {
        let leading = fields.iter().take_while(|f| f.constant.is_some()).count();
        let mut end = Some(0u64);
        for (i, f) in fields.iter().enumerate() {
            let Some(value) = &f.constant else {
                continue;
            };
            if matches!(container, Container::Layout(_)) {
                self.field_error(
                    ErrorCode::Unsupported,
                    f,
                    "const",
                    "a layout has no prefix for a constant".into(),
                );
                continue;
            }
            if i >= leading {
                self.field_error(
                    ErrorCode::Unsupported,
                    f,
                    "const",
                    "constants must be the first fields of their container".into(),
                );
                continue;
            }
            end = self.constant(f, value, end);
        }
        if leading == 0 { Some(0) } else { end }
    }

    /// One leading constant starting at `start` (when known); returns where it ends.
    fn constant(&mut self, f: &FieldRow, value: &str, start: Option<u64>) -> Option<u64> {
        let bad = |checker: &mut Self, column: &'static str, hint: &str| {
            checker.field_error(ErrorCode::BadValue, f, column, hint.into());
        };
        match (f.byte, start) {
            (Some(byte), Some(start)) if u64::from(byte) != start => bad(
                self,
                "byte",
                "constants start at byte 0 and follow each other without gaps",
            ),
            _ => {}
        }
        if f.shape.is_some() {
            bad(self, "shape", "a constant has no shape");
            return None;
        }
        if !matches!(f.length, None | Some(Length::Fixed)) {
            bad(self, "length", "a constant has a fixed length");
        }
        if f.bit != 0 {
            bad(self, "bit", "a constant is byte-aligned (bit 0)");
        }
        for (column, set) in [
            ("conversion", f.conversion.is_some()),
            ("unit", f.unit.is_some()),
        ] {
            if set {
                bad(self, column, "a constant has no conversion or unit");
            }
        }
        if !f.big_endian {
            bad(self, "byte_order", "constants are written big-endian");
        }
        let bits = f.bits.filter(|b| b.is_multiple_of(8));
        if bits.is_none() {
            bad(self, "bits", "a constant is a whole number of bytes wide");
        }
        match (f.field_type, bits) {
            (Some(FieldType::Uint), Some(bits)) if bits <= 64 => {
                let max = if bits == 64 {
                    i128::from(u64::MAX)
                } else {
                    (1i128 << bits) - 1
                };
                if let Err(hint) = values::integer(value, false, 0, max) {
                    bad(self, "const", &hint);
                }
            }
            (Some(FieldType::Bytes), Some(bits)) => match values::bytes(value) {
                Ok(bytes) if bytes.len() as u64 * 8 == u64::from(bits) => {}
                Ok(bytes) => bad(
                    self,
                    "const",
                    &format!("{} bytes for a field of {} bytes", bytes.len(), bits / 8),
                ),
                Err(hint) => bad(self, "const", &hint),
            },
            (Some(FieldType::Uint | FieldType::Bytes), _) => {}
            _ => bad(self, "type", "a constant is a uint or bytes"),
        }
        let start = f.byte.map(u64::from).or(start)?;
        bits.map(|b| start + u64::from(b / 8))
    }

    /// The fields after the constants of a container that is sent: one encoding each, whole
    /// bytes, one after the other (5.5). `end` is where the constants end, when known.
    fn request(&mut self, fields: &[&FieldRow], mut end: Option<u64>) {
        let rest: Vec<&FieldRow> = fields
            .iter()
            .copied()
            .skip_while(|f| f.constant.is_some())
            .collect();
        for (i, f) in rest.iter().enumerate() {
            let unsupported = |checker: &mut Self, column: &'static str, hint: &str| {
                checker.field_error(ErrorCode::Unsupported, f, column, hint.into());
            };
            if f.constant.is_some() || f.shape.is_some() {
                // Reported as a constant out of place, or left to the nested-layout rules.
                end = None;
                continue;
            }
            let length = f.length.unwrap_or(Length::Fixed);
            match length {
                Length::Field => unsupported(
                    self,
                    "length",
                    "a request value cannot take its length from another field",
                ),
                Length::Terminated => {
                    if !matches!(f.terminator, None | Some(Terminator::End)) {
                        unsupported(
                            self,
                            "terminator",
                            "a request value ends only at the end of the message",
                        );
                    }
                    if i + 1 != rest.len() {
                        unsupported(
                            self,
                            "length",
                            "a terminated request value must be the container's last field",
                        );
                    }
                }
                Length::Fixed | Length::Leading => {}
            }
            if f.bit != 0 {
                unsupported(self, "bit", "a request value is byte-aligned (bit 0)");
            }
            if matches!(length, Length::Fixed | Length::Leading)
                && f.bits.is_some_and(|b| !b.is_multiple_of(8))
            {
                unsupported(self, "bits", "a request value is whole bytes wide");
            }
            if let Some(byte) = f.byte
                && end != Some(u64::from(byte))
            {
                unsupported(
                    self,
                    "byte",
                    "a request value starts where the previous field ends",
                );
            }
            let start = f.byte.map(u64::from).or(end);
            end = match (length, f.bits) {
                (Length::Fixed, Some(bits)) if bits.is_multiple_of(8) => {
                    start.map(|s| s + u64::from(bits / 8))
                }
                _ => None,
            };
        }
    }

    /// The fields of a DTC's status layout lie within bits 0 to 7 of byte 0 (4.11).
    fn status_layouts(&mut self) {
        let t = self.t;
        let status: HashSet<&str> = t.dtcs.iter().map(|d| d.status.as_str()).collect();
        for (container, fields) in self.by_container() {
            let Container::Layout(name) = container else {
                continue;
            };
            if !status.contains(name.as_str()) {
                continue;
            }
            for (i, f) in fields.iter().enumerate() {
                // An AfterPrevious field after another starts at the next byte edge.
                let in_byte_0 = f.byte == Some(0) || (i == 0 && f.byte.is_none());
                if !in_byte_0 {
                    self.field_error(
                        ErrorCode::BadValue,
                        f,
                        "byte",
                        format!("the fields of status layout {name} lie in byte 0"),
                    );
                }
                let fits = matches!(f.length, None | Some(Length::Fixed))
                    && f.bits.is_some_and(|b| u32::from(f.bit) + b <= 8);
                if !fits {
                    self.field_error(
                        ErrorCode::BadValue,
                        f,
                        "bits",
                        format!("the fields of status layout {name} lie within bits 0 to 7"),
                    );
                }
            }
        }
    }

    /// Two services of one variant that one `diag.request` call could both mean (4.6, 5.3):
    /// the same SID, the same request field names, and no name a constant in both with
    /// different values. The later declared service is reported.
    fn services(&mut self, definition: &Definition) {
        let t = self.t;
        let mut requests: HashMap<&Container, Vec<&FieldRow>> = HashMap::new();
        for f in &t.fields {
            requests.entry(&f.container).or_default().push(f);
        }
        let names = |container: &Container| -> Option<BTreeMap<String, Option<Vec<u8>>>> {
            let mut out = BTreeMap::new();
            for f in requests.get(container).into_iter().flatten() {
                let value = match &f.constant {
                    None => None,
                    Some(c) => Some(constant_bytes(f, c)?),
                };
                out.insert(f.name.clone(), value);
            }
            Some(out)
        };
        // (sid, request fields, the services.csv line of a declared service).
        type Candidate = (u8, BTreeMap<String, Option<Vec<u8>>>, Option<Line>);
        let mut reported = HashSet::new();
        for variant in &definition.variants {
            let mut candidates: Vec<Candidate> = Vec::new();
            for &i in &variant.services {
                let s = &t.services[i];
                if let Some(fields) = names(&Container::Request(s.name.clone())) {
                    candidates.push((s.sid, fields, Some(s.line)));
                }
            }
            for &i in &variant.dids {
                let d = &t.dids[i];
                let did = Some(d.did.to_be_bytes().to_vec());
                if d.read {
                    let fields = BTreeMap::from([("did".to_owned(), did.clone())]);
                    candidates.push((0x22, fields, None));
                }
                if d.write
                    && let Some(mut fields) = names(&Container::Did(d.name.clone()))
                {
                    fields.insert("did".to_owned(), did);
                    candidates.push((0x2E, fields, None));
                }
            }
            for (j, (sid, fields, line)) in candidates.iter().enumerate() {
                let Some(line) = line else {
                    continue;
                };
                let clash =
                    candidates
                        .iter()
                        .enumerate()
                        .any(|(k, (other_sid, other, other_line))| {
                            k != j
                        && other_sid == sid
                        && other.keys().eq(fields.keys())
                        && fields.iter().all(|(name, value)| {
                            match (value, &other[name]) {
                                (Some(a), Some(b)) => a == b,
                                _ => true,
                            }
                        })
                        // Between two declared services, the later one is reported.
                        && other_line.is_none_or(|other_line| other_line < *line)
                        });
                if clash && reported.insert(*line) {
                    self.error(
                        ErrorCode::Duplicate,
                        "services.csv",
                        *line,
                        "name",
                        format!(
                            "no diag.request call can tell this service from another with SID \
                             0x{sid:02X}: same request field names and no constant that differs"
                        ),
                    );
                }
            }
        }
    }
}

/// A constant's value as the bytes it is written as, or `None` if it does not parse (reported
/// by the constant rules).
fn constant_bytes(f: &FieldRow, value: &str) -> Option<Vec<u8>> {
    let width = usize::try_from(f.bits? / 8).ok()?;
    match f.field_type? {
        FieldType::Uint if (1..=8).contains(&width) => {
            let v = values::integer(value, false, 0, i128::from(u64::MAX)).ok()? as u64;
            Some(v.to_be_bytes()[8 - width..].to_vec())
        }
        FieldType::Bytes => values::bytes(value).ok(),
        _ => None,
    }
}
