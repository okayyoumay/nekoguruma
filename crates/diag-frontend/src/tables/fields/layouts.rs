//! The rules of nested layouts (`docs/csv-tables.md` 5.4 and 6, ADR-242): shaped fields,
//! `repeats.csv` and `select_cases.csv`, layout bounds, cycles and the nesting limit.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{Checker, FILE};
use crate::tables::error::ErrorCode;
use crate::tables::rows::*;

/// The deepest nesting of layouts the decoder accepts (ADR-242 item 7).
const MAX_DEPTH: usize = 16;

impl<'a> Checker<'a> {
    pub(super) fn layouts(&mut self) {
        let t = self.t;
        for f in &t.fields {
            if let Some(shape) = f.shape {
                self.shaped(f, shape);
            }
        }
        self.repeats();
        self.select_cases();
        self.bounds();
        self.nesting();
    }

    /// The `repeats.csv` row of a `repeat` field, if any.
    fn repeat_row(&self, f: &FieldRow) -> Option<&'a RepeatRow> {
        self.t
            .repeats
            .iter()
            .find(|r| r.container == f.container && r.field == f.name)
    }

    /// The field a `repeats.csv` or `select_cases.csv` row belongs to.
    fn owner(&self, container: &Container, name: &str) -> Option<&'a FieldRow> {
        self.t
            .fields
            .iter()
            .find(|f| &f.container == container && f.name == name)
    }

    /// The columns of a shaped field (5.4).
    fn shaped(&mut self, f: &FieldRow, shape: Shape) {
        for (column, set) in [
            ("type", f.field_type.is_some()),
            ("bits", f.bits.is_some()),
            ("length", f.length.is_some()),
            ("min_bytes", f.min_bytes.is_some()),
            ("max_bytes", f.max_bytes.is_some()),
            ("terminator", f.terminator.is_some()),
            ("conversion", f.conversion.is_some()),
            ("unit", f.unit.is_some()),
        ] {
            if set {
                self.field_error(
                    ErrorCode::BadValue,
                    f,
                    column,
                    format!("{column} must be empty for a shaped field"),
                );
            }
        }
        if f.bit != 0 {
            self.field_error(
                ErrorCode::BadValue,
                f,
                "bit",
                "a shaped field starts at a byte edge (bit 0)".into(),
            );
        }
        if self.sent(&f.container) {
            self.field_error(
                ErrorCode::Unsupported,
                f,
                "shape",
                "this version defines no request value for a shaped field".into(),
            );
        }
        match shape {
            Shape::Group | Shape::Repeat if f.layout.is_none() => self.field_error(
                ErrorCode::MissingColumn,
                f,
                "layout",
                "a group or repeat needs a layout".into(),
            ),
            Shape::Select if f.layout.is_some() => self.field_error(
                ErrorCode::BadValue,
                f,
                "layout",
                "a select takes its layouts from its select_cases.csv rows".into(),
            ),
            _ => {}
        }
        let wants_ref = match shape {
            Shape::Group => false,
            Shape::Select => true,
            Shape::Repeat => self
                .repeat_row(f)
                .is_some_and(|r| r.count == RepeatCount::Field),
        };
        match (wants_ref, &f.reference) {
            (true, None) => self.field_error(
                ErrorCode::MissingColumn,
                f,
                "ref",
                "a select, or a repeat counted by a field, needs the field in ref".into(),
            ),
            (false, Some(_)) if shape == Shape::Group || self.repeat_row(f).is_some() => self
                .field_error(
                    ErrorCode::BadValue,
                    f,
                    "ref",
                    "ref is used only by a select or a repeat counted by a field".into(),
                ),
            _ => {}
        }
        if shape == Shape::Repeat && self.repeat_row(f).is_none() {
            self.field_error(
                ErrorCode::MissingColumn,
                f,
                "shape",
                "a repeat needs its row in repeats.csv".into(),
            );
        }
        if shape == Shape::Select {
            self.select_key(f);
        }
    }

    /// A select key is not a float (`ir.fbs` `Select`, ADR-242), and a key of bytes or a
    /// string is at most 8 bytes, the width compared as an integer.
    fn select_key(&mut self, f: &FieldRow) {
        let Some(key) = f.reference.as_ref().and_then(|name| self.referred(f, name)) else {
            return;
        };
        if key.constant.is_some() || key.shape.is_some() {
            // Reported by the reference rules.
            return;
        }
        match key.field_type {
            Some(FieldType::Float) => self.field_error(
                ErrorCode::IrRule,
                f,
                "ref",
                format!("the key {} is a float; case limits are integers", key.name),
            ),
            Some(ty)
                if super::string_like(ty)
                    && matches!(key.length, None | Some(Length::Fixed))
                    && key.bits.is_some_and(|b| b > 64) =>
            {
                self.field_error(
                    ErrorCode::IrRule,
                    f,
                    "ref",
                    format!("the key {} is longer than 8 bytes", key.name),
                )
            }
            _ => {}
        }
    }

    /// Every `repeats.csv` row belongs to a `repeat` field (5.4).
    fn repeats(&mut self) {
        let t = self.t;
        for r in &t.repeats {
            if self
                .owner(&r.container, &r.field)
                .is_some_and(|f| f.shape != Some(Shape::Repeat))
            {
                self.error(
                    ErrorCode::BadValue,
                    "repeats.csv",
                    r.line,
                    "field",
                    format!("{} is not a repeat field", r.field),
                );
            }
        }
    }

    /// `select_cases.csv`: rows of `select` fields, at most one default per field, no overlaps,
    /// and limits the key can hold (5.4).
    fn select_cases(&mut self) {
        let t = self.t;
        let mut by_field: BTreeMap<(Line, &str), Vec<&SelectCaseRow>> = BTreeMap::new();
        for c in &t.select_cases {
            let Some(f) = self.owner(&c.container, &c.field) else {
                continue;
            };
            if f.shape != Some(Shape::Select) {
                self.error(
                    ErrorCode::BadValue,
                    "select_cases.csv",
                    c.line,
                    "field",
                    format!("{} is not a select field", c.field),
                );
                continue;
            }
            by_field
                .entry((f.line, f.name.as_str()))
                .or_default()
                .push(c);
        }
        for ((line, _), cases) in by_field {
            let f = t.fields.iter().find(|f| f.line == line).expect("the owner");
            let key = f.reference.as_ref().and_then(|name| self.referred(f, name));
            let (min, max, signed) = key.map_or((i128::MIN, i128::MAX, true), key_range);
            let mut default: Option<Line> = None;
            let mut earlier: Vec<(i128, i128, Line)> = Vec::new();
            for c in cases {
                let Some((lower, upper)) = c.range else {
                    if let Some(first) = default {
                        self.error(
                            ErrorCode::Duplicate,
                            "select_cases.csv",
                            c.line,
                            "case",
                            format!("a second default case; the first is on line {first}"),
                        );
                    }
                    default.get_or_insert(c.line);
                    continue;
                };
                let mut fits = true;
                // An empty `upper` takes `lower`'s value, and is not reported again.
                let limits = if upper == lower {
                    vec![("lower", lower)]
                } else {
                    vec![("lower", lower), ("upper", upper)]
                };
                for (column, value) in limits {
                    if value < min || value > max {
                        fits = false;
                        self.error(
                            ErrorCode::BadValue,
                            "select_cases.csv",
                            c.line,
                            column,
                            format!("{value} is outside the key's range {min} to {max}"),
                        );
                    }
                }
                if fits && !signed && lower < 1 << 63 && upper >= 1 << 63 {
                    self.error(
                        ErrorCode::BadValue,
                        "select_cases.csv",
                        c.line,
                        "upper",
                        "the range crosses 2^63, so its limits would not be in order as int64"
                            .into(),
                    );
                }
                if lower > upper {
                    // Reported when the row was read.
                    continue;
                }
                if let Some(&(_, _, first)) =
                    earlier.iter().find(|(l, u, _)| lower <= *u && *l <= upper)
                {
                    self.error(
                        ErrorCode::Duplicate,
                        "select_cases.csv",
                        c.line,
                        "lower",
                        format!("the range overlaps the case on line {first}"),
                    );
                }
                earlier.push((lower, upper, c.line));
            }
        }
    }

    /// A layout with a `byte_size` holds its fields: a field at a known position with a fixed
    /// width does not reach past it (`ir.fbs` `Layout`).
    fn bounds(&mut self) {
        let t = self.t;
        for layout in &t.layouts {
            if layout.byte_size == 0 {
                continue;
            }
            let limit = u64::from(layout.byte_size) * 8;
            for f in &t.fields {
                if f.container != Container::Layout(layout.name.clone()) || f.shape.is_some() {
                    continue;
                }
                let (Some(byte), Some(bits), None | Some(Length::Fixed)) =
                    (f.byte, f.bits, f.length)
                else {
                    continue;
                };
                // A width or position already rejected is not judged again against the layout.
                let judged = ["bits", "byte"]
                    .iter()
                    .any(|column| self.reported.contains(&(FILE, f.line, column)));
                if judged {
                    continue;
                }
                let end = u64::from(byte) * 8 + u64::from(f.bit) + u64::from(bits);
                if end > limit {
                    self.field_error(
                        ErrorCode::IrRule,
                        f,
                        "byte",
                        format!(
                            "the field ends at bit {end}, past the {} bytes of layout {}",
                            layout.byte_size, layout.name
                        ),
                    );
                }
            }
        }
    }

    /// The layouts a layout's shaped fields lead to.
    fn children(&self) -> HashMap<&'a str, Vec<&'a str>> {
        let t = self.t;
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for l in &t.layouts {
            children.entry(l.name.as_str()).or_default();
        }
        for f in &t.fields {
            let Container::Layout(parent) = &f.container else {
                continue;
            };
            for child in self.targets(f) {
                children.entry(parent.as_str()).or_default().push(child);
            }
        }
        children
    }

    /// The layouts a shaped field leads to: its own, or its select cases'.
    fn targets(&self, f: &'a FieldRow) -> Vec<&'a str> {
        match f.shape {
            Some(Shape::Group | Shape::Repeat) => f.layout.iter().map(String::as_str).collect(),
            Some(Shape::Select) => self
                .t
                .select_cases
                .iter()
                .filter(|c| c.container == f.container && c.field == f.name)
                .filter_map(|c| c.layout.as_deref())
                .collect(),
            None => Vec::new(),
        }
    }

    /// No layout reaches itself, and no field nests more than 16 layouts (ADR-242 item 7).
    fn nesting(&mut self) {
        let t = self.t;
        let children = self.children();
        let reach = |from: &str| -> HashSet<&str> {
            let mut seen = HashSet::new();
            let mut stack: Vec<&str> = children.get(from).cloned().unwrap_or_default();
            while let Some(l) = stack.pop() {
                if seen.insert(l) {
                    stack.extend(children.get(l).into_iter().flatten().copied());
                }
            }
            seen
        };
        let reaches: HashMap<&str, HashSet<&str>> = t
            .layouts
            .iter()
            .map(|l| (l.name.as_str(), reach(&l.name)))
            .collect();
        let on_cycle = |l: &str| reaches.get(l).is_some_and(|r| r.contains(l));
        // Each cycle is reported once, at its first layout in row order.
        let mut reported: HashSet<&str> = HashSet::new();
        for l in &t.layouts {
            let name = l.name.as_str();
            if !on_cycle(name) || reported.contains(name) {
                continue;
            }
            let members: Vec<&str> = t
                .layouts
                .iter()
                .map(|m| m.name.as_str())
                .filter(|m| reaches[name].contains(m) && reaches[*m].contains(name))
                .collect();
            reported.extend(&members);
            self.error(
                ErrorCode::IrRule,
                "layouts.csv",
                l.line,
                "name",
                format!(
                    "layout {name} reaches itself through {}",
                    members.join(", ")
                ),
            );
        }
        // The depth of each layout that reaches no cycle: itself plus its deepest child.
        let mut depth: HashMap<&str, usize> = HashMap::new();
        fn depth_of<'n>(
            l: &'n str,
            children: &HashMap<&'n str, Vec<&'n str>>,
            depth: &mut HashMap<&'n str, usize>,
        ) -> usize {
            if let Some(&d) = depth.get(l) {
                return d;
            }
            let d = 1 + children
                .get(l)
                .into_iter()
                .flatten()
                .map(|c| depth_of(c, children, depth))
                .max()
                .unwrap_or(0);
            depth.insert(l, d);
            d
        }
        let cyclic = |l: &str| on_cycle(l) || reaches[l].iter().any(|m| on_cycle(m));
        for f in &t.fields {
            if matches!(f.container, Container::Layout(_)) {
                continue;
            }
            let deepest = self
                .targets(f)
                .into_iter()
                .filter(|l| reaches.contains_key(l) && !cyclic(l))
                .map(|l| depth_of(l, &children, &mut depth))
                .max();
            if let Some(d) = deepest
                && d > MAX_DEPTH
            {
                self.field_error(
                    ErrorCode::IrRule,
                    f,
                    "shape",
                    format!("the field nests {d} layouts; at most {MAX_DEPTH} are decoded"),
                );
            }
        }
    }
}

/// The internal values a select key can hold, and whether they are signed (5.4): an `int`
/// key's signed range, a `uint` or byte or string key's unsigned range, by its width when it
/// is fixed.
fn key_range(key: &FieldRow) -> (i128, i128, bool) {
    let bits = key
        .bits
        .filter(|_| matches!(key.length, None | Some(Length::Fixed)))
        .map(|b| b.min(64));
    match key.field_type {
        Some(FieldType::Int) => {
            let b = bits.unwrap_or(64).max(1);
            (-(1i128 << (b - 1)), (1i128 << (b - 1)) - 1, true)
        }
        Some(FieldType::Bcd) => {
            let digits = bits.unwrap_or(64) / 4;
            (0, 10i128.pow(digits.min(19)) - 1, false)
        }
        _ => (0, (1i128 << bits.unwrap_or(64)) - 1, false),
    }
}
