//! The rules of nested layouts (`docs/csv-tables.md` 5.4 and 6, ADR-242): shaped fields,
//! `repeats.csv` and `select_cases.csv`, layout bounds, cycles and the nesting limit.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{Checker, FILE};
use crate::tables::Tables;
use crate::tables::error::ErrorCode;
use crate::tables::rows::*;

/// The deepest nesting of layouts the decoder accepts (ADR-242 item 7).
const MAX_DEPTH: usize = 16;

/// The rows the layout rules look up, indexed once so that every rule is linear in the rows.
struct Index<'a> {
    /// Each field by its container and name.
    fields: HashMap<(&'a Container, &'a str), &'a FieldRow>,
    /// The fields of each layout, in row order.
    layout_fields: HashMap<&'a str, Vec<&'a FieldRow>>,
    /// The `repeats.csv` row of each field that has one.
    repeats: HashMap<(&'a Container, &'a str), &'a RepeatRow>,
    /// The layouts of each field's select cases, in row order.
    case_layouts: HashMap<(&'a Container, &'a str), Vec<&'a str>>,
}

impl<'a> Index<'a> {
    fn new(t: &'a Tables) -> Self {
        let mut index = Index {
            fields: HashMap::new(),
            layout_fields: HashMap::new(),
            repeats: HashMap::new(),
            case_layouts: HashMap::new(),
        };
        for f in &t.fields {
            index
                .fields
                .entry((&f.container, f.name.as_str()))
                .or_insert(f);
            if let Container::Layout(name) = &f.container {
                index
                    .layout_fields
                    .entry(name.as_str())
                    .or_default()
                    .push(f);
            }
        }
        for r in &t.repeats {
            index
                .repeats
                .entry((&r.container, r.field.as_str()))
                .or_insert(r);
        }
        for c in &t.select_cases {
            if let Some(layout) = &c.layout {
                index
                    .case_layouts
                    .entry((&c.container, c.field.as_str()))
                    .or_default()
                    .push(layout.as_str());
            }
        }
        index
    }

    fn repeat_row(&self, f: &'a FieldRow) -> Option<&'a RepeatRow> {
        self.repeats.get(&(&f.container, f.name.as_str())).copied()
    }

    /// The field a `repeats.csv` or `select_cases.csv` row belongs to.
    fn owner(&self, container: &'a Container, name: &'a str) -> Option<&'a FieldRow> {
        self.fields.get(&(container, name)).copied()
    }

    /// The layouts a shaped field leads to: its own, or its select cases'.
    fn targets(&self, f: &'a FieldRow) -> Vec<&'a str> {
        match f.shape {
            Some(Shape::Group | Shape::Repeat) => f.layout.iter().map(String::as_str).collect(),
            Some(Shape::Select) => self
                .case_layouts
                .get(&(&f.container, f.name.as_str()))
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn layout_fields(&self, layout: &str) -> &[&'a FieldRow] {
        self.layout_fields.get(layout).map_or(&[], Vec::as_slice)
    }
}

/// Where a field at a known position with a fixed width ends, in bits from its layout's start.
fn fixed_end(f: &FieldRow) -> Option<u64> {
    match (f.byte, f.bits, f.length) {
        (Some(byte), Some(bits), None | Some(Length::Fixed)) if f.shape.is_none() => {
            Some(u64::from(byte) * 8 + u64::from(f.bit) + u64::from(bits))
        }
        _ => None,
    }
}

impl<'a> Checker<'a> {
    pub(super) fn layouts(&mut self) {
        let t = self.t;
        let index = Index::new(t);
        for f in &t.fields {
            match f.shape {
                Some(shape) => self.shaped(&index, f, shape),
                None if f.layout.is_some() => self.field_error(
                    ErrorCode::BadValue,
                    f,
                    "layout",
                    "layout is used only by a group or a repeat".into(),
                ),
                None => {}
            }
        }
        self.repeats(&index);
        self.select_cases(&index);
        self.bounds(&index);
        self.fixed_items(&index);
        self.nesting(&index);
    }

    /// The columns of a shaped field (5.4).
    fn shaped(&mut self, index: &Index<'a>, f: &'a FieldRow, shape: Shape) {
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
            Shape::Repeat => index
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
            (false, Some(_)) if shape == Shape::Group || index.repeat_row(f).is_some() => self
                .field_error(
                    ErrorCode::BadValue,
                    f,
                    "ref",
                    "ref is used only by a select or a repeat counted by a field".into(),
                ),
            _ => {}
        }
        if shape == Shape::Repeat && index.repeat_row(f).is_none() {
            // A row of another table is missing, so no cell of this one is named, and no
            // error of a cell hides it.
            self.e.push(
                ErrorCode::MissingColumn,
                FILE,
                Some(f.line),
                None,
                "a repeat needs its row in repeats.csv",
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
        if key.constant.is_some() || key.shape.is_some() || !self.key_cells_sound(key) {
            // Reported by the reference rules, or by the key's own cells.
            return;
        }
        match key.field_type {
            Some(FieldType::Float) => self.field_error(
                ErrorCode::IrRule,
                f,
                "ref",
                format!("the key {} is a float; case limits are integers", key.name),
            ),
            Some(ty) if super::string_like(ty) && !short_key(key) => self.field_error(
                ErrorCode::IrRule,
                f,
                "ref",
                format!("the key {} can be longer than 8 bytes", key.name),
            ),
            _ => {}
        }
    }

    /// Every `repeats.csv` row belongs to a `repeat` field (5.4).
    fn repeats(&mut self, index: &Index<'a>) {
        let t = self.t;
        for r in &t.repeats {
            if index
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

    /// Whether a select key gives a range to judge its cases by: a scalar of a type a key may
    /// have, with no error of its own in the cells that set its range.
    fn sound_key(&self, key: &FieldRow) -> bool {
        key.constant.is_none()
            && key.shape.is_none()
            && key.field_type.is_some_and(|ty| ty != FieldType::Float)
            && (!key.field_type.is_some_and(super::string_like) || short_key(key))
            && self.key_cells_sound(key)
    }

    /// Whether the cells that set a key's type and width have no error of their own.
    fn key_cells_sound(&self, key: &FieldRow) -> bool {
        ["type", "bits", "length", "max_bytes"]
            .iter()
            .all(|column| !self.reported.contains(&(FILE, key.line, column)))
    }

    /// `select_cases.csv`: rows of `select` fields, at most one default per field, no overlaps,
    /// and limits the key can hold (5.4).
    fn select_cases(&mut self, index: &Index<'a>) {
        let t = self.t;
        let mut by_field: BTreeMap<Line, (&FieldRow, Vec<&SelectCaseRow>)> = BTreeMap::new();
        for c in &t.select_cases {
            let Some(f) = index.owner(&c.container, &c.field) else {
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
            by_field.entry(f.line).or_insert((f, Vec::new())).1.push(c);
        }
        for (f, cases) in by_field.into_values() {
            // A key with an error of its own gives no range to judge the cases by.
            let key = f
                .reference
                .as_ref()
                .and_then(|name| self.referred(f, name))
                .filter(|k| self.sound_key(k));
            let (min, max, signed) = key.map_or((i128::MIN, i128::MAX, true), key_range);
            let compared_whole = key
                .and_then(|k| k.field_type)
                .is_some_and(super::string_like);
            let mut default: Option<Line> = None;
            // The cases accepted so far, disjoint, by their lower limit: a new case overlaps one
            // of them exactly when it overlaps the one with the greatest lower limit not above
            // its own upper limit.
            let mut earlier: BTreeMap<i128, (i128, Line)> = BTreeMap::new();
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
                if compared_whole && lower != upper {
                    self.error(
                        ErrorCode::BadValue,
                        "select_cases.csv",
                        c.line,
                        "upper",
                        "a bytes or string key is compared for equality, so a case is one value"
                            .into(),
                    );
                }
                match earlier.range(..=upper).next_back() {
                    Some((_, &(u, first))) if u >= lower => self.error(
                        ErrorCode::Duplicate,
                        "select_cases.csv",
                        c.line,
                        "lower",
                        format!("the range overlaps the case on line {first}"),
                    ),
                    _ => {
                        earlier.insert(lower, (upper, c.line));
                    }
                }
            }
        }
    }

    /// Whether a field's width or position already has an error, so that it is not judged
    /// again against a layout or an item.
    fn placed(&self, f: &FieldRow) -> bool {
        !["bits", "byte"]
            .iter()
            .any(|column| self.reported.contains(&(FILE, f.line, column)))
    }

    /// A layout with a `byte_size` holds its fields: a field at a known position with a fixed
    /// width does not reach past it, and a shaped field does not start past it (`ir.fbs`
    /// `Layout`).
    fn bounds(&mut self, index: &Index<'a>) {
        let t = self.t;
        for layout in &t.layouts {
            if layout.byte_size == 0 {
                continue;
            }
            let limit = u64::from(layout.byte_size) * 8;
            for &f in index.layout_fields(&layout.name) {
                if f.shape.is_some() {
                    // A shaped field consumes its layouts from where it starts.
                    if let Some(byte) = f.byte
                        && byte > layout.byte_size
                    {
                        self.field_error(
                            ErrorCode::IrRule,
                            f,
                            "byte",
                            format!(
                                "the field starts past the {} bytes of layout {}",
                                layout.byte_size, layout.name
                            ),
                        );
                    }
                    continue;
                }
                if let Some(end) = fixed_end(f)
                    && end > limit
                    && self.placed(f)
                {
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

    /// A fixed repeat's items hold the fields of its layout: a field at a known position with a
    /// fixed width does not reach past `item_bytes`, and a shaped field does not start past it
    /// (`ir.fbs` `Repeat`).
    fn fixed_items(&mut self, index: &Index<'a>) {
        let t = self.t;
        let mut extents: HashMap<&str, Option<(u64, &FieldRow)>> = HashMap::new();
        for r in &t.repeats {
            let RepeatCount::Fixed { item_bytes, .. } = r.count else {
                continue;
            };
            let Some(layout) = index
                .owner(&r.container, &r.field)
                .and_then(|f| f.layout.as_ref())
            else {
                continue;
            };
            let limit = u64::from(item_bytes) * 8;
            // Each layout's extent is found once, however many repeats share it.
            let extent = *extents.entry(layout.as_str()).or_insert_with(|| {
                index
                    .layout_fields(layout)
                    .iter()
                    .filter(|f| self.placed(f))
                    .filter_map(|&f| {
                        let bits = match f.shape {
                            // A shaped field consumes its layouts from where it starts.
                            Some(_) => u64::from(f.byte?) * 8,
                            None => fixed_end(f)?,
                        };
                        Some((bits, f))
                    })
                    .max_by_key(|&(bits, f)| (bits, std::cmp::Reverse(f.line)))
            });
            if let Some((_, f)) = extent.filter(|&(bits, _)| bits > limit) {
                self.error(
                    ErrorCode::IrRule,
                    "repeats.csv",
                    r.line,
                    "item_bytes",
                    format!(
                        "field {} of layout {layout} reaches past the {item_bytes} bytes of an item",
                        f.name
                    ),
                );
            }
        }
    }

    /// No layout reaches itself, and no field nests more than 16 layouts (ADR-242 item 7).
    /// The layouts are walked without recursion, in time linear in their fields.
    fn nesting(&mut self, rows: &Index<'a>) {
        let t = self.t;
        let names: Vec<&str> = t.layouts.iter().map(|l| l.name.as_str()).collect();
        let index: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        // Each layout's edges: the layouts its shaped fields lead to.
        let adj: Vec<Vec<usize>> = names
            .iter()
            .map(|n| {
                rows.layout_fields(n)
                    .iter()
                    .flat_map(|f| rows.targets(f))
                    .filter_map(|c| index.get(c).copied())
                    .collect()
            })
            .collect();
        let (order, component) = components(&adj);
        // The members of each component, in row order.
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
        for (v, &c) in component.iter().enumerate() {
            members[c].push(v);
        }
        let on_cycle: Vec<bool> = (0..names.len())
            .map(|v| members[component[v]].len() > 1 || adj[v].contains(&v))
            .collect();
        // Each cycle is reported once, at its first layout in row order.
        let mut reported = HashSet::new();
        for v in 0..names.len() {
            if on_cycle[v] && reported.insert(component[v]) {
                let members: Vec<&str> = members[component[v]].iter().map(|&m| names[m]).collect();
                self.error(
                    ErrorCode::IrRule,
                    "layouts.csv",
                    t.layouts[v].line,
                    "name",
                    format!(
                        "layout {} reaches itself through {}",
                        names[v],
                        members.join(", ")
                    ),
                );
            }
        }
        // The depth of each layout off a cycle: itself plus its deepest child off a cycle, so a
        // cycle below hides no chain that is too deep. Children finish before their parents in
        // `order`, except along a cycle.
        let mut depth = vec![0usize; names.len()];
        for &v in &order {
            if on_cycle[v] {
                continue;
            }
            depth[v] = 1 + adj[v]
                .iter()
                .filter(|&&c| !on_cycle[c])
                .map(|&c| depth[c])
                .max()
                .unwrap_or(0);
        }
        for f in &t.fields {
            if matches!(f.container, Container::Layout(_)) {
                continue;
            }
            let deepest = rows
                .targets(f)
                .into_iter()
                .filter_map(|l| index.get(l).copied())
                .filter(|&v| !on_cycle[v])
                .map(|v| depth[v])
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

/// The strongly connected components of a graph (Kosaraju's algorithm, without recursion):
/// the vertices in the order the first search finishes them, and each vertex's component.
fn components(adj: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
    let n = adj.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    for start in 0..n {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0usize)];
        while let Some((v, next)) = stack.last_mut() {
            if let Some(&c) = adj[*v].get(*next) {
                *next += 1;
                if !visited[c] {
                    visited[c] = true;
                    stack.push((c, 0));
                }
            } else {
                order.push(*v);
                stack.pop();
            }
        }
    }
    let mut reverse = vec![Vec::new(); n];
    for (v, cs) in adj.iter().enumerate() {
        for &c in cs {
            reverse[c].push(v);
        }
    }
    let mut component = vec![usize::MAX; n];
    let mut count = 0;
    for &start in order.iter().rev() {
        if component[start] != usize::MAX {
            continue;
        }
        component[start] = count;
        let mut stack = vec![start];
        while let Some(v) = stack.pop() {
            for &c in &reverse[v] {
                if component[c] == usize::MAX {
                    component[c] = count;
                    stack.push(c);
                }
            }
        }
        count += 1;
    }
    (order, component)
}

/// Whether a bytes or string key is at most 8 bytes, as a key compared as an integer must be
/// (ADR-242): fixed at most 64 bits wide, or terminated with `max_bytes` 1 to 8.
fn short_key(key: &FieldRow) -> bool {
    match key.length.unwrap_or(Length::Fixed) {
        Length::Fixed => key.bits.is_some_and(|b| b <= 64),
        Length::Terminated => key.max_bytes.is_some_and(|m| (1..=8).contains(&m)),
        Length::Field | Length::Leading => false,
    }
}

/// The internal values a select key can hold, and whether they are signed (5.4): an `int`
/// key's signed range, a `uint` or byte or string key's unsigned range, by its width when it
/// is fixed or, for a terminated key, by its `max_bytes`.
fn key_range(key: &FieldRow) -> (i128, i128, bool) {
    let bits = match key.length {
        None | Some(Length::Fixed) => key.bits,
        // A terminated key holds at most `max_bytes` bytes.
        Some(Length::Terminated) => key
            .max_bytes
            .filter(|&m| m != 0)
            .map(|m| m.saturating_mul(8)),
        Some(Length::Field | Length::Leading) => None,
    }
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
