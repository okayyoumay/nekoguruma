//! Lowering a checked Starlark procedure to a `diag_ir::Program` (subset 2.5, 4.1, 4.4;
//! ADR-254, ADR-304).
//!
//! The core of the lowering: top-level constants (folded at ingestion), top-level `def`s with
//! positional parameters, calls between them, locals, the operators the IR's instructions
//! cover, `and`, `or`, `not`, conditional expressions, `if`/`elif`/`else`, `for` loops
//! (ADR-305), and `diag.fail`, `diag.wait`, `diag.log` and `diag.capture`. Every value that
//! reaches run time has one type, inferred at ingestion (ADR-304); an operation the IR cannot
//! express for its operands is `STAR_NOT_LOWERABLE`.
//!
//! Not lowered yet, each its own backlog item: the other `diag` functions (`hmi` and `record`;
//! `routine` and `security_access`; `flash_transfer`), reading `dtcs` of `diag.read_dtc`, built-ins and
//! string methods on values known only at run time, and section and idempotency annotations.
//! Each is rejected with `STAR_NOT_LOWERABLE` (or `STAR_API_NOT_AVAILABLE` for a `diag`
//! function without an instruction), so nothing is lowered silently wrong.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use diag_ir::declaration::{EcuVariant, LengthKind, Request};
use diag_ir::encode::{ParamValue, encode_request};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, SourceSpan};
use starlark_syntax::codemap::Span;
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignOp, AssignTargetP, AstArgument, AstExpr, AstLiteral, AstNoPayload, AstStmt,
    BinOp, DefP, ExprP, ForP, ParameterP, StmtP,
};
use starlark_syntax::syntax::uniplate::Visit;

use crate::profile::{Intent, Part, ResultMap, Source};

use super::{
    Annotation, BUILTINS, Checked, Pending, RequestConstant, RequestIndex, STRING_METHODS,
    StarCode, StarError, collect_locals, is_string_expression, resolve, to_u32,
};

/// The largest constant a fold or literal may build ([`Const::size`]), so a constant expression
/// cannot make ingestion allocate or work without bound.
const MAX_CONSTANT_SIZE: usize = 1 << 20;

/// The total [`Const::size`] of the constants one procedure may build: each is bounded by
/// [`MAX_CONSTANT_SIZE`], and this bounds how much many of them hold together.
const MAX_CONSTANTS_TOTAL: usize = 16 << 20;

/// How much work folding may do over the whole lowering, every pass included: each fold is
/// charged the sizes ([`Const::size`]) of the constants it reads, which bounds what it walks.
/// Naming a constant shares it, so this, not the constant budget, bounds repeated searches and
/// comparisons over one large constant.
const MAX_FOLD_WORK: usize = 1 << 30;

const TOO_MANY_CONSTANTS: &str =
    "the procedure's constants together hold more than 16 Mi values and bytes (4.4)";
const TOO_MUCH_FOLDING: &str =
    "folding the procedure's constants reads more than 2^30 values and bytes in all (4.4)";

/// The most unrolling one pass over the function bodies may do, counting each iteration and
/// each statement and expression lowered in an unrolled body, nested loops multiplying, so a
/// long constant list cannot make the program, or the work of lowering it, grow without bound
/// (subset 4.2).
const MAX_UNROLLED: usize = 1 << 20;

/// The bytes of a name that unrolling may copy, hash or compare for one unit of
/// [`MAX_UNROLLED`]. Each unrolled copy handles its names again, so a long name repeated in many
/// iterations costs in proportion to its length; a name shorter than this costs nothing extra.
const NAME_BYTES_PER_UNIT: usize = 64;

/// The limit of a `range` loop whose bound is known only at run time and that has no
/// `@maxIterations` (subset 4.2).
const DEFAULT_MAX_ITERATIONS: u32 = 1000;

/// The `Fail` code of a loop that runs past its limit (subset 4.2, ADR-280).
const LOOP_LIMIT_EXCEEDED: &str = "LOOP_LIMIT_EXCEEDED";

const TOO_MUCH_UNROLLING: &str = "the procedure's constant loops unroll to more than 2^20 \
     iterations, statements and expressions in all (4.2)";

/// How deeply constant lists and dicts may nest ([`Const::depth`]). Each literal is shallow,
/// but globals can nest one in the next, and folding recurses through every level.
const MAX_CONSTANT_DEPTH: usize = 64;

/// How many passes type inference may take before it gives up (ADR-304). Each pass can only
/// widen a type, so real procedures settle in a few; a file that needs more is rejected rather
/// than analysed without bound.
const MAX_TYPE_PASSES: usize = 256;

/// The longest `diag.fail` code (subset 3.1).
const MAX_FAIL_CODE_LEN: usize = 64;

/// Lowers `checked`, the procedure in `file`, to a program for `variant`, against whose
/// declarations it is lowered: every procedure goes with every variant, and is lowered once per
/// variant (ADR-307, ADR-311). Every reason the procedure cannot be lowered is reported, in
/// source order.
///
/// The lowering walks the AST recursively, so it runs on the parser's thread
/// ([`super::on_parser_stack`]): only that stack is large enough for every file within the
/// nesting bounds (subset 2.8, ADR-303).
pub fn lower(
    file: &str,
    checked: &Checked,
    variant: EcuVariant<'_>,
    requests: &RequestIndex,
) -> Result<Lowered, Vec<StarError>> {
    super::on_parser_stack(|| lower_here(file, checked, variant, requests))
}

/// A lowered procedure and the warnings its lowering reported (such as
/// `STAR_LOOP_LIMIT_DEFAULTED`, subset 4.2), which do not reject it.
#[derive(Debug)]
pub struct Lowered {
    pub program: Program,
    pub warnings: Vec<StarError>,
}

fn lower_here<'a>(
    file: &str,
    checked: &'a Checked,
    variant: EcuVariant<'a>,
    requests: &'a RequestIndex,
) -> Result<Lowered, Vec<StarError>> {
    let mut lowerer = Lowerer::new(checked, variant, requests);
    lowerer.annotations();
    lowerer.constants();
    lowerer.globals_built = lowerer.built;
    lowerer.functions();
    if lowerer.errors.is_empty() {
        lowerer.infer();
        lowerer.reject_if_exhausted();
    }
    if lowerer.errors.is_empty() {
        lowerer.emit_all();
        lowerer.reject_if_exhausted();
    }
    if !lowerer.errors.is_empty() {
        let errors = std::mem::take(&mut lowerer.errors);
        return Err(resolve(file, &checked.module, errors));
    }
    let program = lowerer.assemble();
    // Every check `validate` makes is one the lowering guarantees by construction.
    if let Err(error) = program.validate() {
        let span = checked.module.statement().span;
        return Err(resolve(
            file,
            &checked.module,
            vec![Pending {
                code: StarCode::NotLowerable,
                span: Span::new(span.begin(), span.begin()),
                hint: format!("the lowered program is not valid: {error}"),
            }],
        ));
    }
    let warnings = std::mem::take(&mut lowerer.warnings);
    Ok(Lowered {
        program,
        warnings: resolve(file, &checked.module, warnings),
    })
}

// ---------------------------------------------------------------- Types and constants

/// A run-time type (subset 2.5): `str` exists at run time as UTF-8 bytes, and the response of
/// `diag.request` (3.4) as the response bytes, read only through its literal keys (ADR-317).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    Float,
    Bool,
    Bytes,
    Str,
    Response,
    /// A response's `fields` (subset 3.4): the response bytes, read only by a literal field
    /// name, which decodes the field through the declaration part (ADR-316, ADR-320).
    Fields,
}

const TYS: [Ty; 7] = [
    Ty::Int,
    Ty::Float,
    Ty::Bool,
    Ty::Bytes,
    Ty::Str,
    Ty::Response,
    Ty::Fields,
];

impl Ty {
    fn bit(self) -> u8 {
        match self {
            Ty::Int => 1,
            Ty::Float => 2,
            Ty::Bool => 4,
            Ty::Bytes => 8,
            Ty::Str => 16,
            Ty::Response => 32,
            Ty::Fields => 64,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::Bytes => "bytes",
            Ty::Str => "str",
            Ty::Response => "response",
            Ty::Fields => "response fields",
        }
    }
}

/// The types a value may have at run time, inferred at ingestion. Empty until something is
/// known about it. A response, and its fields, also carry the declared service they come from,
/// which decides what a field name means (ADR-320).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tys {
    bits: u8,
    service: Service,
}

/// The declared service a response comes from, as far as it is known at ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Service {
    /// No response, or one whose call was already reported as an error.
    #[default]
    Unknown,
    /// The `Service.id` of the one request the response can come from.
    One(u32),
    /// A `diag.read_dtc` result from the service of that `Service.id`: its `fields` are the
    /// names the profile maps the response to, not the declared ones (ADR-314 item 4).
    Dtcs(u32),
    /// Requests to more than one service: a variable assigned from both anywhere in its
    /// function (types are per variable, subset 4.8), or a parameter whose callers pass both.
    Several,
}

impl Service {
    fn union(self, other: Service) -> Service {
        match (self, other) {
            (Service::Unknown, s) | (s, Service::Unknown) => s,
            (Service::One(a), Service::One(b)) if a == b => Service::One(a),
            (Service::Dtcs(a), Service::Dtcs(b)) if a == b => Service::Dtcs(a),
            _ => Service::Several,
        }
    }
}

impl Tys {
    fn of(ty: Ty) -> Self {
        Tys {
            bits: ty.bit(),
            service: Service::Unknown,
        }
    }

    /// A response, or its fields (`ty`), from `service`.
    fn from_service(ty: Ty, service: u32) -> Self {
        Tys {
            bits: ty.bit(),
            service: Service::One(service),
        }
    }

    /// A `diag.read_dtc` result from `service`.
    fn from_dtc_service(service: u32) -> Self {
        Tys {
            bits: Ty::Response.bit(),
            service: Service::Dtcs(service),
        }
    }

    fn union(self, other: Tys) -> Tys {
        Tys {
            bits: self.bits | other.bits,
            service: self.service.union(other.service),
        }
    }

    fn has(self, ty: Ty) -> bool {
        self.bits & ty.bit() != 0
    }

    /// The one type, when there is exactly one.
    fn one(self) -> Option<Ty> {
        TYS.into_iter().find(|ty| self.bits == ty.bit())
    }

    fn describe(self) -> String {
        let names: Vec<_> = TYS
            .into_iter()
            .filter(|ty| self.has(*ty))
            .map(Ty::name)
            .collect();
        if names.is_empty() {
            "no known type".to_owned()
        } else {
            names.join(" or ")
        }
    }
}

/// A value known at ingestion. Strings, bytes, lists and dicts are shared, so naming a constant
/// does not copy it; a list or dict carries its [`Const::size`].
#[derive(Debug, Clone, PartialEq)]
enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
    List(Rc<[Const]>, Shape),
    /// String keys in source order.
    Dict(Rc<[(Rc<str>, Const)]>, Shape),
}

/// A list's or dict's [`Const::size`] and [`Const::depth`], kept with it so neither is
/// recomputed by walking it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shape {
    size: usize,
    depth: usize,
}

impl Const {
    fn str(s: impl Into<Rc<str>>) -> Const {
        Const::Str(s.into())
    }

    fn bytes(b: impl Into<Rc<[u8]>>) -> Const {
        Const::Bytes(b.into())
    }

    fn list(items: Vec<Const>) -> Const {
        let shape = items
            .iter()
            .fold(Shape { size: 1, depth: 1 }, |shape, item| Shape {
                size: shape.size.saturating_add(item.size()),
                depth: shape.depth.max(item.depth() + 1),
            });
        Const::List(items.into(), shape)
    }

    fn dict(entries: Vec<(Rc<str>, Const)>) -> Const {
        let shape = entries
            .iter()
            .fold(Shape { size: 1, depth: 1 }, |shape, (key, value)| Shape {
                size: shape
                    .size
                    .saturating_add(key.len())
                    .saturating_add(value.size()),
                depth: shape.depth.max(value.depth() + 1),
            });
        Const::Dict(entries.into(), shape)
    }

    /// The size the constant limit counts: one for each value, plus the bytes of each string,
    /// bytes value and dict key. A list that holds the same value twice counts it twice, since
    /// folding walks it twice.
    fn size(&self) -> usize {
        match self {
            Const::Int(_) | Const::Float(_) | Const::Bool(_) => 1,
            Const::Str(s) => 1 + s.len(),
            Const::Bytes(b) => 1 + b.len(),
            Const::List(_, shape) | Const::Dict(_, shape) => shape.size,
        }
    }

    /// How deeply lists and dicts nest: 0 for other values, 1 for a list of them. Comparing,
    /// searching and dropping a constant recurse this deep, so it is bounded too.
    fn depth(&self) -> usize {
        match self {
            Const::List(_, shape) | Const::Dict(_, shape) => shape.depth,
            _ => 0,
        }
    }

    fn ty(&self) -> Option<Ty> {
        match self {
            Const::Int(_) => Some(Ty::Int),
            Const::Float(_) => Some(Ty::Float),
            Const::Bool(_) => Some(Ty::Bool),
            Const::Str(_) => Some(Ty::Str),
            Const::Bytes(_) => Some(Ty::Bytes),
            Const::List(..) | Const::Dict(..) => None,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Const::List(..) => "list",
            Const::Dict(..) => "dict",
            other => other.ty().map_or("value", Ty::name),
        }
    }

    /// Starlark's truth value.
    fn truth(&self) -> bool {
        match self {
            Const::Int(n) => *n != 0,
            Const::Float(f) => *f != 0.0,
            Const::Bool(b) => *b,
            Const::Str(s) => !s.is_empty(),
            Const::Bytes(b) => !b.is_empty(),
            Const::List(l, _) => !l.is_empty(),
            Const::Dict(d, _) => !d.is_empty(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Const::Str(s) => s.len(),
            Const::Bytes(b) => b.len(),
            Const::List(l, _) => l.len(),
            Const::Dict(d, _) => d.len(),
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------- Instructions with labels

/// A jump target, resolved when the program is assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Label(u32);

/// An instruction before assembly: jumps and calls name labels and functions, not positions.
#[derive(Debug, Clone)]
enum Ins {
    Op(Op),
    Jump(Label),
    JumpIfFalse(Label),
    Call(usize),
    Mark(Label),
}

/// Lowered code: instructions with the source position each comes from.
type Code = Vec<(Ins, Span)>;

/// The result of lowering an expression.
enum Val {
    /// Known at ingestion; no code yet.
    Const(Const),
    /// Code that pushes one value of these types.
    Run(Tys, Code),
    /// Code that pushes nothing: a call to a function that returns no value.
    Void(Code),
}

// ---------------------------------------------------------------- Functions

struct Func<'a> {
    def: &'a DefP<AstNoPayload>,
    name: String,
    params: Vec<String>,
    /// Local slots by name: the parameters first, then every other name the body assigns.
    slots: HashMap<String, u16>,
    /// Slots for temporaries come after the named ones.
    temps: u16,
    /// Whether some `return` carries a value.
    returns_value: bool,
    /// Inferred types: of each slot, and of the returned value.
    slot_tys: Vec<Tys>,
    ret: Tys,
    label: Label,
    code: Code,
    /// Whether the function is part of the program: reached from `main`.
    emitted: bool,
}

struct Lowerer<'a> {
    checked: &'a Checked,
    /// The variant's declaration part, whose services `diag.request` encodes (ADR-311).
    variant: EcuVariant<'a>,
    /// The variant's services as `diag.request` selects among them (ADR-313).
    requests: &'a RequestIndex,
    /// The position of each of the variant's services by `Service.id`.
    service_at: HashMap<u32, usize>,
    /// Folded top-level constants.
    globals: HashMap<String, Const>,
    funcs: Vec<Func<'a>>,
    by_name: HashMap<String, usize>,
    errors: Vec<Pending>,
    /// Above 0 while types are inferred: nothing is reported, and code is thrown away.
    quiet: u32,
    /// Set when inference widened a type in the current pass.
    changed: bool,
    next_label: u32,
    /// The functions called while lowering a function body, whether or not its code is kept.
    called: Vec<usize>,
    /// The constant pool, and each entry's index.
    pool: Vec<Vec<u8>>,
    pooled: HashMap<Vec<u8>, u32>,
    /// Pool indexes by the address and length of a shared constant's bytes, with the constant.
    shared: HashMap<(usize, usize), (Const, u32)>,
    /// The total size of the constants built so far, against [`MAX_CONSTANTS_TOTAL`]: the
    /// globals' (`globals_built`) and those of the current pass over the function bodies, an
    /// inference pass or the emission. A pass drops its constants, so each starts again from
    /// the globals' total, and each is bounded, reported or not.
    built: usize,
    globals_built: usize,
    /// Where the total first passed the budget, in any pass. A placeholder that inference
    /// built in place of a constant could give a value the wrong type, and which pass runs
    /// out depends on the order functions are visited in, so running out anywhere, reported
    /// or not, rejects the procedure.
    exhausted: Option<(Span, &'static str)>,
    /// The work folding has done so far, against [`MAX_FOLD_WORK`]; never reset.
    work: usize,
    /// Warnings, reported like errors but not rejecting the procedure.
    warnings: Vec<Pending>,
    /// The code and place of every error and warning recorded, so each is recorded once.
    reported: HashSet<(
        StarCode,
        starlark_syntax::codemap::Pos,
        starlark_syntax::codemap::Pos,
    )>,
    /// `@maxIterations` limits by the line of their `for` (4.2).
    loop_limits: HashMap<u32, u32>,
    /// The loops being lowered, innermost last: where `break` and `continue` jump.
    loops: Vec<LoopLabels>,
    /// Loop variables bound to the constant item of an unrolled iteration, innermost last. An
    /// assignment to the variable clears its entries (`None`) rather than removing them, so the
    /// entries of the loops around keep their places. The name is shared by the iterations.
    bound: Vec<(Rc<str>, Option<Const>)>,
    /// The work of unrolling in the current pass over the function bodies, against
    /// [`MAX_UNROLLED`]: one for each iteration and each statement and expression lowered in an
    /// unrolled body, so nested loops multiply and a large body counts in full, and one for each
    /// [`NAME_BYTES_PER_UNIT`] bytes of a name an unrolled copy copies, hashes or compares.
    unrolled: usize,
    /// How many unrolled loops enclose what is being lowered.
    unrolling: u32,
    /// Whether the variable of a loop over items that exist only at ingestion is used outside
    /// the loop, by statement, so the function is searched once per loop.
    variable_escapes: HashMap<*const AstStmt, bool>,
    /// How often each name is read or updated in each function, by function, taken once.
    mention_counts: HashMap<usize, HashMap<String, usize>>,
    /// The names each loop body assigns, by body, taken once.
    assigned: HashMap<*const AstStmt, Rc<HashSet<String>>>,
}

/// Where `break` and `continue` jump in the loop being lowered.
#[derive(Clone, Copy)]
struct LoopLabels {
    end: Label,
    next: Label,
}

impl<'a> Lowerer<'a> {
    fn new(checked: &'a Checked, variant: EcuVariant<'a>, requests: &'a RequestIndex) -> Self {
        Lowerer {
            checked,
            variant,
            requests,
            service_at: variant
                .services()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(at, service)| (service.id(), at))
                .collect(),
            globals: HashMap::new(),
            funcs: Vec::new(),
            by_name: HashMap::new(),
            errors: Vec::new(),
            quiet: 0,
            changed: false,
            next_label: 0,
            called: Vec::new(),
            pool: Vec::new(),
            pooled: HashMap::new(),
            shared: HashMap::new(),
            built: 0,
            globals_built: 0,
            exhausted: None,
            work: 0,
            warnings: Vec::new(),
            reported: HashSet::new(),
            loop_limits: checked
                .annotations
                .iter()
                .filter_map(|annotation| match annotation {
                    Annotation::MaxIterations {
                        for_line, limit, ..
                    } => Some((*for_line, *limit)),
                    _ => None,
                })
                .collect(),
            loops: Vec::new(),
            bound: Vec::new(),
            unrolled: 0,
            unrolling: 0,
            variable_escapes: HashMap::new(),
            mention_counts: HashMap::new(),
            assigned: HashMap::new(),
        }
    }

    /// Records an error once per code and place: an unrolled body is lowered once per item and
    /// reports the same problem at the same place each time.
    fn error(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        if self.quiet == 0 && self.reported.insert((code, span.begin(), span.end())) {
            self.errors.push(Pending {
                code,
                span,
                hint: hint.into(),
            });
        }
    }

    /// Whether `error(code, span, ...)` would record nothing, so a caller can skip building an
    /// expensive hint.
    fn is_muted(&self, code: StarCode, span: Span) -> bool {
        self.quiet > 0 || self.reported.contains(&(code, span.begin(), span.end()))
    }

    fn not_lowerable(&mut self, span: Span, hint: impl Into<String>) {
        self.error(StarCode::NotLowerable, span, hint);
    }

    fn label(&mut self) -> Label {
        self.next_label += 1;
        Label(self.next_label)
    }

    fn top_level(&self) -> Vec<&'a AstStmt> {
        super::Checker::statements(self.checked.module.statement())
    }

    /// Rejects the annotations whose ranges are not lowered yet, so their attributes are not
    /// dropped silently (ADR-304). `@maxIterations` belongs to a `for`, which is rejected itself.
    fn annotations(&mut self) {
        let codemap = self
            .checked
            .module
            .file_span(self.checked.module.statement().span)
            .file;
        for annotation in &self.checked.annotations {
            let (line, what) = match annotation {
                Annotation::Section { line, .. } => (*line, "a `@section` annotation (4.5)"),
                Annotation::Idempotency { line, .. } => {
                    (*line, "an `@idempotency` annotation (4.6)")
                }
                Annotation::MaxIterations { .. } => continue,
            };
            let begin = codemap.line_span(line.saturating_sub(1) as usize).begin();
            self.not_lowerable(
                Span::new(begin, begin),
                format!("{what} is not lowered yet"),
            );
        }
    }

    /// Folds the top-level constants in order; each may name the ones before it.
    fn constants(&mut self) {
        for stmt in self.top_level() {
            if let StmtP::Assign(assign) = &stmt.node
                && let AssignTargetP::Identifier(name) = &assign.lhs.node
            {
                match self.expr(None, &assign.rhs) {
                    Val::Const(value) => {
                        self.globals.insert(name.node.ident.clone(), value);
                    }
                    // The checker allows no calls at the top level, so a value is constant
                    // unless an error was reported for it.
                    _ => self.not_lowerable(
                        assign.rhs.span,
                        "a top-level constant must be known at ingestion",
                    ),
                }
            }
        }
    }

    /// Numbers the top-level `def`s and gives each its slots.
    fn functions(&mut self) {
        for stmt in self.top_level() {
            let StmtP::Def(def) = &stmt.node else {
                continue;
            };
            let params: Vec<String> = def
                .params
                .iter()
                .filter_map(|p| match &p.node {
                    ParameterP::Normal(ident, ..) => Some(ident.node.ident.clone()),
                    _ => None,
                })
                .collect();
            let mut assigned = HashSet::new();
            collect_locals(&def.body, &mut assigned, &mut HashSet::new());
            // Removing the parameters from the set, rather than searching the parameters for
            // each name, keeps this linear in the number of names.
            for param in &params {
                assigned.remove(param);
            }
            let mut names: Vec<String> = params.clone();
            let mut others: Vec<String> = assigned.into_iter().collect();
            // Sorted, so the slots do not depend on hashing.
            others.sort();
            names.extend(others);
            if names.len() > usize::from(u16::MAX) {
                self.not_lowerable(def.name.span, "a function may have at most 65,535 locals");
                continue;
            }
            let slots: HashMap<String, u16> = names
                .iter()
                .enumerate()
                .map(|(at, name)| (name.clone(), at as u16))
                .collect();
            let label = self.label();
            let id = self.funcs.len();
            self.by_name.insert(def.name.node.ident.clone(), id);
            self.funcs.push(Func {
                def,
                name: def.name.node.ident.clone(),
                temps: names.len() as u16,
                slot_tys: vec![Tys::default(); names.len()],
                params,
                slots,
                returns_value: returns_value(&def.body),
                ret: Tys::default(),
                label,
                code: Vec::new(),
                emitted: false,
            });
        }
    }

    /// Infers every slot's and return value's types: passes over the functions `main` reaches,
    /// without reporting or keeping code, until no type widens and no new function is reached
    /// (ADR-304). A function nothing reaches contributes no types, since it never runs.
    fn infer(&mut self) {
        let Some(&main) = self.by_name.get("main") else {
            return;
        };
        let mut reached = vec![false; self.funcs.len()];
        reached[main] = true;
        self.quiet += 1;
        for _ in 0..MAX_TYPE_PASSES {
            self.changed = false;
            self.built = self.globals_built;
            self.unrolled = 0;
            for id in 0..self.funcs.len() {
                if !reached[id] {
                    continue;
                }
                self.called.clear();
                self.function(id);
                for callee in std::mem::take(&mut self.called) {
                    if !reached[callee] {
                        reached[callee] = true;
                        self.changed = true;
                    }
                }
            }
            // Once a budget is spent the procedure is rejected, so further passes would only
            // repeat work.
            if !self.changed || self.exhausted.is_some() {
                self.quiet -= 1;
                return;
            }
        }
        self.quiet -= 1;
        let span = self.checked.module.statement().span;
        self.not_lowerable(
            Span::new(span.begin(), span.begin()),
            format!(
                "the types of the procedure's values do not settle within {MAX_TYPE_PASSES} passes"
            ),
        );
    }

    /// Lowers `main` and every function it reaches. A function nothing calls has no argument
    /// types to lower with, and never runs, so it is left out.
    fn emit_all(&mut self) {
        let Some(&main) = self.by_name.get("main") else {
            return;
        };
        self.built = self.globals_built;
        self.unrolled = 0;
        let mut pending = vec![main];
        let mut emitted = vec![false; self.funcs.len()];
        emitted[main] = true;
        while let Some(id) = pending.pop() {
            self.called.clear();
            let code = self.function(id);
            for callee in std::mem::take(&mut self.called) {
                if !emitted[callee] {
                    emitted[callee] = true;
                    pending.push(callee);
                }
            }
            self.funcs[id].code = code;
            self.funcs[id].emitted = true;
        }
    }

    /// Lowers one function's body.
    fn function(&mut self, id: usize) -> Code {
        let def = self.funcs[id].def;
        let span = def.name.span;
        self.funcs[id].temps = self.funcs[id].slots.len() as u16;
        let mut code: Code = vec![(Ins::Mark(self.funcs[id].label), span)];
        // The caller pushed the arguments in order.
        for at in (0..self.funcs[id].params.len()).rev() {
            code.push((Ins::Op(Op::StoreLocal(at as u16)), span));
        }
        for (index, stmt) in super::Checker::statements(&def.body)
            .into_iter()
            .enumerate()
        {
            if index == 0 && is_string_expression(stmt) {
                continue;
            }
            self.stmt(id, stmt, &mut code);
        }
        // Unreachable after a function that returns on every path, but it keeps every label a
        // jump can name inside the code.
        code.push((Ins::Op(Op::Ret), span));
        if self.funcs[id].returns_value && !always_returns(&def.body) {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                format!(
                    "`{}` returns a value on some paths and not on others; a function that \
                     returns a value must return one on every path, since None is not \
                     supported (2.5)",
                    self.funcs[id].name
                ),
            );
        }
        code
    }

    // ------------------------------------------------------------ Statements

    fn stmt(&mut self, id: usize, stmt: &AstStmt, code: &mut Code) {
        if self.unrolling > 0 && !self.unroll_step(stmt.span, 1) {
            return;
        }
        match &stmt.node {
            StmtP::Pass => {}
            StmtP::Statements(list) => {
                for inner in list {
                    self.stmt(id, inner, code);
                }
            }
            StmtP::Expression(expr) => match self.expr(Some(id), expr) {
                Val::Const(_) => {}
                Val::Void(more) => code.extend(more),
                Val::Run(_, more) => {
                    code.extend(more);
                    code.push((Ins::Op(Op::Pop), expr.span));
                }
            },
            StmtP::Assign(assign) => {
                let AssignTargetP::Identifier(name) = &assign.lhs.node else {
                    self.not_lowerable(assign.lhs.span, "only a name can be assigned (2.3)");
                    return;
                };
                let value = self.expr(Some(id), &assign.rhs);
                self.store(id, &name.node.ident, value, assign.rhs.span, code);
            }
            StmtP::AssignModify(target, op, rhs) => {
                let AssignTargetP::Identifier(name) = &target.node else {
                    self.not_lowerable(target.span, "only a name can be assigned (2.3)");
                    return;
                };
                let name = name.node.ident.as_str();
                let current = self.load(id, name, target.span);
                let rhs_value = self.expr(Some(id), rhs);
                let value = self.binary(Some(id), assign_op(*op), current, rhs_value, stmt.span);
                self.store(id, name, value, stmt.span, code);
            }
            StmtP::Return(value) => {
                match value {
                    None => {
                        if self.funcs[id].returns_value {
                            self.error(
                                StarCode::UnsupportedSyntax,
                                stmt.span,
                                "a function that returns a value must return one here, since \
                                 None is not supported (2.5)",
                            );
                        }
                    }
                    Some(expr) => {
                        let value = self.expr(Some(id), expr);
                        if let Some((tys, more)) = self.force(value, expr.span) {
                            code.extend(more);
                            self.widen_return(id, tys);
                        }
                    }
                }
                code.push((Ins::Op(Op::Ret), stmt.span));
            }
            StmtP::If(cond, then) => {
                match self.condition(Some(id), cond) {
                    Some(Cond::Known(true)) => self.stmt(id, then, code),
                    Some(Cond::Known(false)) => {
                        // Lowered anyway, so its errors are reported and its types counted.
                        self.stmt(id, then, &mut Vec::new());
                    }
                    Some(Cond::Run(test)) => {
                        let end = self.label();
                        code.extend(test);
                        code.push((Ins::JumpIfFalse(end), cond.span));
                        self.stmt(id, then, code);
                        code.push((Ins::Mark(end), stmt.span));
                    }
                    None => self.stmt(id, then, &mut Vec::new()),
                }
            }
            StmtP::IfElse(cond, branches) => {
                let (then, otherwise) = &**branches;
                match self.condition(Some(id), cond) {
                    Some(Cond::Known(known)) => {
                        let (taken, skipped) = if known {
                            (then, otherwise)
                        } else {
                            (otherwise, then)
                        };
                        self.stmt(id, taken, code);
                        self.stmt(id, skipped, &mut Vec::new());
                    }
                    Some(Cond::Run(test)) => {
                        let (other, end) = (self.label(), self.label());
                        code.extend(test);
                        code.push((Ins::JumpIfFalse(other), cond.span));
                        self.stmt(id, then, code);
                        code.push((Ins::Jump(end), cond.span));
                        code.push((Ins::Mark(other), cond.span));
                        self.stmt(id, otherwise, code);
                        code.push((Ins::Mark(end), stmt.span));
                    }
                    None => {
                        self.stmt(id, then, &mut Vec::new());
                        self.stmt(id, otherwise, &mut Vec::new());
                    }
                }
            }
            StmtP::For(for_) => self.for_loop(id, stmt, for_, code),
            StmtP::Break | StmtP::Continue => match self.loops.last() {
                Some(labels) => {
                    let target = if matches!(stmt.node, StmtP::Break) {
                        labels.end
                    } else {
                        labels.next
                    };
                    code.push((Ins::Jump(target), stmt.span));
                }
                None => self.not_lowerable(stmt.span, "`break` and `continue` belong in a loop"),
            },
            StmtP::Def(_) | StmtP::Load(_) => self.not_lowerable(
                stmt.span,
                "a nested `def` or `load` cannot be lowered (2.3)",
            ),
        }
    }

    // ------------------------------------------------------------ Loops

    /// A `for` loop (subset 4.2): over a constant list or dict it is unrolled, over `range(...)`
    /// it runs, limited by `@maxIterations` when a bound is known only at run time.
    fn for_loop(&mut self, id: usize, stmt: &AstStmt, for_: &ForP<AstNoPayload>, code: &mut Code) {
        let AssignTargetP::Identifier(var) = &for_.var.node else {
            self.not_lowerable(
                for_.var.span,
                "a loop variable must be a name; unpacking is not lowered (2.3)",
            );
            return;
        };
        if !self.charge_name(&var.node.ident, 1, for_.var.span) {
            return;
        }
        let var: Rc<str> = Rc::from(var.node.ident.as_str());
        if let ExprP::Call(function, args) = &for_.over.node
            && matches!(&function.node, ExprP::Identifier(f) if f.node.ident == "range")
            && !self.funcs[id].slots.contains_key("range")
            && !self.globals.contains_key("range")
            && !self.by_name.contains_key("range")
        {
            // Each argument is looked at in every unrolled copy, and more than three are
            // refused before any is.
            if self.unrolling > 0 && !self.unroll_step(for_.over.span, args.args.len()) {
                return;
            }
            if !(1..=3).contains(&args.args.len()) {
                self.error(
                    StarCode::UnsupportedSyntax,
                    for_.over.span,
                    "`range` takes one to three arguments (2.4)",
                );
                return;
            }
            let mut positional = Vec::new();
            for arg in &args.args {
                match &arg.node {
                    ArgumentP::Positional(expr) => positional.push(expr),
                    _ => {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            arg.span,
                            "`range` takes positional arguments (2.3)",
                        );
                        return;
                    }
                }
            }
            self.range_loop(id, stmt, &var, &positional, &for_.body, code);
            return;
        }
        // Shared, not copied: an iteration takes its item when it comes to it.
        let items = match self.expr(Some(id), &for_.over) {
            Val::Const(Const::List(items, _)) => Items::List(items),
            Val::Const(Const::Dict(entries, _)) => Items::Keys(entries),
            Val::Const(other) => {
                self.not_lowerable(
                    for_.over.span,
                    format!(
                        "a {} is not iterable; a loop runs over a constant list or dict, or over \
                         `range(...)` (4.2)",
                        other.type_name()
                    ),
                );
                return;
            }
            _ => {
                self.not_lowerable(
                    for_.over.span,
                    "a loop over a value known only at run time is not lowered: lists exist only \
                     at ingestion, and bytes are not iterable, so loop over `range(...)` (4.2)",
                );
                return;
            }
        };
        self.unroll(id, stmt, &var, items, &for_.body, code);
    }

    /// Unrolls a loop over constant items: each iteration binds the variable to its item, so
    /// the body folds with it, and stores the item in the variable's slot when the item can
    /// exist at run time, so the variable holds the last item after the loop, as in Starlark.
    fn unroll(
        &mut self,
        id: usize,
        stmt: &AstStmt,
        var: &Rc<str>,
        items: Items,
        body: &AstStmt,
        code: &mut Code,
    ) {
        let end = self.label();
        self.loops.push(LoopLabels { end, next: end });
        self.unrolling += 1;
        for at in 0..items.len() {
            // The iteration may look the variable up in the body's assigned names.
            if !self.unroll_step(stmt.span, 1) || !self.charge_name(var, 1, stmt.span) {
                break;
            }
            let item = items.get(at);
            if item.ty().is_none() {
                // No slot can hold the item, so the variable has a value only inside the body.
                let escapes = self.variable_escapes(id, stmt, body, var);
                if escapes || self.assigned_in(body).contains(&**var) {
                    if self.is_muted(StarCode::NotLowerable, stmt.span) {
                        break;
                    }
                    let place = if escapes {
                        "is used outside the loop"
                    } else {
                        "is assigned in the body"
                    };
                    self.not_lowerable(
                        stmt.span,
                        format!(
                            "`{var}` {place}, but its items are {}s, which exist only at \
                             ingestion, so it has no value at run time; give the loop a \
                             variable of its own (2.5, 4.2)",
                            item.type_name()
                        ),
                    );
                    break;
                }
            }
            let next = self.label();
            if let Some(labels) = self.loops.last_mut() {
                labels.next = next;
            }
            if item.ty().is_some() {
                self.store(id, var, Val::Const(item.clone()), stmt.span, code);
            }
            let depth = self.bound.len();
            self.bound.push((Rc::clone(var), Some(item)));
            self.stmt(id, body, code);
            self.bound.truncate(depth);
            code.push((Ins::Mark(next), stmt.span));
        }
        self.unrolling -= 1;
        self.loops.pop();
        code.push((Ins::Mark(end), stmt.span));
    }

    /// Ends the constant bindings of `name`, in place.
    fn unbind(&mut self, name: &str) {
        for (bound, value) in &mut self.bound {
            if **bound == *name {
                *value = None;
            }
        }
    }

    /// Charges `units` of unrolling work; false, after an error, once the budget is spent.
    /// False as well once any budget is spent, since the procedure is rejected then: the rest
    /// of the copies would only repeat work, such as building a literal again and again.
    fn unroll_step(&mut self, span: Span, units: usize) -> bool {
        if self.exhausted.is_some() {
            return false;
        }
        self.unrolled = self.unrolled.saturating_add(units);
        if self.unrolled > MAX_UNROLLED {
            self.exhausted.get_or_insert((span, TOO_MUCH_UNROLLING));
            self.not_lowerable(span, TOO_MUCH_UNROLLING);
            return false;
        }
        true
    }

    /// Charges the unrolling work of handling `name` `times` over in an unrolled copy (copying,
    /// hashing or comparing it); true outside unrolled loops. False, after an error, once the
    /// budget is spent.
    fn charge_name(&mut self, name: &str, times: usize, span: Span) -> bool {
        let units = (name.len() / NAME_BYTES_PER_UNIT).saturating_mul(times);
        self.unrolling == 0 || units == 0 || self.unroll_step(span, units)
    }

    /// Whether function `id` reads `var`, or updates it with an augmented assignment, anywhere
    /// outside the body of the loop `stmt`: whether it is mentioned more often in the function
    /// than in the body. The function's counts are taken once, and a loop's once, so sibling
    /// loops do not each search the whole function.
    fn variable_escapes(&mut self, id: usize, stmt: &AstStmt, body: &AstStmt, var: &str) -> bool {
        let key: *const AstStmt = stmt;
        if let Some(&escapes) = self.variable_escapes.get(&key) {
            return escapes;
        }
        let total = match self.mention_counts.get(&id) {
            Some(counts) => counts.get(var).copied().unwrap_or(0),
            None => {
                let mut counts = HashMap::new();
                count_mentions(&self.funcs[id].def.body, &mut counts);
                let total = counts.get(var).copied().unwrap_or(0);
                self.mention_counts.insert(id, counts);
                total
            }
        };
        let mut inside = HashMap::new();
        count_mentions(body, &mut inside);
        let escapes = total > inside.get(var).copied().unwrap_or(0);
        self.variable_escapes.insert(key, escapes);
        escapes
    }

    /// The names `body` assigns, nested statements included, taken once per body.
    fn assigned_in(&mut self, body: &AstStmt) -> Rc<HashSet<String>> {
        let key: *const AstStmt = body;
        self.assigned
            .entry(key)
            .or_insert_with(|| {
                let mut assigned = HashSet::new();
                collect_locals(body, &mut assigned, &mut HashSet::new());
                Rc::new(assigned)
            })
            .clone()
    }

    /// A loop over `range(start, stop, step)`: the step must be a constant other than 0. When
    /// a bound is known only at run time, or the loop has `@maxIterations`, a counter limits
    /// it, and an iteration past the limit ends the procedure with `Fail`
    /// `LOOP_LIMIT_EXCEEDED`.
    fn range_loop(
        &mut self,
        id: usize,
        stmt: &AstStmt,
        var: &str,
        args: &[&AstExpr],
        body: &AstStmt,
        code: &mut Code,
    ) {
        let span = stmt.span;
        let values: Vec<Val> = args.iter().map(|arg| self.expr(Some(id), arg)).collect();
        let (start, stop, step) = match <[Val; 3]>::try_from(match values.len() {
            1 => {
                let mut v = values;
                v.insert(0, Val::Const(Const::Int(0)));
                v.push(Val::Const(Const::Int(1)));
                v
            }
            2 => {
                let mut v = values;
                v.push(Val::Const(Const::Int(1)));
                v
            }
            _ => values,
        }) {
            Ok([start, stop, step]) => (start, stop, step),
            Err(_) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    span,
                    "`range` takes one to three arguments (2.4)",
                );
                return;
            }
        };
        let step = match step {
            Val::Const(Const::Int(0)) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    span,
                    "the step of `range` must not be 0 (2.4)",
                );
                return;
            }
            Val::Const(Const::Int(step)) => step,
            Val::Const(other) => {
                self.not_lowerable(
                    span,
                    format!("the step of `range` is an int, not a {}", other.type_name()),
                );
                return;
            }
            _ => {
                self.not_lowerable(
                    span,
                    "a `range` step known only at run time is not lowered: the loop's \
                     comparison depends on its sign (4.2)",
                );
                return;
            }
        };
        let constant_bounds = matches!(
            (&start, &stop),
            (Val::Const(Const::Int(_)), Val::Const(Const::Int(_)))
        );
        let line = self.line_of(span);
        let annotated = self.loop_limits.get(&line).copied();
        let limit = match annotated {
            Some(limit) => Some(limit),
            None if constant_bounds => None,
            None => {
                self.warn(
                    StarCode::LoopLimitDefaulted,
                    span,
                    format!(
                        "a bound of this `range` is known only at run time; specify \
                         @maxIterations (default {DEFAULT_MAX_ITERATIONS} applied) (4.2)"
                    ),
                );
                Some(DEFAULT_MAX_ITERATIONS)
            }
        };
        let (Some((ts, start_code)), Some((tt, stop_code))) = (
            self.single(start, span, "`range`"),
            self.single(stop, span, "`range`"),
        ) else {
            return;
        };
        if (ts, tt) != (Ty::Int, Ty::Int) {
            self.not_lowerable(span, "the bounds of `range` are ints (2.4)");
            return;
        }
        let Some(&slot) = self.funcs[id].slots.get(var) else {
            self.not_lowerable(span, format!("`{var}` is not a local of this function"));
            return;
        };
        // The variable now holds a run-time value, not an enclosing unrolled loop's constant.
        self.unbind(var);
        self.widen_slot(id, slot, Tys::of(Ty::Int));
        let (i, stop_slot) = (self.temp(id, span), self.temp(id, span));
        let counter = limit.map(|_| self.temp(id, span));
        let (head, next, end, over) = (self.label(), self.label(), self.label(), self.label());
        let at = |ins: Ins| (ins, span);
        code.extend(start_code);
        code.push(at(Ins::Op(Op::StoreLocal(i))));
        code.extend(stop_code);
        code.push(at(Ins::Op(Op::StoreLocal(stop_slot))));
        if let Some(c) = counter {
            code.extend([Ins::Op(Op::PushI64(0)), Ins::Op(Op::StoreLocal(c))].map(at));
        }
        code.extend(
            [
                Ins::Mark(head),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::LoadLocal(stop_slot)),
                Ins::Op(if step > 0 { Op::CmpLt } else { Op::CmpGt }),
                Ins::JumpIfFalse(end),
            ]
            .map(at),
        );
        if let (Some(c), Some(limit)) = (counter, limit) {
            code.extend(
                [
                    Ins::Op(Op::LoadLocal(c)),
                    Ins::Op(Op::PushI64(i64::from(limit))),
                    Ins::Op(Op::CmpLt),
                    Ins::JumpIfFalse(over),
                    Ins::Op(Op::LoadLocal(c)),
                    Ins::Op(Op::PushI64(1)),
                    Ins::Op(Op::Add),
                    Ins::Op(Op::StoreLocal(c)),
                ]
                .map(at),
            );
        }
        code.extend([Ins::Op(Op::LoadLocal(i)), Ins::Op(Op::StoreLocal(slot))].map(at));
        // The body runs again after it assigns a name, so a name it assigns must not fold to an
        // enclosing unrolled loop's constant anywhere in it, even before the assignment.
        // The enclosing bindings are few (one per enclosing unrolled loop), so they are checked
        // against the assigned names, not the other way round.
        let assigned = self.assigned_in(body);
        let names: usize = self.bound.iter().map(|(name, _)| name.len()).sum();
        if names >= NAME_BYTES_PER_UNIT && self.unrolling > 0 {
            let units = names / NAME_BYTES_PER_UNIT;
            if !self.unroll_step(span, units) {
                return;
            }
        }
        for (name, value) in &mut self.bound {
            if assigned.contains(&**name) {
                *value = None;
            }
        }
        self.loops.push(LoopLabels { end, next });
        self.stmt(id, body, code);
        self.loops.pop();
        // A value one step on would leave the int range only when it is past every stop
        // anyway, so the loop ends there instead of overflowing.
        let (edge, past) = if step > 0 {
            (i64::MAX - step, Op::CmpGt)
        } else {
            (i64::MIN - step, Op::CmpLt)
        };
        code.extend(
            [
                Ins::Mark(next),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::PushI64(edge)),
                Ins::Op(past),
                Ins::Op(Op::Not),
                Ins::JumpIfFalse(end),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::PushI64(step)),
                Ins::Op(Op::Add),
                Ins::Op(Op::StoreLocal(i)),
                Ins::Jump(head),
            ]
            .map(at),
        );
        if let Some(limit) = limit {
            let code_index = self.pool(LOOP_LIMIT_EXCEEDED.as_bytes());
            let detail = self.pool(
                format!("the loop on line {line} ran past its limit of {limit} iterations")
                    .as_bytes(),
            );
            code.extend(
                [
                    Ins::Mark(over),
                    Ins::Op(Op::PushBytes(detail)),
                    Ins::Op(Op::Fail { code: code_index }),
                ]
                .map(at),
            );
        }
        code.push(at(Ins::Mark(end)));
    }

    /// The 1-based line `span` starts on.
    fn line_of(&self, span: Span) -> u32 {
        let codemap = self
            .checked
            .module
            .file_span(self.checked.module.statement().span)
            .file;
        to_u32(codemap.find_line(span.begin()) + 1)
    }

    fn warn(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        if self.quiet == 0 && self.reported.insert((code, span.begin(), span.end())) {
            self.warnings.push(Pending {
                code,
                span,
                hint: hint.into(),
            });
        }
    }

    /// Stores `value` in the local `name`.
    fn store(&mut self, id: usize, name: &str, value: Val, span: Span, code: &mut Code) {
        // An assignment to a loop variable bound to a constant ends the binding; the slot
        // holds the new value (an unrolled iteration stores its item in the slot as well).
        // Clearing the bindings compares the name with each; the slot lookup hashes it.
        if !self.charge_name(name, self.bound.len() + 1, span) {
            return;
        }
        self.unbind(name);
        let Some(&slot) = self.funcs[id].slots.get(name) else {
            self.not_lowerable(span, format!("`{name}` is not a local of this function"));
            return;
        };
        if let Some((tys, more)) = self.force(value, span) {
            code.extend(more);
            code.push((Ins::Op(Op::StoreLocal(slot)), span));
            self.widen_slot(id, slot, tys);
        }
    }

    fn widen_slot(&mut self, id: usize, slot: u16, tys: Tys) {
        let current = &mut self.funcs[id].slot_tys[usize::from(slot)];
        let widened = current.union(tys);
        if widened != *current {
            *current = widened;
            self.changed = true;
        }
    }

    fn widen_return(&mut self, id: usize, tys: Tys) {
        let widened = self.funcs[id].ret.union(tys);
        if widened != self.funcs[id].ret {
            self.funcs[id].ret = widened;
            self.changed = true;
        }
    }

    // ------------------------------------------------------------ Expressions

    /// Lowers an expression in function `id`, or at the top level for `None`.
    fn expr(&mut self, id: Option<usize>, expr: &AstExpr) -> Val {
        let span = expr.span;
        if self.unrolling > 0 && !self.unroll_step(span, 1) {
            return Val::Run(Tys::default(), Vec::new());
        }
        match &expr.node {
            ExprP::Literal(literal) => match literal {
                AstLiteral::Int(int) => match &int.node {
                    TokenInt::I32(n) => Val::Const(Const::Int(i64::from(*n))),
                    TokenInt::BigInt(big) => match i64::try_from(big) {
                        Ok(n) => Val::Const(Const::Int(n)),
                        Err(_) => {
                            self.not_lowerable(span, "an integer outside the 64-bit range (4.4)");
                            Val::Const(Const::Int(0))
                        }
                    },
                },
                AstLiteral::Float(f) => Val::Const(Const::Float(f.node)),
                AstLiteral::String(s) => self.bounded(Const::str(s.node.as_str()), span),
                AstLiteral::Bytes(b) => self.bounded(Const::bytes(b.node.as_slice()), span),
                AstLiteral::Ellipsis => {
                    self.not_lowerable(span, "`...` cannot be lowered (2.3)");
                    Val::Const(Const::Int(0))
                }
            },
            ExprP::Identifier(ident) => self.name(id, &ident.node.ident, span),
            ExprP::Not(inner) => {
                let value = self.expr(id, inner);
                self.not(value, span)
            }
            // -2^63 is in range though its magnitude is not: negate the literal as a whole.
            ExprP::Minus(inner) if is_two_to_the_63(inner) => Val::Const(Const::Int(i64::MIN)),
            ExprP::Minus(inner) => {
                let value = self.expr(id, inner);
                self.unary(Unary::Minus, value, span)
            }
            ExprP::Plus(inner) => {
                let value = self.expr(id, inner);
                self.unary(Unary::Plus, value, span)
            }
            ExprP::BitNot(inner) => {
                let value = self.expr(id, inner);
                self.unary(Unary::BitNot, value, span)
            }
            ExprP::Op(lhs, BinOp::And, rhs) => self.and_or(id, true, lhs, rhs, span),
            ExprP::Op(lhs, BinOp::Or, rhs) => self.and_or(id, false, lhs, rhs, span),
            ExprP::Op(lhs, op, rhs) => {
                let a = self.expr(id, lhs);
                let b = self.expr(id, rhs);
                self.binary(id, *op, a, b, span)
            }
            ExprP::If(parts) => {
                let (cond, then, otherwise) = &**parts;
                match self.condition(id, cond) {
                    Some(Cond::Known(known)) => {
                        let (taken, skipped) = if known {
                            (then, otherwise)
                        } else {
                            (otherwise, then)
                        };
                        // The other branch is lowered for its errors and types only.
                        self.quiet += 1;
                        self.expr(id, skipped);
                        self.quiet -= 1;
                        self.expr(id, taken)
                    }
                    Some(Cond::Run(test)) => {
                        let a = self.expr(id, then);
                        let b = self.expr(id, otherwise);
                        let (Some((ta, ca)), Some((tb, cb))) =
                            (self.force(a, then.span), self.force(b, otherwise.span))
                        else {
                            return Val::Run(Tys::default(), Vec::new());
                        };
                        let (other, end) = (self.label(), self.label());
                        let mut code = test;
                        code.push((Ins::JumpIfFalse(other), cond.span));
                        code.extend(ca);
                        code.push((Ins::Jump(end), span));
                        code.push((Ins::Mark(other), span));
                        code.extend(cb);
                        code.push((Ins::Mark(end), span));
                        Val::Run(ta.union(tb), code)
                    }
                    None => Val::Run(Tys::default(), Vec::new()),
                }
            }
            ExprP::List(items) => {
                let mut values = Vec::new();
                for item in items {
                    match self.expr(id, item) {
                        Val::Const(value) => values.push(value),
                        _ => {
                            self.not_lowerable(
                                item.span,
                                "a list exists only at ingestion, so its items must be constants (2.5)",
                            );
                            return Val::Const(Const::list(Vec::new()));
                        }
                    }
                }
                self.bounded(Const::list(values), span)
            }
            ExprP::Dict(pairs) => {
                let mut entries = Vec::new();
                let mut keys = HashSet::new();
                for (key, value) in pairs {
                    let key = match self.expr(id, key) {
                        Val::Const(Const::Str(key)) => key,
                        _ => {
                            self.not_lowerable(
                                key.span,
                                "a dict key must be a string literal (4.1)",
                            );
                            continue;
                        }
                    };
                    if !keys.insert(key.clone()) {
                        self.not_lowerable(
                            span,
                            format!("the dict repeats the key {key:?}, which Starlark rejects"),
                        );
                        continue;
                    }
                    match self.expr(id, value) {
                        Val::Const(value) => entries.push((key, value)),
                        _ => self.not_lowerable(
                            value.span,
                            "a dict exists only at ingestion, so its values must be constants (2.5)",
                        ),
                    }
                }
                self.bounded(Const::dict(entries), span)
            }
            ExprP::Index(parts) => {
                let (object, index) = &**parts;
                let a = self.expr(id, object);
                let b = self.expr(id, index);
                match a {
                    Val::Run(tys, code) if matches!(tys.one(), Some(Ty::Response | Ty::Fields)) => {
                        // Subset 4.1 holds for a response however it reached here, also through
                        // a function the checker does not follow.
                        let literal = matches!(&index.node, ExprP::Literal(AstLiteral::String(_)));
                        let key = if literal {
                            b
                        } else {
                            Val::Run(Tys::default(), Vec::new())
                        };
                        if tys.one() == Some(Ty::Response) {
                            self.response_key(id, code, key, tys.service, span)
                        } else {
                            self.field_key(code, key, tys.service, span)
                        }
                    }
                    a => self.index(a, b, span),
                }
            }
            ExprP::Slice(object, start, stop, step) => {
                let object = self.expr(id, object);
                let start = start.as_ref().map(|expr| self.expr(id, expr));
                let stop = stop.as_ref().map(|expr| self.expr(id, expr));
                let known = |bound: &Option<Val>| match bound {
                    None => Some(None),
                    Some(Val::Const(Const::Int(n))) => Some(Some(*n)),
                    Some(_) => None,
                };
                if step.is_some() {
                    self.not_lowerable(span, "a slice with a step is not lowered (2.5)");
                    return Val::Run(Tys::default(), Vec::new());
                }
                match (object, known(&start), known(&stop)) {
                    (Val::Const(object), Some(from), Some(to)) => {
                        self.slice(object, from, to, span)
                    }
                    (object, ..) => self.run_slice(object, start, stop, span),
                }
            }
            ExprP::Call(function, args) => self.call(id, function, &args.args, span),
            ExprP::Dot(..) => {
                self.not_lowerable(span, "an attribute is lowered only as a method call");
                Val::Run(Tys::default(), Vec::new())
            }
            _ => {
                self.not_lowerable(span, "this expression cannot be lowered (2.3)");
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    fn name(&mut self, id: Option<usize>, name: &str, span: Span) -> Val {
        // The bindings are compared with the name, then a slot and a global looked up by it.
        if !self.charge_name(name, self.bound.len() + 2, span) {
            return Val::Run(Tys::default(), Vec::new());
        }
        if id.is_some()
            && let Some((_, Some(value))) =
                self.bound.iter().rev().find(|(bound, _)| **bound == *name)
        {
            return Val::Const(value.clone());
        }
        if let Some(id) = id
            && self.funcs[id].slots.contains_key(name)
        {
            return self.load(id, name, span);
        }
        if let Some(value) = self.globals.get(name) {
            return Val::Const(value.clone());
        }
        match name {
            "True" => Val::Const(Const::Bool(true)),
            "False" => Val::Const(Const::Bool(false)),
            _ => {
                self.not_lowerable(span, format!("`{name}` is not a value that can be lowered"));
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    fn load(&mut self, id: usize, name: &str, span: Span) -> Val {
        if !self.charge_name(name, 1, span) {
            return Val::Run(Tys::default(), Vec::new());
        }
        match self.funcs[id].slots.get(name) {
            Some(&slot) => Val::Run(
                self.funcs[id].slot_tys[usize::from(slot)],
                vec![(Ins::Op(Op::LoadLocal(slot)), span)],
            ),
            None => {
                self.not_lowerable(span, format!("`{name}` is not a local of this function"));
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    /// Code that pushes `value`, with its types; `None` (after an error) when it cannot exist at
    /// run time.
    fn force(&mut self, value: Val, span: Span) -> Option<(Tys, Code)> {
        match value {
            Val::Run(tys, code) => Some((tys, code)),
            Val::Void(_) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    span,
                    "this call returns no value, and None is not supported (2.5)",
                );
                None
            }
            Val::Const(value) => {
                let Some(ty) = value.ty() else {
                    self.not_lowerable(
                        span,
                        format!(
                            "a {} exists only at ingestion; it cannot be a value at run time (2.5)",
                            value.type_name()
                        ),
                    );
                    return None;
                };
                let code = self.push_const(&value, span);
                Some((Tys::of(ty), code))
            }
        }
    }

    fn push_const(&mut self, value: &Const, span: Span) -> Code {
        let ops = match value {
            Const::Int(n) => vec![Op::PushI64(*n)],
            Const::Float(f) => vec![Op::PushF64(*f)],
            // There is no boolean push: a comparison makes one.
            Const::Bool(b) => vec![
                Op::PushI64(0),
                Op::PushI64(if *b { 0 } else { 1 }),
                Op::CmpEq,
            ],
            Const::Str(_) | Const::Bytes(_) => vec![Op::PushBytes(self.pool_shared(value))],
            Const::List(..) | Const::Dict(..) => Vec::new(),
        };
        ops.into_iter().map(|op| (Ins::Op(op), span)).collect()
    }

    /// The pool index of a string or bytes constant. A constant pushed again (a global named in
    /// many places, in every inference pass) is found by its shared allocation, without hashing
    /// its bytes again; the map keeps the allocation alive, so its address is not reused.
    fn pool_shared(&mut self, value: &Const) -> u32 {
        if self.quiet > 0 {
            return 0;
        }
        let bytes: &[u8] = match value {
            Const::Str(s) => s.as_bytes(),
            Const::Bytes(b) => b,
            _ => return 0,
        };
        let key = (bytes.as_ptr() as usize, bytes.len());
        if let Some((_, index)) = self.shared.get(&key) {
            return *index;
        }
        let index = self.pool(bytes);
        self.shared.insert(key, (value.clone(), index));
        index
    }

    fn pool(&mut self, bytes: &[u8]) -> u32 {
        // Code made while types are inferred is thrown away, so it needs no pool entry.
        if self.quiet > 0 {
            return 0;
        }
        if let Some(&index) = self.pooled.get(bytes) {
            return index;
        }
        let index = to_u32(self.pool.len());
        self.pool.push(bytes.to_vec());
        self.pooled.insert(bytes.to_vec(), index);
        index
    }

    /// A value that must have exactly one type at run time.
    fn single(&mut self, value: Val, span: Span, what: &str) -> Option<(Ty, Code)> {
        let (tys, code) = self.force(value, span)?;
        match tys.one() {
            Some(ty) => Some((ty, code)),
            None => {
                self.not_lowerable(
                    span,
                    format!(
                        "{what} needs one type known at ingestion, but this value can be {} (ADR-304)",
                        tys.describe()
                    ),
                );
                None
            }
        }
    }

    // ------------------------------------------------------------ Truth and logic

    /// A condition: known at ingestion, or code that pushes a `Bool`.
    fn condition(&mut self, id: Option<usize>, expr: &AstExpr) -> Option<Cond> {
        let value = self.expr(id, expr);
        self.truth(value, expr.span)
    }

    fn truth(&mut self, value: Val, span: Span) -> Option<Cond> {
        if let Val::Const(value) = &value {
            return Some(Cond::Known(value.truth()));
        }
        let (ty, mut code) = self.single(value, span, "a truth value")?;
        code.extend(self.truth_of(ty, span)?);
        Some(Cond::Run(code))
    }

    /// Code that turns the value of type `ty` on top of the stack into its truth value.
    fn truth_of(&mut self, ty: Ty, span: Span) -> Option<Code> {
        let ops = match ty {
            Ty::Bool => Vec::new(),
            Ty::Int => vec![Op::PushI64(0), Op::CmpEq, Op::Not],
            Ty::Float => vec![Op::PushF64(0.0), Op::CmpEq, Op::Not],
            Ty::Bytes | Ty::Str => {
                self.not_lowerable(
                    span,
                    "the truth of bytes or a string known at run time is not lowered yet (2.5)",
                );
                return None;
            }
            Ty::Response | Ty::Fields => {
                self.not_lowerable(
                    span,
                    "a response is always true; test `res[\"ok\"]` instead (3.4)",
                );
                return None;
            }
        };
        Some(ops.into_iter().map(|op| (Ins::Op(op), span)).collect())
    }

    fn not(&mut self, value: Val, span: Span) -> Val {
        match self.truth(value, span) {
            Some(Cond::Known(b)) => Val::Const(Const::Bool(!b)),
            Some(Cond::Run(mut code)) => {
                code.push((Ins::Op(Op::Not), span));
                Val::Run(Tys::of(Ty::Bool), code)
            }
            None => Val::Run(Tys::of(Ty::Bool), Vec::new()),
        }
    }

    /// `a and b` (`and_` true) or `a or b`: the value of `a` when it decides, else of `b`.
    fn and_or(
        &mut self,
        id: Option<usize>,
        and_: bool,
        lhs: &AstExpr,
        rhs: &AstExpr,
        span: Span,
    ) -> Val {
        let a = self.expr(id, lhs);
        if let Val::Const(value) = &a {
            // `a` decides when it is false for `and`, true for `or`.
            if value.truth() != and_ {
                self.quiet += 1;
                self.expr(id, rhs);
                self.quiet -= 1;
                return a;
            }
            return self.expr(id, rhs);
        }
        let b = self.expr(id, rhs);
        let (Some((ta, ca)), Some((tb, cb))) = (
            self.single(a, lhs.span, "`and` or `or`"),
            self.force(b, rhs.span),
        ) else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let Some(test) = self.truth_of(ta, lhs.span) else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let end = self.label();
        let mut code = ca;
        code.push((Ins::Op(Op::Dup), span));
        code.extend(test);
        if !and_ {
            code.push((Ins::Op(Op::Not), span));
        }
        // When `a` decides, its copy stays as the result.
        code.push((Ins::JumpIfFalse(end), span));
        code.push((Ins::Op(Op::Pop), span));
        code.extend(cb);
        code.push((Ins::Mark(end), span));
        Val::Run(Tys::of(ta).union(tb), code)
    }

    // ------------------------------------------------------------ Operators

    fn unary(&mut self, op: Unary, value: Val, span: Span) -> Val {
        if let Val::Const(value) = value {
            return match (op, value) {
                (Unary::Plus, v @ (Const::Int(_) | Const::Float(_))) => Val::Const(v),
                (Unary::Minus, Const::Int(n)) => match n.checked_neg() {
                    Some(n) => Val::Const(Const::Int(n)),
                    None => self.out_of_range(span),
                },
                (Unary::Minus, Const::Float(f)) => Val::Const(Const::Float(-f)),
                (Unary::BitNot, Const::Int(n)) => Val::Const(Const::Int(!n)),
                (_, value) => {
                    self.not_lowerable(
                        span,
                        format!("{} does not apply to a {}", op.symbol(), value.type_name()),
                    );
                    Val::Run(Tys::default(), Vec::new())
                }
            };
        }
        let Some((ty, mut code)) = self.single(value, span, op.symbol()) else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let ops = match (op, ty) {
            (Unary::Plus, Ty::Int | Ty::Float) => Vec::new(),
            // 0 - x, so the most negative integer fails as overflow at run time.
            (Unary::Minus, Ty::Int) => {
                let mut negated = vec![(Ins::Op(Op::PushI64(0)), span)];
                negated.extend(code);
                negated.push((Ins::Op(Op::Sub), span));
                return Val::Run(Tys::of(Ty::Int), negated);
            }
            // Multiplying by -1 keeps the sign of zero, as negation does.
            (Unary::Minus, Ty::Float) => vec![Op::PushF64(-1.0), Op::Mul],
            (Unary::BitNot, Ty::Int) => vec![Op::PushI64(-1), Op::BitXor],
            _ => {
                self.not_lowerable(
                    span,
                    format!("{} does not apply to a {}", op.symbol(), ty.name()),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
        };
        code.extend(ops.into_iter().map(|op| (Ins::Op(op), span)));
        Val::Run(Tys::of(ty), code)
    }

    /// `value`, or a placeholder after an error when it is larger than the constant limit, so a
    /// later fold does not build on it.
    fn bounded(&mut self, value: Const, span: Span) -> Val {
        if value.size() > MAX_CONSTANT_SIZE {
            self.not_lowerable(
                span,
                "the constant is too large: more than 1 Mi values and bytes (4.4)",
            );
            return Val::Const(Const::Int(0));
        }
        if value.depth() > MAX_CONSTANT_DEPTH {
            self.not_lowerable(
                span,
                "lists and dicts nest more than 64 deep in this constant (4.4)",
            );
            return Val::Const(Const::Int(0));
        }
        if matches!(
            value,
            Const::Str(_) | Const::Bytes(_) | Const::List(..) | Const::Dict(..)
        ) {
            self.built = self.built.saturating_add(value.size());
            if self.built > MAX_CONSTANTS_TOTAL {
                self.exhausted.get_or_insert((span, TOO_MANY_CONSTANTS));
                self.not_lowerable(span, TOO_MANY_CONSTANTS);
                return Val::Const(Const::Int(0));
            }
        }
        Val::Const(value)
    }

    /// Reports running out of the constant or folding budget when nothing reported it,
    /// because it happened only where reports are muted (in inference, or in an operand that is
    /// never evaluated).
    fn reject_if_exhausted(&mut self) {
        if let Some((span, hint)) = self.exhausted
            && self.errors.is_empty()
        {
            self.errors.push(Pending {
                code: StarCode::NotLowerable,
                span,
                hint: hint.to_owned(),
            });
        }
    }

    /// Charges a fold that reads `cost` values and bytes; false, after an error, once the
    /// folding budget is spent, and the fold is then not done.
    fn charge(&mut self, cost: usize, span: Span) -> bool {
        self.work = self.work.saturating_add(cost);
        if self.work > MAX_FOLD_WORK {
            self.exhausted.get_or_insert((span, TOO_MUCH_FOLDING));
            self.not_lowerable(span, TOO_MUCH_FOLDING);
            return false;
        }
        true
    }

    fn out_of_range(&mut self, span: Span) -> Val {
        self.not_lowerable(span, "the result is outside the 64-bit integer range (4.4)");
        Val::Const(Const::Int(0))
    }

    fn binary(&mut self, id: Option<usize>, op: BinOp, a: Val, b: Val, span: Span) -> Val {
        if let (Val::Const(x), Val::Const(y)) = (&a, &b) {
            // A list search compares the needle with every item, so it reads the needle once
            // per item.
            let cost = match (op, y) {
                (BinOp::In | BinOp::NotIn, Const::List(items, _)) => {
                    x.size().saturating_mul(items.len())
                }
                _ => x.size(),
            };
            if !self.charge(cost.saturating_add(y.size()), span) {
                return Val::Const(Const::Int(0));
            }
            return match fold(op, x, y) {
                Ok(value) => self.bounded(value, span),
                Err(reason) => {
                    self.not_lowerable(span, reason);
                    Val::Const(Const::Int(0))
                }
            };
        }
        if [&a, &b]
            .iter()
            .any(|v| matches!(v, Val::Run(tys, _) if tys.has(Ty::Response) || tys.has(Ty::Fields)))
        {
            // Starlark would compare the dict, which a procedure that left out `["raw"]` gets
            // wrong silently (ADR-317 item 3).
            self.not_lowerable(
                span,
                format!(
                    "`{}` does not take a response: read it through its keys, such as \
                     `res[\"raw\"]` (3.4)",
                    binop_symbol(op)
                ),
            );
            return Val::Run(Tys::default(), Vec::new());
        }
        let _ = id;
        let what = binop_symbol(op);
        let (Some((ta, ca)), Some((tb, cb))) =
            (self.single(a, span, what), self.single(b, span, what))
        else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let mut code = ca;
        code.extend(cb);
        let at = |op: Op| (Ins::Op(op), span);
        let result = match (op, ta, tb) {
            // Starlark computes an int with a float, and `/` of two ints, in floats (4.4).
            (
                BinOp::Add | BinOp::Subtract | BinOp::Multiply | BinOp::Divide,
                Ty::Int | Ty::Float,
                Ty::Int | Ty::Float,
            ) if ta != tb || op == BinOp::Divide => {
                if ta == Ty::Int {
                    code.extend([Op::Swap, Op::IntToFloat, Op::Swap].map(at));
                }
                if tb == Ty::Int {
                    code.push(at(Op::IntToFloat));
                }
                if op == BinOp::Divide {
                    code.extend(self.float_divide(span));
                } else {
                    code.push(at(match op {
                        BinOp::Add => Op::Add,
                        BinOp::Subtract => Op::Sub,
                        _ => Op::Mul,
                    }));
                }
                Ty::Float
            }
            // A run-time string is UTF-8 `Bytes` (2.5), and joining two keeps it UTF-8.
            (BinOp::Add, Ty::Bytes, Ty::Bytes) | (BinOp::Add, Ty::Str, Ty::Str) => {
                code.push(at(Op::Concat));
                ta
            }
            (BinOp::Add | BinOp::Subtract | BinOp::Multiply, Ty::Int, Ty::Int)
            | (BinOp::Add | BinOp::Subtract | BinOp::Multiply, Ty::Float, Ty::Float) => {
                code.push(at(match op {
                    BinOp::Add => Op::Add,
                    BinOp::Subtract => Op::Sub,
                    _ => Op::Mul,
                }));
                ta
            }
            (BinOp::FloorDivide | BinOp::Percent, Ty::Int, Ty::Int) => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                code.extend(self.floor(id, op == BinOp::Percent, span));
                Ty::Int
            }
            (BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, Ty::Int, Ty::Int) => {
                code.push(at(match op {
                    BinOp::BitAnd => Op::BitAnd,
                    BinOp::BitOr => Op::BitOr,
                    _ => Op::BitXor,
                }));
                Ty::Int
            }
            (BinOp::RightShift, Ty::Int, Ty::Int) => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                code.extend(self.shift_right(id, span));
                Ty::Int
            }
            (BinOp::Equal | BinOp::NotEqual, Ty::Float, Ty::Float) => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                code.extend(self.float_compare(id, BinOp::Equal, span));
                if op == BinOp::NotEqual {
                    code.push(at(Op::Not));
                }
                Ty::Bool
            }
            (BinOp::Equal | BinOp::NotEqual, x, y) if x == y => {
                code.push(at(Op::CmpEq));
                if op == BinOp::NotEqual {
                    code.push(at(Op::Not));
                }
                Ty::Bool
            }
            (BinOp::Equal | BinOp::NotEqual, Ty::Int, Ty::Float)
            | (BinOp::Equal | BinOp::NotEqual, Ty::Float, Ty::Int) => {
                self.not_lowerable(
                    span,
                    "comparing an int with a float known at run time is not lowered: converting \
                     the int to a float could round it and change the answer (4.4)",
                );
                return Val::Run(Tys::of(Ty::Bool), Vec::new());
            }
            // Values of different types are never equal in Starlark.
            (BinOp::Equal | BinOp::NotEqual, _, _) => {
                code.push(at(Op::Pop));
                code.push(at(Op::Pop));
                code.extend(self.push_const(&Const::Bool(op == BinOp::NotEqual), span));
                Ty::Bool
            }
            (BinOp::Less | BinOp::Greater, Ty::Int, Ty::Int) => {
                code.push(at(if op == BinOp::Less {
                    Op::CmpLt
                } else {
                    Op::CmpGt
                }));
                Ty::Bool
            }
            (BinOp::LessOrEqual | BinOp::GreaterOrEqual, Ty::Int, Ty::Int) => {
                // Integers are totally ordered, so a <= b is not (a > b).
                code.push(at(if op == BinOp::LessOrEqual {
                    Op::CmpGt
                } else {
                    Op::CmpLt
                }));
                code.push(at(Op::Not));
                Ty::Bool
            }
            (
                BinOp::Less | BinOp::Greater | BinOp::LessOrEqual | BinOp::GreaterOrEqual,
                Ty::Float,
                Ty::Float,
            ) => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                // Floats are totally ordered in Starlark (NaN is the greatest), so a <= b is
                // not (a > b), as for integers.
                let (strict, negate) = match op {
                    BinOp::Less => (BinOp::Less, false),
                    BinOp::Greater => (BinOp::Greater, false),
                    BinOp::LessOrEqual => (BinOp::Greater, true),
                    _ => (BinOp::Less, true),
                };
                code.extend(self.float_compare(id, strict, span));
                if negate {
                    code.push(at(Op::Not));
                }
                Ty::Bool
            }
            _ => {
                self.not_lowerable(span, binary_hint(op, ta, tb));
                return Val::Run(Tys::default(), Vec::new());
            }
        };
        Val::Run(Tys::of(result), code)
    }

    /// A fresh temporary slot in function `id`.
    fn temp(&mut self, id: usize, span: Span) -> u16 {
        let func = &mut self.funcs[id];
        if func.temps == u16::MAX {
            self.not_lowerable(
                span,
                "a function may have at most 65,535 locals, and each unrolled copy of a body \
                 takes temporaries of its own",
            );
            return 0;
        }
        let slot = func.temps;
        func.temps += 1;
        if usize::from(slot) >= func.slot_tys.len() {
            func.slot_tys.push(Tys::of(Ty::Int));
        }
        slot
    }

    /// Floor division (`percent` false) or its remainder (subset 4.4): the IR's division
    /// truncates, so the quotient is lowered by one, and the remainder raised by the divisor,
    /// when the remainder is not zero and its sign differs from the divisor's. The operands are
    /// on the stack, the dividend below.
    fn floor(&mut self, id: usize, percent: bool, span: Span) -> Code {
        let (a, b, q, r) = (
            self.temp(id, span),
            self.temp(id, span),
            self.temp(id, span),
            self.temp(id, span),
        );
        let (done, minus_one) = (self.label(), self.label());
        let mut ops = vec![Ins::Op(Op::StoreLocal(b)), Ins::Op(Op::StoreLocal(a))];
        if percent {
            // x % -1 is 0 for every x, while the division overflows for the smallest integer.
            ops.extend([
                Ins::Op(Op::LoadLocal(b)),
                Ins::Op(Op::PushI64(-1)),
                Ins::Op(Op::CmpEq),
                Ins::Op(Op::Not),
                Ins::JumpIfFalse(minus_one),
            ]);
        }
        ops.extend([
            // q = a / b, truncated; r = a - q * b.
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::Div),
            Ins::Op(Op::StoreLocal(q)),
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(q)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::Mul),
            Ins::Op(Op::Sub),
            Ins::Op(Op::StoreLocal(r)),
            // r != 0
            Ins::Op(Op::LoadLocal(r)),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::CmpEq),
            Ins::Op(Op::Not),
            Ins::JumpIfFalse(done),
            // (r < 0) != (b < 0)
            Ins::Op(Op::LoadLocal(r)),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::CmpLt),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::CmpLt),
            Ins::Op(Op::CmpEq),
            Ins::Op(Op::Not),
            Ins::JumpIfFalse(done),
        ]);
        if percent {
            ops.extend([
                Ins::Op(Op::LoadLocal(r)),
                Ins::Op(Op::LoadLocal(b)),
                Ins::Op(Op::Add),
                Ins::Op(Op::StoreLocal(r)),
                Ins::Jump(done),
                Ins::Mark(minus_one),
                Ins::Op(Op::PushI64(0)),
                Ins::Op(Op::StoreLocal(r)),
            ]);
        } else {
            ops.extend([
                Ins::Op(Op::LoadLocal(q)),
                Ins::Op(Op::PushI64(1)),
                Ins::Op(Op::Sub),
                Ins::Op(Op::StoreLocal(q)),
            ]);
        }
        ops.push(Ins::Mark(done));
        ops.push(Ins::Op(Op::LoadLocal(if percent { r } else { q })));
        ops.into_iter().map(|ins| (ins, span)).collect()
    }

    /// Starlark's arithmetic `x >> n` from the IR's logical shift (subset 4.4): `x >> n` for
    /// `x >= 0`, `~((~x) >> n)` otherwise.
    fn shift_right(&mut self, id: usize, span: Span) -> Code {
        let (x, n) = (self.temp(id, span), self.temp(id, span));
        let (negative, end) = (self.label(), self.label());
        let ops = vec![
            Ins::Op(Op::StoreLocal(n)),
            Ins::Op(Op::StoreLocal(x)),
            Ins::Op(Op::LoadLocal(x)),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::CmpLt),
            Ins::Op(Op::Not),
            Ins::JumpIfFalse(negative),
            Ins::Op(Op::LoadLocal(x)),
            Ins::Op(Op::LoadLocal(n)),
            Ins::Op(Op::Shr),
            Ins::Jump(end),
            Ins::Mark(negative),
            Ins::Op(Op::LoadLocal(x)),
            Ins::Op(Op::PushI64(-1)),
            Ins::Op(Op::BitXor),
            Ins::Op(Op::LoadLocal(n)),
            Ins::Op(Op::Shr),
            Ins::Op(Op::PushI64(-1)),
            Ins::Op(Op::BitXor),
            Ins::Mark(end),
        ];
        ops.into_iter().map(|ins| (ins, span)).collect()
    }

    /// `a == b`, `a < b` or `a > b` (`op`) on the floats on the stack, in Starlark's total order:
    /// NaN equals NaN and is greater than every other float, where the IR's comparisons follow
    /// IEEE 754 and are false for NaN. A float is NaN when it does not equal itself.
    fn float_compare(&mut self, id: usize, op: BinOp, span: Span) -> Code {
        // The temporaries hold floats.
        let (a, b) = (self.temp(id, span), self.temp(id, span));
        for slot in [a, b] {
            if let Some(tys) = self.funcs[id].slot_tys.get_mut(usize::from(slot)) {
                *tys = Tys::of(Ty::Float);
            }
        }
        let (a_nan, b_nan, end) = (self.label(), self.label(), self.label());
        let ieee = match op {
            BinOp::Less => Op::CmpLt,
            BinOp::Greater => Op::CmpGt,
            _ => Op::CmpEq,
        };
        let mut ops = vec![
            Ins::Op(Op::StoreLocal(b)),
            Ins::Op(Op::StoreLocal(a)),
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::CmpEq),
            Ins::JumpIfFalse(a_nan),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::CmpEq),
            Ins::JumpIfFalse(b_nan),
            // Neither is NaN.
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(ieee),
            Ins::Jump(end),
            // Only b is NaN: a is less.
            Ins::Mark(b_nan),
        ];
        let boolean = |this: &mut Self, b: bool| {
            this.push_const(&Const::Bool(b), span)
                .into_iter()
                .map(|(ins, _)| ins)
                .collect::<Vec<_>>()
        };
        ops.extend(boolean(self, op == BinOp::Less));
        ops.extend([Ins::Jump(end), Ins::Mark(a_nan)]);
        // Only a, or both, are NaN: a equals b when b is NaN too, and is greater otherwise.
        match op {
            BinOp::Less => ops.extend(boolean(self, false)),
            _ => {
                ops.extend([
                    Ins::Op(Op::LoadLocal(b)),
                    Ins::Op(Op::LoadLocal(b)),
                    Ins::Op(Op::CmpEq),
                ]);
                if op == BinOp::Equal {
                    ops.push(Ins::Op(Op::Not));
                }
            }
        }
        ops.push(Ins::Mark(end));
        ops.into_iter().map(|ins| (ins, span)).collect()
    }

    fn index(&mut self, object: Val, index: Val, span: Span) -> Val {
        match (object, index) {
            (Val::Const(object), Val::Const(index)) => {
                // A dict is searched; a list or bytes is indexed directly.
                let cost = match &object {
                    Const::Dict(..) => object.size(),
                    _ => 1,
                };
                if !self.charge(cost.saturating_add(index.size()), span) {
                    return Val::Const(Const::Int(0));
                }
                self.fold_index_checked(object, index, span)
            }
            (_, Val::Const(Const::Int(n))) if n < 0 => {
                self.not_lowerable(
                    span,
                    "a negative index into bytes known at run time is not lowered yet (2.5)",
                );
                Val::Run(Tys::of(Ty::Int), Vec::new())
            }
            (object, index) => self.index_at_run_time(object, index, span),
        }
    }

    /// `res["ok"]`, `res["nrc"]` or `res["raw"]` of a `diag.request` response (subset 3.4,
    /// ADR-317): `code` pushes the response bytes. A response is negative when its first byte is
    /// the marker of the variant's negative-response shape, and its NRC is then the byte at the
    /// shape's code position (ADR-309 item 4, [`Self::negative_shape`]).
    fn response_key(
        &mut self,
        id: Option<usize>,
        code: Code,
        key: Val,
        service: Service,
        span: Span,
    ) -> Val {
        let Val::Const(Const::Str(key)) = key else {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                span,
                "a response is read with a string literal key (3.4, 4.1)",
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let mut code = code;
        match &*key {
            "raw" => Val::Run(Tys::of(Ty::Bytes), code),
            "ok" => {
                code.extend(self.response_ok(span));
                Val::Run(Tys::of(Ty::Bool), code)
            }
            "nrc" => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                let response = self.temp(id, span);
                let (positive, end) = (self.label(), self.label());
                code.extend([
                    (Ins::Op(Op::StoreLocal(response)), span),
                    (Ins::Op(Op::LoadLocal(response)), span),
                ]);
                code.extend(self.response_ok(span));
                let code_at = i64::from(self.negative_shape().1);
                code.extend(
                    [
                        Ins::Op(Op::Not),
                        Ins::JumpIfFalse(positive),
                        // A negative response holds the NRC at the shape's code position; a
                        // shorter one fails the run as an index out of range.
                        Ins::Op(Op::LoadLocal(response)),
                        Ins::Op(Op::PushI64(code_at)),
                        Ins::Op(Op::IndexGet),
                        Ins::Op(Op::Swap),
                        Ins::Op(Op::Pop),
                        Ins::Jump(end),
                        Ins::Mark(positive),
                        Ins::Op(Op::PushI64(0)),
                        Ins::Mark(end),
                        // The slot would otherwise keep a copy of the response in the VM state,
                        // and its journaled checkpoints, until the function returns.
                        Ins::Op(Op::PushI64(0)),
                        Ins::Op(Op::StoreLocal(response)),
                    ]
                    .map(|ins| (ins, span)),
                );
                Val::Run(Tys::of(Ty::Int), code)
            }
            "fields" => Val::Run(
                Tys {
                    bits: Ty::Fields.bit(),
                    service,
                },
                code,
            ),
            other => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    span,
                    format!(
                        "a response has the keys `ok`, `nrc`, `fields` and `raw`, not `{other}` \
                         (3.4)"
                    ),
                );
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    /// `res["fields"]["name"]` (subset 3.4, ADR-320): `code` pushes the response bytes of a
    /// request to `service`, and the field the variant declares under `name` among the top-level
    /// fields of that service's positive response is decoded from them (`Op::DecodeField`,
    /// ADR-316). The field's declaration gives the value's type.
    fn field_key(&mut self, code: Code, key: Val, service: Service, span: Span) -> Val {
        let Val::Const(Const::Str(key)) = key else {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                span,
                "a response's fields are read with a string literal key (3.4, 4.1)",
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let (service, profiled) = match service {
            Service::One(service) => (service, false),
            Service::Dtcs(service) => (service, true),
            // The request's own error is already reported.
            Service::Unknown => return Val::Run(Tys::default(), Vec::new()),
            Service::Several => {
                self.not_lowerable(
                    span,
                    "this value can hold responses to different services, so its fields are not \
                     known at ingestion; keep each response in a variable of its own (3.4, 4.8)",
                );
                return Val::Run(Tys::default(), Vec::new());
            }
        };
        let declared = self
            .variant
            .services()
            .and_then(|services| services.iter().find(|s| s.id() == service))
            .and_then(|s| s.positive());
        let field = if profiled {
            let Some(position) = self.dtc_result_position(&key, span) else {
                return Val::Run(Tys::default(), Vec::new());
            };
            declared
                .and_then(|positive| positive.fields())
                .and_then(|fields| fields.iter().nth(position))
        } else {
            declared
                .and_then(|positive| positive.fields())
                .and_then(|fields| {
                    fields
                        .iter()
                        .find(|f| f.short_name().is_some_and(|name| name == &*key))
                })
        };
        let Some(field) = field else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                format!(
                    "the response declares no field `{key}` among the top-level fields of its \
                     positive response (3.4)"
                ),
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let ty = match diag_ir::decode::scalar_kind(&field) {
            Ok(diag_ir::decode::ScalarKind::Integer) => Ty::Int,
            Ok(diag_ir::decode::ScalarKind::Float) => Ty::Float,
            Ok(diag_ir::decode::ScalarKind::Text) => Ty::Str,
            Ok(diag_ir::decode::ScalarKind::Bytes) => Ty::Bytes,
            Err(what) => {
                self.not_lowerable(
                    span,
                    format!("the field `{key}` is {what}, which a procedure cannot read (3.4)"),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
        };
        let mut code = code;
        code.push((
            Ins::Op(Op::DecodeField {
                service,
                field: field.id(),
            }),
            span,
        ));
        Val::Run(Tys::of(ty), code)
    }

    /// The marker that starts a negative response, and the position of its code: those of the
    /// shape the variant's protocols declare, which all name one application layer (ADR-309
    /// items 3 and 4). A variant that declares no protocol (in tests) uses its profile's.
    fn negative_shape(&self) -> (u8, u8) {
        let declared = self
            .variant
            .protocols()
            .and_then(|protocols| protocols.iter().next())
            .and_then(|protocol| protocol.negative_shape());
        match declared {
            Some(shape) => (shape.marker(), shape.code_at()),
            None => {
                let shape = self.requests.profile.negative;
                (shape.marker, shape.code_at)
            }
        }
    }

    /// Code that turns the response bytes on top of the stack into whether the response is
    /// positive: its first byte, if any, is not the declared shape's marker.
    fn response_ok(&mut self, span: Span) -> Code {
        let marker = self.negative_shape().0;
        let negative = self.pool(&[marker]);
        [
            Op::PushI64(0),
            Op::PushI64(1),
            Op::Slice,
            Op::PushBytes(negative),
            Op::CmpEq,
            Op::Not,
        ]
        .map(|op| (Ins::Op(op), span))
        .to_vec()
    }

    fn fold_index_checked(&mut self, object: Const, index: Const, span: Span) -> Val {
        match fold_index(&object, &index) {
            Ok(value) => Val::Const(value),
            Err(reason) => {
                self.not_lowerable(span, reason);
                Val::Const(Const::Int(0))
            }
        }
    }

    fn index_at_run_time(&mut self, object: Val, index: Val, span: Span) -> Val {
        let (Some((to, co)), Some((ti, ci))) = (
            self.single(object, span, "an index"),
            self.single(index, span, "an index"),
        ) else {
            return Val::Run(Tys::default(), Vec::new());
        };
        if (to, ti) != (Ty::Bytes, Ty::Int) {
            self.not_lowerable(
                span,
                format!(
                    "only bytes known at run time can be indexed, by an int; this is a {} indexed by a {} (2.5)",
                    to.name(),
                    ti.name()
                ),
            );
            return Val::Run(Tys::default(), Vec::new());
        }
        let mut code = co;
        code.extend(ci);
        // `IndexGet` leaves the bytes below the byte read.
        code.extend([Op::IndexGet, Op::Swap, Op::Pop].map(|op| (Ins::Op(op), span)));
        Val::Run(Tys::of(Ty::Int), code)
    }

    fn slice(&mut self, object: Const, start: Option<i64>, stop: Option<i64>, span: Span) -> Val {
        // Charged before copying, so slicing a shared constant again and again is bounded too.
        if !self.charge(object.size(), span) {
            return Val::Const(Const::Int(0));
        }
        // The same bounds `Op::Slice` takes at run time.
        let (from, to) =
            diag_ir::slice_bounds(start.unwrap_or(0), stop.unwrap_or(i64::MAX), object.len());
        match object {
            Const::Bytes(b) => self.bounded(Const::bytes(&b[from..to]), span),
            Const::List(l, _) => self.bounded(Const::list(l[from..to].to_vec()), span),
            // Indexes count bytes here, which are characters only in ASCII.
            Const::Str(s) if s.is_ascii() => self.bounded(Const::str(&s[from..to]), span),
            other => {
                self.not_lowerable(
                    span,
                    format!("a {} cannot be sliced here", other.type_name()),
                );
                Val::Const(Const::Int(0))
            }
        }
    }

    /// A built-in of 2.4 on values known only at run time: `int` of a number and `float` of a
    /// number, through the IR's conversions. The others are not lowered yet.
    fn run_builtin(&mut self, name: &str, values: Vec<Val>, span: Span) -> Val {
        let failed = || Val::Run(Tys::default(), Vec::new());
        if let ("int" | "float", [_]) = (name, values.as_slice()) {
            let value = values.into_iter().next().expect("one argument");
            let Some((ty, mut code)) = self.single(value, span, &format!("`{name}`")) else {
                return failed();
            };
            let result = match (name, ty) {
                ("int", Ty::Int) | ("float", Ty::Float) => ty,
                ("int", Ty::Float) => {
                    code.push((Ins::Op(Op::FloatToInt), span));
                    Ty::Int
                }
                ("float", Ty::Int) => {
                    code.push((Ins::Op(Op::IntToFloat), span));
                    Ty::Float
                }
                _ => {
                    self.not_lowerable(
                        span,
                        format!(
                            "`{name}` of a {} known only at run time is not lowered yet (2.4)",
                            ty.name()
                        ),
                    );
                    return failed();
                }
            };
            return Val::Run(Tys::of(result), code);
        }
        self.not_lowerable(
            span,
            format!("`{name}` on a value known only at run time is not lowered yet (2.4)"),
        );
        failed()
    }

    /// Divides the two floats on top of the stack. Starlark rejects a division by zero, while
    /// the IR's float `Div` gives an infinity or NaN: a zero divisor is sent to the integer
    /// `Div` by zero, which stops the program with the same error.
    fn float_divide(&mut self, span: Span) -> Code {
        let (zero, end) = (self.label(), self.label());
        [
            Ins::Op(Op::Dup),
            Ins::Op(Op::PushF64(0.0)),
            Ins::Op(Op::CmpEq),
            Ins::Op(Op::Not),
            Ins::JumpIfFalse(zero),
            Ins::Op(Op::Div),
            Ins::Jump(end),
            Ins::Mark(zero),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::PushI64(0)),
            Ins::Op(Op::Div),
            Ins::Mark(end),
        ]
        .map(|ins| (ins, span))
        .into()
    }

    /// A slice of bytes known only at run time, or with bounds known only at run time: the
    /// bytes, the start (0 when omitted) and the stop (`i64::MAX` when omitted, which `Op::Slice`
    /// clamps to the end), then `Op::Slice`. A string is not sliced at run time, since its
    /// indexes count characters and the VM holds it as UTF-8 bytes.
    fn run_slice(&mut self, object: Val, start: Option<Val>, stop: Option<Val>, span: Span) -> Val {
        let failed = || Val::Run(Tys::default(), Vec::new());
        let Some((ty, mut code)) = self.single(object, span, "a slice") else {
            return failed();
        };
        if ty != Ty::Bytes {
            self.not_lowerable(
                span,
                format!(
                    "only bytes are sliced at run time; this value is {} (2.5)",
                    ty.name()
                ),
            );
            return failed();
        }
        for (bound, omitted) in [(start, 0), (stop, i64::MAX)] {
            match bound {
                None => code.push((Ins::Op(Op::PushI64(omitted)), span)),
                Some(bound) => match self.single(bound, span, "a slice index") {
                    Some((Ty::Int, bound)) => code.extend(bound),
                    Some((other, _)) => {
                        self.not_lowerable(
                            span,
                            format!("a slice index must be an int, not {}", other.name()),
                        );
                        return failed();
                    }
                    None => return failed(),
                },
            }
        }
        code.push((Ins::Op(Op::Slice), span));
        Val::Run(Tys::of(Ty::Bytes), code)
    }

    // ------------------------------------------------------------ Calls

    fn call(
        &mut self,
        id: Option<usize>,
        function: &AstExpr,
        args: &[AstArgument],
        span: Span,
    ) -> Val {
        // Each argument is looked at in every unrolled copy, even when one is refused.
        if self.unrolling > 0 && !self.unroll_step(span, args.len()) {
            return Val::Run(Tys::default(), Vec::new());
        }
        let mut positional = Vec::new();
        for arg in args {
            match &arg.node {
                ArgumentP::Positional(expr) => positional.push(expr),
                _ => {
                    self.not_lowerable(arg.span, "only positional arguments are lowered (2.3)");
                    return Val::Run(Tys::default(), Vec::new());
                }
            }
        }
        match &function.node {
            ExprP::Identifier(ident) => {
                let name = ident.node.ident.as_str();
                // The name is looked up among the functions, then the builtins.
                if !self.charge_name(name, 2, function.span) {
                    return Val::Run(Tys::default(), Vec::new());
                }
                if let Some(&callee) = self.by_name.get(name) {
                    return self.call_function(id, callee, &positional, span);
                }
                let values: Vec<Val> = positional.iter().map(|arg| self.expr(id, arg)).collect();
                if BUILTINS.contains(&name) {
                    let all_const = values.iter().all(|v| matches!(v, Val::Const(_)));
                    let (consts, values_copy): (Option<Vec<Const>>, Vec<Val>) = if all_const {
                        let consts = values
                            .into_iter()
                            .map(|v| match v {
                                Val::Const(c) => c,
                                _ => unreachable!("all constant"),
                            })
                            .collect();
                        (Some(consts), Vec::new())
                    } else {
                        (None, values)
                    };
                    if let Some(args) = &consts {
                        // `min` and `max` compare their best so far with every argument, so
                        // the largest argument may be read once per argument.
                        let largest = args.iter().map(Const::size).max().unwrap_or(0);
                        let cost = args
                            .iter()
                            .fold(1usize, |cost, arg| cost.saturating_add(arg.size()))
                            .saturating_add(largest.saturating_mul(args.len()));
                        if !self.charge(cost, span) {
                            return Val::Const(Const::Int(0));
                        }
                    }
                    return match consts.map(|args| fold_builtin(name, &args)) {
                        Some(Ok(value)) => self.bounded(value, span),
                        Some(Err(reason)) => {
                            self.not_lowerable(span, reason);
                            Val::Run(Tys::default(), Vec::new())
                        }
                        None => self.run_builtin(name, values_copy, span),
                    };
                }
                self.not_lowerable(span, format!("`{name}` cannot be called here"));
                Val::Run(Tys::default(), Vec::new())
            }
            ExprP::Dot(object, method) => {
                if matches!(&object.node, ExprP::Identifier(m) if m.node.ident == "diag")
                    && !id.is_some_and(|id| self.funcs[id].slots.contains_key("diag"))
                    && !self.globals.contains_key("diag")
                    && !self.by_name.contains_key("diag")
                {
                    return self.call_diag(id, &method.node, &positional, span);
                }
                let receiver = self.expr(id, object);
                let values: Vec<Val> = positional.iter().map(|arg| self.expr(id, arg)).collect();
                let consts: Option<Vec<Const>> = values
                    .into_iter()
                    .map(|v| match v {
                        Val::Const(c) => Some(c),
                        _ => None,
                    })
                    .collect();
                match (receiver, consts) {
                    (Val::Const(Const::Str(s)), Some(args))
                        if STRING_METHODS.contains(&method.node.as_str()) =>
                    {
                        let cost = args
                            .iter()
                            .fold(1 + s.len(), |cost, arg| cost.saturating_add(arg.size()));
                        if !self.charge(cost, span) {
                            return Val::Const(Const::Int(0));
                        }
                        match fold_method(&s, &method.node, &args) {
                            Ok(value) => self.bounded(value, span),
                            Err(reason) => {
                                self.not_lowerable(span, reason);
                                Val::Run(Tys::default(), Vec::new())
                            }
                        }
                    }
                    _ => {
                        self.not_lowerable(
                            span,
                            format!(
                                "`.{}` on a value known only at run time is not lowered yet (2.4)",
                                method.node
                            ),
                        );
                        Val::Run(Tys::default(), Vec::new())
                    }
                }
            }
            _ => {
                self.not_lowerable(span, "this call cannot be lowered");
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    fn call_function(
        &mut self,
        id: Option<usize>,
        callee: usize,
        args: &[&AstExpr],
        span: Span,
    ) -> Val {
        self.called.push(callee);
        let mut code = Vec::new();
        if args.len() != self.funcs[callee].params.len() {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                format!(
                    "`{}` takes {} arguments, not {}",
                    self.funcs[callee].name,
                    self.funcs[callee].params.len(),
                    args.len()
                ),
            );
            return Val::Run(Tys::default(), Vec::new());
        }
        for (at, arg) in args.iter().enumerate() {
            let value = self.expr(id, arg);
            let Some((tys, more)) = self.force(value, arg.span) else {
                return Val::Run(Tys::default(), Vec::new());
            };
            code.extend(more);
            self.widen_slot(callee, at as u16, tys);
        }
        code.push((Ins::Call(callee), span));
        if self.funcs[callee].returns_value {
            Val::Run(self.funcs[callee].ret, code)
        } else {
            Val::Void(code)
        }
    }

    fn call_diag(&mut self, id: Option<usize>, name: &str, args: &[&AstExpr], span: Span) -> Val {
        match name {
            "fail" => {
                let [code_arg, detail] = args else {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        span,
                        "`diag.fail` takes a code and a detail (3.1)",
                    );
                    return Val::Void(Vec::new());
                };
                let code_value = self.expr(id, code_arg);
                let failure = match code_value {
                    Val::Const(Const::Str(text))
                        if (1..=MAX_FAIL_CODE_LEN).contains(&text.len()) =>
                    {
                        Some(self.pool(text.as_bytes()))
                    }
                    Val::Const(Const::Str(_)) => {
                        self.error(
                            StarCode::ArgumentOutOfRange,
                            code_arg.span,
                            "a `diag.fail` code is 1 to 64 bytes (3.1)",
                        );
                        None
                    }
                    _ => {
                        self.error(
                            StarCode::NonConstantArgument,
                            code_arg.span,
                            "a `diag.fail` code is a string constant (3.1)",
                        );
                        None
                    }
                };
                let detail_value = self.expr(id, detail);
                let detail_code = match self.single(
                    detail_value,
                    detail.span,
                    "a `diag.fail` detail",
                ) {
                    Some((Ty::Str, code)) => Some(code),
                    Some((ty, _)) => {
                        self.not_lowerable(
                            detail.span,
                            format!(
                                "a `diag.fail` detail is a string; turning a {} into one needs a conversion instruction (2.5)",
                                ty.name()
                            ),
                        );
                        None
                    }
                    None => None,
                };
                match (failure, detail_code) {
                    (Some(failure), Some(mut code)) => {
                        code.push((Ins::Op(Op::Fail { code: failure }), span));
                        Val::Void(code)
                    }
                    _ => Val::Void(Vec::new()),
                }
            }
            "wait" | "capture" => {
                let what = if name == "wait" {
                    "millis"
                } else {
                    "back_millis"
                };
                let [millis] = args else {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        span,
                        format!("`diag.{name}` takes one argument, `{what}` (3.1)"),
                    );
                    return Val::Void(Vec::new());
                };
                let Some(millis) =
                    self.constant_int(id, millis, name, what, 0..=i64::from(u32::MAX))
                else {
                    return Val::Void(Vec::new());
                };
                // In range by the check above.
                let millis = millis as u32;
                let op = if name == "wait" {
                    Op::Wait { millis }
                } else {
                    Op::MonitorCapture {
                        back_millis: millis,
                    }
                };
                Val::Void(vec![(Ins::Op(op), span)])
            }
            "log" => {
                let [level, message] = args else {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        span,
                        "`diag.log` takes a level and a message (3.1)",
                    );
                    return Val::Void(Vec::new());
                };
                let level = self.constant_int(id, level, name, "level", 0..=255);
                let message = match self.expr(id, message) {
                    Val::Const(Const::Str(text)) => Some(text),
                    Val::Const(other) => {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            message.span,
                            format!("a `diag.log` message is a str, not a {}", other.type_name()),
                        );
                        None
                    }
                    _ => {
                        self.error(
                            StarCode::NonConstantArgument,
                            message.span,
                            "a `diag.log` message must be a string constant: `Log` takes it from \
                             the constant pool, and a message built at run time needs a log \
                             instruction that takes it from the stack (3.2)",
                        );
                        None
                    }
                };
                match (level, message) {
                    (Some(level), Some(text)) => {
                        // Found by its shared allocation, so a large global logged many times
                        // is not hashed again for each call.
                        let message = self.pool_shared(&Const::Str(text));
                        Val::Void(vec![(
                            Ins::Op(Op::Log {
                                // In range by the check above.
                                level: level as u8,
                                message,
                            }),
                            span,
                        )])
                    }
                    _ => Val::Void(Vec::new()),
                }
            }
            "request" => self.diag_request(id, args, span),
            "read_dtc" if self.requests.profile.call(Intent::ReadDtc).is_some() => {
                self.diag_read_dtc(id, args, span)
            }
            "read_dtc" | "routine" | "security_access" => {
                let intent = match name {
                    "read_dtc" => Intent::ReadDtc,
                    "routine" => Intent::Routine,
                    _ => Intent::SecurityAccess,
                };
                let profile = self.requests.profile;
                if profile.call(intent).is_none() {
                    self.error(
                        StarCode::NotInProfile,
                        span,
                        format!(
                            "the {} profile of this variant's protocols does not define \
                             `diag.{name}` (3.2)",
                            profile.layer
                        ),
                    );
                } else {
                    self.not_lowerable(span, format!("`diag.{name}` is not lowered yet (3.2)"));
                }
                Val::Run(Tys::default(), Vec::new())
            }
            "ecu_info" | "precondition" => {
                self.error(
                    StarCode::ApiNotAvailable,
                    span,
                    format!("`diag.{name}` has no IR instruction yet (3.2)"),
                );
                Val::Run(Tys::default(), Vec::new())
            }
            _ => {
                self.not_lowerable(span, format!("`diag.{name}` is not lowered yet (3.2)"));
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    /// `diag.request(service_id, params)`: selects the variant's service the SID and the
    /// params' keys name, by the rule of `docs/csv-tables.md` 5.3 (ADR-313), and sends the
    /// request [`encode_request`] writes from the remaining params. Its value is the response
    /// (ADR-317), read through [`Self::response_key`].
    fn diag_request(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let [sid_arg, params_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.request` takes a service identifier and params (3.1)",
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        let sid = self
            .constant_int(id, sid_arg, "request", "service_id", 0..=0xFF)
            .filter(|&sid| {
                // RequestDownload, TransferData and RequestTransferExit bound a flash plan's
                // transfer, which only `diag.flash_transfer` lowers (`Program::validate`).
                let transfer = matches!(sid, 0x34 | 0x36 | 0x37);
                if transfer {
                    self.error(
                        StarCode::ArgumentOutOfRange,
                        sid_arg.span,
                        format!(
                            "`diag.request` cannot send service 0x{sid:02X}: a data transfer is \
                             sent by `diag.flash_transfer` (3.2)"
                        ),
                    );
                }
                !transfer
            });
        let params = match self.request_params(id, sid, params_arg) {
            None => None,
            Some(Val::Const(Const::Dict(entries, _))) => Some(entries),
            Some(Val::Const(other)) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    params_arg.span,
                    format!(
                        "`params` of `diag.request` is a dict, not a {} (3.1)",
                        other.type_name()
                    ),
                );
                None
            }
            Some(_) => {
                self.error(
                    StarCode::NonConstantArgument,
                    params_arg.span,
                    "`params` of `diag.request` must be a dict known at ingestion: they select \
                     the service (3.1)",
                );
                None
            }
        };
        let (Some(sid), Some(params)) = (sid, params) else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        // In range by the check above.
        let sid = sid as u8;
        let Some((service, request, constants)) = self.select_request(sid, &params) else {
            let mut keys: Vec<&str> = params.iter().map(|(key, _)| &**key).collect();
            keys.sort_unstable();
            self.error(
                StarCode::UnknownService,
                span,
                format!(
                    "no single service of this variant has SID 0x{sid:02X}, takes the params {} \
                     and has the constants they give (csv-tables 5.3)",
                    if keys.is_empty() {
                        "{}".to_owned()
                    } else {
                        keys.join(", ")
                    }
                ),
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        let mut values = HashMap::new();
        for (key, value) in params.iter() {
            if constants.contains(&**key) {
                continue;
            }
            let value = match value {
                Const::Int(n) => ParamValue::Int(*n),
                Const::Float(x) => ParamValue::Float(*x),
                Const::Bytes(bytes) => ParamValue::Bytes(bytes),
                Const::Str(text) => ParamValue::Text(text),
                other => {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        params_arg.span,
                        format!(
                            "the `{key}` param of `diag.request` is an int, float, bytes or str, \
                             not a {} (3.1)",
                            other.type_name()
                        ),
                    );
                    return Val::Run(Tys::of(Ty::Response), Vec::new());
                }
            };
            values.insert(&**key, value);
        }
        match encode_request(&request, &values) {
            Ok(operand) => {
                let operand = self.pool(&operand);
                Val::Run(
                    Tys::from_service(Ty::Response, service),
                    vec![
                        (Ins::Op(Op::PushBytes(operand)), span),
                        (
                            Ins::Op(Op::ServiceRequest {
                                service: u16::from(sid),
                            }),
                            span,
                        ),
                    ],
                )
            }
            Err(e) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    params_arg.span,
                    format!("the request of `diag.request` cannot be encoded: {e} (3.1)"),
                );
                Val::Run(Tys::of(Ty::Response), Vec::new())
            }
        }
    }

    /// The `params` of `diag.request(sid, ...)` as a value. A dict literal is built here, so
    /// that a value known only at run time is reported for what it is: under a key some service
    /// with the SID holds as a constant it is needed to select the service (csv-tables 5.3);
    /// under another key it is a field's value, which only a run-time encoding could send.
    /// `None` once the dict literal's errors are reported.
    fn request_params(
        &mut self,
        id: Option<usize>,
        sid: Option<i64>,
        arg: &AstExpr,
    ) -> Option<Val> {
        let ExprP::Dict(pairs) = &arg.node else {
            return Some(self.expr(id, arg));
        };
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        let mut complete = true;
        for (key, value) in pairs {
            let key = match self.expr(id, key) {
                Val::Const(Const::Str(key)) => key,
                _ => {
                    self.not_lowerable(key.span, "a dict key must be a string literal (4.1)");
                    complete = false;
                    continue;
                }
            };
            if !keys.insert(key.clone()) {
                self.not_lowerable(
                    arg.span,
                    format!("the dict repeats the key {key:?}, which Starlark rejects"),
                );
                complete = false;
                continue;
            }
            match self.expr(id, value) {
                Val::Const(value) => entries.push((key, value)),
                _ => {
                    complete = false;
                    let constant = self.requests.services.iter().any(|service| {
                        Some(i64::from(service.sid)) == sid
                            && !service.routine
                            && service.constants.iter().any(|(name, _)| **name == *key)
                    });
                    if constant {
                        self.error(
                            StarCode::NonConstantArgument,
                            value.span,
                            format!(
                                "the `{key}` param of `diag.request` selects the service, so \
                                 it must be known at ingestion (3.4)"
                            ),
                        );
                    } else {
                        self.not_lowerable(
                            value.span,
                            format!(
                                "the `{key}` param of `diag.request` is known only at run time; \
                                 a request is encoded at ingestion for now (3.2)"
                            ),
                        );
                    }
                }
            }
        }
        if !complete {
            return None;
        }
        // Anything but a dict means `bounded` refused it and reported why.
        match self.bounded(Const::dict(entries), arg.span) {
            value @ Val::Const(Const::Dict(..)) => Some(value),
            _ => None,
        }
    }

    /// `diag.read_dtc(mask)` through the variant's profile (ADR-309 items 2 to 4, ADR-314):
    /// the request the profile's template builds, sent as it is by the generic send-receive to
    /// the declared service the template selects. The result is read under the profile's names
    /// ([`Self::dtc_result_position`]).
    fn diag_read_dtc(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let [mask_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.read_dtc` takes a status mask (3.1)",
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        let mask = self.constant_int(id, mask_arg, "read_dtc", "mask", 0..=0xFF);
        let profile = self.requests.profile;
        let call = profile.call(Intent::ReadDtc).expect("the caller checked");
        let [step] = call.steps else {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile's `diag.read_dtc` takes several steps, which are not lowered \
                     yet (3.2)",
                    profile.layer
                ),
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        let Some(mask) = mask else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        // In range by the check above.
        let args = [(Source::Mask, mask as u64)];
        let Some((service, bytes)) = self.select_template(&step.request, &args, span) else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        if !self.dtc_response_fits(service, step.result, span) {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        }
        let operand = self.pool(&bytes);
        Val::Run(
            Tys::from_dtc_service(service),
            vec![
                (Ins::Op(Op::PushBytes(operand)), span),
                (Ins::Op(Op::SendReceive { service }), span),
            ],
        )
    }

    /// The one declared service of this variant that a profile's request template selects
    /// (ADR-314 item 2), with the complete request bytes, its SID first: the template's
    /// constants and selecting arguments, encoded at their widths, must be the declared
    /// request's prefix byte for byte, its other arguments fill the declared request's fields in
    /// order, each a fixed `uint` of the template's width with no conversion, and the request
    /// must encode to exactly the bytes the template stands for. `args` gives the call's
    /// argument values. `None` after an error.
    fn select_template(
        &mut self,
        template: &crate::profile::Request,
        args: &[(Source, u64)],
        span: Span,
    ) -> Option<(u32, Vec<u8>)> {
        let value = |source: Source| {
            args.iter()
                .find(|(s, _)| *s == source)
                .map(|&(_, v)| v)
                .expect("the call gives each source its template uses")
        };
        let services = self.variant.services();
        let mut found = Vec::new();
        for indexed in &self.requests.services {
            if indexed.sid != template.sid || indexed.routine {
                continue;
            }
            let Some(request) = services.and_then(|services| {
                self.service_at
                    .get(&indexed.service_id)
                    .and_then(|&at| services.get(at).request())
            }) else {
                continue;
            };
            let fields: Vec<_> = request.params().into_iter().flatten().collect();
            let mut fields_at = fields.iter();
            // The constants lead the request (csv-tables 5.3), so they are its prefix.
            let mut prefix = Vec::new();
            // The operand the template stands for: the prefix, then each argument at its width.
            let mut expected = Vec::new();
            let mut values = HashMap::new();
            let fits = template.parts.iter().all(|part| {
                let (bits, constant) = match *part {
                    Part::Const { bits, value } => (bits, value),
                    Part::Select { bits, arg } => (bits, value(arg)),
                    _ => (0, 0),
                };
                match *part {
                    // A constant after a field cannot be part of the prefix.
                    Part::Const { .. } | Part::Select { .. } if values.is_empty() => {
                        let width = usize::from(bits / 8);
                        prefix.extend_from_slice(&constant.to_be_bytes()[8 - width..]);
                        expected.extend_from_slice(&constant.to_be_bytes()[8 - width..]);
                        true
                    }
                    Part::Const { .. } | Part::Select { .. } => false,
                    // A profile's argument is a raw value such as a bit mask, so its field is
                    // an unsigned integer that sends it as it is, not BCD or a signed number.
                    Part::Arg { bits, arg } => fields_at.next().is_some_and(|field| {
                        let fits = is_fixed_uint(field, bits)
                            && field.compu_type() == diag_ir::declaration::Compu::CompuIdentity;
                        // A value an `i64` cannot hold selects nothing; the profiles' arguments
                        // are 8 or 16 bits wide.
                        let value = i64::try_from(value(arg));
                        if let (true, Ok(value)) = (fits, value) {
                            let name = field.short_name().unwrap_or_default();
                            values.insert(name, ParamValue::Int(value));
                            let width = usize::from(bits / 8);
                            expected.extend_from_slice(&value.to_be_bytes()[8 - width..]);
                        }
                        fits && value.is_ok()
                    }),
                    // No call that is lowered through a template yet takes `params`.
                    Part::Params => false,
                }
            });
            let declared_prefix = request.prefix().map(|p| p.bytes()).unwrap_or_default();
            if !fits || declared_prefix != &prefix[..] || fields_at.next().is_some() {
                continue;
            }
            // The declared fields must also sit where the template puts the arguments, in its
            // byte order: the request encodes to exactly the template's bytes.
            if encode_request(&request, &values).is_ok_and(|operand| operand == expected) {
                found.push((indexed.service_id, expected));
            }
        }
        let layer = self.requests.profile.layer;
        let [(service, operand)] = &found[..] else {
            self.error(
                StarCode::UnknownService,
                span,
                format!(
                    "{} services of this variant have SID 0x{:02X} and the constants and \
                     fields the {layer} profile's request needs, not one (csv-tables 5.3, \
                     ADR-314)",
                    found.len(),
                    template.sid
                ),
            );
            return None;
        };
        Some((*service, [&[template.sid][..], operand].concat()))
    }

    /// Whether the declared positive response of `service` fits the profile's `diag.read_dtc`
    /// map (ADR-314 item 3): an integer field as wide as a DTC's status at the mask's position,
    /// and a field at the records' position. Reports the field that does not.
    fn dtc_response_fits(&mut self, service: u32, map: ResultMap, span: Span) -> bool {
        let ResultMap::ReadDtc {
            mask,
            dtcs,
            code_bits,
            status_bits,
        } = map
        else {
            unreachable!("`Profile::validate` gives `diag.read_dtc` its DTC map")
        };
        let positive = self
            .variant
            .services()
            .and_then(|services| services.iter().find(|s| s.id() == service))
            .and_then(|s| s.positive());
        let fields: Vec<_> = positive
            .and_then(|positive| positive.fields())
            .into_iter()
            .flatten()
            .collect();
        let prefix = positive.and_then(|p| p.prefix()).map_or(0, |p| p.len());
        let layer = self.requests.profile.layer;
        let mask_field = fields.get(usize::from(mask.0));
        let problem = match mask_field {
            None => Some(format!("has no field at the mask's position {}", mask.0)),
            // The availability mask is a status mask, as wide as each DTC's status, and its bits
            // are read as they are.
            Some(field)
                if !is_fixed_uint(field, status_bits)
                    || field.compu_type() != diag_ir::declaration::Compu::CompuIdentity =>
            {
                Some(format!(
                    "has `{}`, which is not a {status_bits}-bit `uint` without a conversion, at \
                     the mask's position",
                    field.short_name().unwrap_or_default()
                ))
            }
            Some(_) => match fields.get(usize::from(dtcs.0)) {
                None => Some(format!(
                    "has no field at the DTC records' position {}",
                    dtcs.0
                )),
                Some(records) if !self.is_dtc_records(records, code_bits, status_bits) => {
                    Some(format!(
                        "has `{}` at the DTC records' position, which is not a repeat until the \
                         end of the message of a {code_bits}-bit code then a {status_bits}-bit \
                         status, both `uint` without a conversion",
                        records.short_name().unwrap_or_default()
                    ))
                }
                // The records run to the end of the message, so nothing can follow them.
                Some(records) if fields.len() > usize::from(dtcs.0) + 1 => Some(format!(
                    "declares fields after `{}`, which runs to the end of the message",
                    records.short_name().unwrap_or_default()
                )),
                // The profile's fields are on the wire one after the other from the end of the
                // prefix, as the profile's response has them.
                Some(_) => fields[..=usize::from(dtcs.0)]
                    .iter()
                    .scan(Some((1 + prefix as u32) * 8), |next, field| {
                        let start = (*next)?;
                        let placed = match field.placement() {
                            diag_ir::declaration::Placement::Absolute => {
                                field.bit_offset() == start
                            }
                            diag_ir::declaration::Placement::AfterPrevious => {
                                field.bit_offset() == 0 && start % 8 == 0
                            }
                            _ => false,
                        };
                        *next = (field.length_kind() == LengthKind::Fixed
                            && field.shape_type() == diag_ir::declaration::Shape::NONE)
                            .then(|| start + field.bit_length());
                        Some((field, placed))
                    })
                    .find(|(_, placed)| !placed)
                    .map(|(field, _)| {
                        format!(
                            "has `{}` apart from the field before it, so the response is not \
                             the profile's fields one after the other",
                            field.short_name().unwrap_or_default()
                        )
                    }),
            },
        };
        if let Some(problem) = problem {
            self.error(
                StarCode::NotInProfile,
                span,
                format!(
                    "the positive response of the selected service {problem}, so it does not fit \
                     the {layer} profile's `diag.read_dtc` (ADR-314)"
                ),
            );
            return false;
        }
        true
    }

    /// Whether `records` repeats, until the end of the message, an unpadded layout of exactly a
    /// `code_bits`-bit code and then a `status_bits`-bit status, each a `uint` without a
    /// conversion: the records the profile's `dtcs` reads (ADR-314 item 4).
    fn is_dtc_records(
        &self,
        records: &diag_ir::declaration::Field<'_>,
        code_bits: u8,
        status_bits: u8,
    ) -> bool {
        let Some(repeat) = records.shape_as_repeat() else {
            return false;
        };
        if repeat.count() != diag_ir::declaration::RepeatCount::UntilEndOfPdu {
            return false;
        }
        let layout = self
            .variant
            .layouts()
            .and_then(|layouts| layouts.iter().find(|l| l.id() == repeat.layout_id()));
        let fields: Vec<_> = layout
            .and_then(|l| l.fields())
            .into_iter()
            .flatten()
            .collect();
        // A field starts at `offset` bits into the record: placed there, or right after the
        // previous field (whole bytes here, so the bit position is 0).
        let at = |field: &diag_ir::declaration::Field<'_>, bits: u8, offset: u32| {
            use diag_ir::declaration::Placement;
            is_fixed_uint(field, bits)
                && field.compu_type() == diag_ir::declaration::Compu::CompuIdentity
                && match field.placement() {
                    Placement::Absolute => field.bit_offset() == offset,
                    Placement::AfterPrevious => field.bit_offset() == 0,
                    _ => false,
                }
        };
        // Each record is the layout's size apart, so it may not be padded (0 is its extent).
        let size = u32::from(code_bits) / 8 + u32::from(status_bits) / 8;
        let unpadded = layout.is_some_and(|l| l.byte_size() == 0 || l.byte_size() == size);
        unpadded
            && matches!(&fields[..], [code, status]
                if at(code, code_bits, 0) && at(status, status_bits, u32::from(code_bits)))
    }

    /// The position among the declared response's top-level fields of `key`, a name the
    /// profile's `diag.read_dtc` map defines (ADR-314 item 4), or `None` after an error.
    fn dtc_result_position(&mut self, key: &str, span: Span) -> Option<usize> {
        let profile = self.requests.profile;
        let map = profile.call(Intent::ReadDtc).and_then(|call| {
            call.steps.iter().find_map(|step| match step.result {
                ResultMap::ReadDtc { mask, dtcs, .. } => Some((mask, dtcs)),
                _ => None,
            })
        });
        let Some((mask, _)) = map else {
            unreachable!("only a `diag.read_dtc` the profile defines is lowered")
        };
        match key {
            "mask" => Some(usize::from(mask.0)),
            "dtcs" => {
                self.not_lowerable(
                    span,
                    "`dtcs` is a list of records, which a procedure cannot read yet; read `raw` \
                     instead (3.4)",
                );
                None
            }
            _ => {
                self.error(
                    StarCode::NotInProfile,
                    span,
                    format!(
                        "the result of `diag.read_dtc` has `mask` and `dtcs`, the names the {} \
                         profile maps it to, not `{key}` (3.4, ADR-314)",
                        profile.layer
                    ),
                );
                None
            }
        }
    }

    /// The one service of this variant that `diag.request(sid, params)` names, with the names of
    /// its request constants, or `None` when none or several match (csv-tables 5.3).
    fn select_request(
        &self,
        sid: u8,
        params: &[(Rc<str>, Const)],
    ) -> Option<(u32, Request<'a>, HashSet<&'a str>)> {
        let services = self.variant.services()?;
        let mut found = None;
        for indexed in &self.requests.services {
            if indexed.sid != sid || indexed.routine {
                continue;
            }
            let constants_match = indexed.constants.iter().all(|(name, constant)| {
                params
                    .iter()
                    .find(|(key, _)| **key == **name)
                    .is_some_and(|(_, value)| match (constant, value) {
                        (RequestConstant::Uint(c), Const::Int(n)) => {
                            u64::try_from(*n).is_ok_and(|n| n == *c)
                        }
                        (RequestConstant::Bytes(c), Const::Bytes(b)) => **c == **b,
                        _ => false,
                    })
            });
            if !constants_match {
                continue;
            }
            let Some(request) = self
                .service_at
                .get(&indexed.service_id)
                .and_then(|&at| services.get(at).request())
            else {
                continue;
            };
            let constants: HashSet<&'a str> = indexed
                .constants
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            let fields: HashSet<&str> = request
                .params()
                .into_iter()
                .flatten()
                .map(|field| field.short_name().unwrap_or_default())
                .collect();
            let rest: HashSet<&str> = params
                .iter()
                .map(|(key, _)| &**key)
                .filter(|key| !constants.contains(key))
                .collect();
            if rest != fields {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((indexed.service_id, request, constants));
        }
        found
    }

    /// The value of `arg`, an argument of `diag.{function}` that is fixed at ingestion, in its
    /// instruction or in the request bytes built then: an `int` constant in `range` (subset
    /// 3.1), or `None` after an error.
    fn constant_int(
        &mut self,
        id: Option<usize>,
        arg: &AstExpr,
        function: &str,
        what: &str,
        range: std::ops::RangeInclusive<i64>,
    ) -> Option<i64> {
        match self.expr(id, arg) {
            Val::Const(Const::Int(n)) if range.contains(&n) => Some(n),
            Val::Const(Const::Int(n)) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    arg.span,
                    format!(
                        "`{what}` of `diag.{function}` is {n}, outside {} to {} (3.1)",
                        range.start(),
                        range.end()
                    ),
                );
                None
            }
            Val::Const(other) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    arg.span,
                    format!(
                        "`{what}` of `diag.{function}` is an int, not a {} (3.1)",
                        other.type_name()
                    ),
                );
                None
            }
            _ => {
                self.error(
                    StarCode::NonConstantArgument,
                    arg.span,
                    format!(
                        "`{what}` of `diag.{function}` must be known at ingestion: it is a \
                         fixed field of the instruction or of the request built then (3.1)"
                    ),
                );
                None
            }
        }
    }

    // ------------------------------------------------------------ Assembly

    /// Lays out the entry (a call to `main`, then the end) and the functions, and resolves
    /// labels, calls and source positions.
    fn assemble(&mut self) -> Program {
        let main = self.by_name.get("main").copied().unwrap_or(0);
        let entry_span = self.funcs[main].def.name.span;
        let mut code: Code = vec![
            (Ins::Call(main), entry_span),
            (Ins::Op(Op::Ret), entry_span),
        ];
        for func in self.funcs.iter_mut().filter(|f| f.emitted) {
            code.append(&mut func.code);
        }
        // Positions of labels, counting only real instructions.
        let mut positions: HashMap<Label, u32> = HashMap::new();
        let mut pc = 0u32;
        for (ins, _) in &code {
            match ins {
                Ins::Mark(label) => {
                    positions.insert(*label, pc);
                }
                _ => pc += 1,
            }
        }
        let labels: Vec<Label> = self.funcs.iter().map(|f| f.label).collect();
        let mut ops = Vec::with_capacity(pc as usize);
        let mut spans = Vec::with_capacity(pc as usize);
        for (ins, span) in code {
            let op = match ins {
                Ins::Mark(_) => continue,
                Ins::Op(op) => op,
                Ins::Jump(label) => Op::Jump(positions[&label]),
                Ins::JumpIfFalse(label) => Op::JumpIfFalse(positions[&label]),
                Ins::Call(callee) => Op::Call(positions[&labels[callee]]),
            };
            ops.push(op);
            spans.push(span);
        }
        let source_map = self.source_map(&spans);
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code: ops,
            constants: std::mem::take(&mut self.pool),
            sections: Vec::new(),
            source_map,
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Default::default(),
        }
    }

    /// One entry for each run of instructions from the same source position; an entry covers
    /// its instruction and those after it up to the next entry (ADR-304). Columns count
    /// characters from 1, found in one sweep over the sorted positions.
    fn source_map(&self, spans: &[Span]) -> Vec<SourceSpan> {
        let module = &self.checked.module;
        let codemap = module.file_span(module.statement().span).file;
        let source = codemap.source();
        let mut begins: Vec<u32> = spans.iter().map(|s| s.begin().get()).collect();
        begins.sort_unstable();
        begins.dedup();
        let mut located: HashMap<u32, (u32, u32)> = HashMap::with_capacity(begins.len());
        let (mut current, mut byte, mut column) = (usize::MAX, 0usize, 0usize);
        for begin in begins {
            let line = codemap.find_line(starlark_syntax::codemap::Pos::new(begin));
            if line != current {
                current = line;
                byte = codemap.line_span(line).begin().get() as usize;
                column = 0;
            }
            let at = begin as usize;
            column += source[byte..at].chars().count();
            byte = at;
            located.insert(begin, (to_u32(line + 1), to_u32(column + 1)));
        }
        let mut map = Vec::new();
        let mut previous = None;
        for (pc, span) in spans.iter().enumerate() {
            let position = located[&span.begin().get()];
            if previous != Some(position) {
                map.push(SourceSpan {
                    pc: to_u32(pc),
                    file: 0,
                    line: position.0,
                    column: position.1,
                });
                previous = Some(position);
            }
        }
        map
    }
}

/// A condition's value.
enum Cond {
    Known(bool),
    /// Code that pushes a `Bool`.
    Run(Code),
}

#[derive(Clone, Copy)]
enum Unary {
    Minus,
    Plus,
    BitNot,
}

impl Unary {
    fn symbol(self) -> &'static str {
        match self {
            Unary::Minus => "unary `-`",
            Unary::Plus => "unary `+`",
            Unary::BitNot => "`~`",
        }
    }
}

fn assign_op(op: AssignOp) -> BinOp {
    match op {
        AssignOp::Add => BinOp::Add,
        AssignOp::Subtract => BinOp::Subtract,
        AssignOp::Multiply => BinOp::Multiply,
        AssignOp::Divide => BinOp::Divide,
        AssignOp::FloorDivide => BinOp::FloorDivide,
        AssignOp::Percent => BinOp::Percent,
        AssignOp::BitAnd => BinOp::BitAnd,
        AssignOp::BitOr => BinOp::BitOr,
        AssignOp::BitXor => BinOp::BitXor,
        AssignOp::LeftShift => BinOp::LeftShift,
        AssignOp::RightShift => BinOp::RightShift,
    }
}

fn binop_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Or => "`or`",
        BinOp::And => "`and`",
        BinOp::Equal => "`==`",
        BinOp::NotEqual => "`!=`",
        BinOp::Less => "`<`",
        BinOp::Greater => "`>`",
        BinOp::LessOrEqual => "`<=`",
        BinOp::GreaterOrEqual => "`>=`",
        BinOp::In => "`in`",
        BinOp::NotIn => "`not in`",
        BinOp::Subtract => "`-`",
        BinOp::Add => "`+`",
        BinOp::Multiply => "`*`",
        BinOp::Percent => "`%`",
        BinOp::Divide => "`/`",
        BinOp::FloorDivide => "`//`",
        BinOp::BitAnd => "`&`",
        BinOp::BitOr => "`|`",
        BinOp::BitXor => "`^`",
        BinOp::LeftShift => "`<<`",
        BinOp::RightShift => "`>>`",
    }
}

/// Why an operator cannot be lowered for operands of these types known only at run time.
fn binary_hint(op: BinOp, a: Ty, b: Ty) -> String {
    let symbol = binop_symbol(op);
    let reason = match op {
        BinOp::LeftShift => "an overflow-checked shift instruction (4.4)",
        BinOp::In | BinOp::NotIn => "a search instruction",
        BinOp::Percent if a == Ty::Str => "string formatting instructions (2.5)",
        BinOp::FloorDivide | BinOp::Percent
            if matches!(a, Ty::Int | Ty::Float) && matches!(b, Ty::Int | Ty::Float) =>
        {
            "a float floor, which is not lowered yet (4.4)"
        }
        _ => {
            return format!(
                "{symbol} does not apply to a {} and a {}",
                a.name(),
                b.name()
            );
        }
    };
    format!(
        "{symbol} on a {} and a {} known at run time needs {reason}",
        a.name(),
        b.name()
    )
}

/// Whether some `return` in the body carries a value.
fn returns_value(stmt: &AstStmt) -> bool {
    match &stmt.node {
        StmtP::Return(value) => value.is_some(),
        StmtP::Statements(list) => list.iter().any(returns_value),
        StmtP::If(_, then) => returns_value(then),
        StmtP::IfElse(_, branches) => returns_value(&branches.0) || returns_value(&branches.1),
        StmtP::For(for_) => returns_value(&for_.body),
        _ => false,
    }
}

/// Whether every path through the body ends in a `return`.
/// Counts, for each name, how often `stmt` reads it or updates it with an augmented
/// assignment.
fn count_mentions(stmt: &AstStmt, counts: &mut HashMap<String, usize>) {
    if let StmtP::AssignModify(target, ..) = &stmt.node
        && let AssignTargetP::Identifier(name) = &target.node
    {
        *counts.entry(name.node.ident.clone()).or_default() += 1;
    }
    stmt.visit_children(|child| match child {
        Visit::Stmt(inner) => count_mentions(inner, counts),
        Visit::Expr(expr) => count_expr_mentions(expr, counts),
    });
}

fn count_expr_mentions(expr: &AstExpr, counts: &mut HashMap<String, usize>) {
    if let ExprP::Identifier(ident) = &expr.node {
        *counts.entry(ident.node.ident.clone()).or_default() += 1;
    }
    expr.visit_expr(|inner| count_expr_mentions(inner, counts));
}

/// The items of a constant loop, shared with the constant they come from.
enum Items {
    List(Rc<[Const]>),
    /// A dict's keys.
    Keys(Rc<[(Rc<str>, Const)]>),
}

impl Items {
    fn len(&self) -> usize {
        match self {
            Items::List(items) => items.len(),
            Items::Keys(entries) => entries.len(),
        }
    }

    fn get(&self, at: usize) -> Const {
        match self {
            Items::List(items) => items[at].clone(),
            Items::Keys(entries) => Const::Str(entries[at].0.clone()),
        }
    }
}

fn always_returns(stmt: &AstStmt) -> bool {
    match &stmt.node {
        StmtP::Return(_) => true,
        StmtP::Statements(list) => list.iter().any(always_returns),
        StmtP::IfElse(_, branches) => always_returns(&branches.0) && always_returns(&branches.1),
        // `diag.fail` ends the procedure, so no path through it returns (3.2).
        StmtP::Expression(expr) => is_diag_fail(expr),
        _ => false,
    }
}

fn is_diag_fail(expr: &AstExpr) -> bool {
    let ExprP::Call(function, _) = &expr.node else {
        return false;
    };
    let ExprP::Dot(object, method) = &function.node else {
        return false;
    };
    method.node == "fail" && matches!(&object.node, ExprP::Identifier(m) if m.node.ident == "diag")
}

/// Whether `expr` is the integer literal 2^63, whatever its spelling.
fn is_two_to_the_63(expr: &AstExpr) -> bool {
    let ExprP::Literal(AstLiteral::Int(int)) = &expr.node else {
        return false;
    };
    matches!(&int.node, TokenInt::BigInt(big) if u64::try_from(big) == Ok(1 << 63))
}

// ---------------------------------------------------------------- Folding

fn number(value: &Const) -> Option<f64> {
    match value {
        Const::Int(n) => Some(*n as f64),
        Const::Float(f) => Some(*f),
        _ => None,
    }
}

/// Starlark's floor division and remainder on integers. `x % -1` is 0 for every `x`, so only
/// the quotient of the smallest integer by -1 is out of range.
fn floor_div_mod(a: i64, b: i64, percent: bool) -> Result<(i64, i64), String> {
    if b == 0 {
        return Err("division by zero".to_owned());
    }
    if b == -1 && percent {
        return Ok((0, 0));
    }
    let q = a
        .checked_div(b)
        .ok_or("the result is outside the 64-bit integer range (4.4)")?;
    let r = a - q * b;
    if r != 0 && ((r < 0) != (b < 0)) {
        Ok((q - 1, r + b))
    } else {
        Ok((q, r))
    }
}

/// Folds a binary operator on two constants with Starlark's semantics.
fn fold(op: BinOp, a: &Const, b: &Const) -> Result<Const, String> {
    use Const::*;
    let range = || "the result is outside the 64-bit integer range (4.4)".to_owned();
    let mismatch = || {
        format!(
            "{} does not apply to a {} and a {}",
            binop_symbol(op),
            a.type_name(),
            b.type_name()
        )
    };
    match op {
        BinOp::And => Ok(if a.truth() { b.clone() } else { a.clone() }),
        BinOp::Or => Ok(if a.truth() { a.clone() } else { b.clone() }),
        BinOp::Equal => Ok(Bool(equal(a, b))),
        BinOp::NotEqual => Ok(Bool(!equal(a, b))),
        BinOp::Less | BinOp::Greater | BinOp::LessOrEqual | BinOp::GreaterOrEqual => {
            let ordering = match (a, b) {
                (Str(x), Str(y)) => x.cmp(y),
                (Bytes(x), Bytes(y)) => x.cmp(y),
                _ => match (a, b) {
                    (Int(x), Int(y)) => x.cmp(y),
                    _ => match number_order(a, b) {
                        Some(ordering) => ordering,
                        None => return Err(mismatch()),
                    },
                },
            };
            use std::cmp::Ordering::*;
            Ok(Bool(match op {
                BinOp::Less => ordering == Less,
                BinOp::Greater => ordering == Greater,
                BinOp::LessOrEqual => ordering != Greater,
                _ => ordering != Less,
            }))
        }
        BinOp::In | BinOp::NotIn => {
            let found = match (a, b) {
                (_, List(items, _)) => items.iter().any(|item| equal(a, item)),
                (Str(key), Dict(entries, _)) => entries.iter().any(|(k, _)| k == key),
                (Str(needle), Str(haystack)) => haystack.contains(&**needle),
                (Bytes(needle), Bytes(haystack)) => contains_bytes(haystack, needle),
                (Int(byte), Bytes(haystack)) => match u8::try_from(*byte) {
                    Ok(byte) => haystack.contains(&byte),
                    Err(_) => return Err(format!("{byte} is not a byte value (0 to 255)")),
                },
                _ => return Err(mismatch()),
            };
            Ok(Bool(found == (op == BinOp::In)))
        }
        BinOp::Add | BinOp::Subtract | BinOp::Multiply => match (a, b) {
            (Int(x), Int(y)) => match op {
                BinOp::Add => x.checked_add(*y),
                BinOp::Subtract => x.checked_sub(*y),
                _ => x.checked_mul(*y),
            }
            .map(Int)
            .ok_or_else(range),
            (Str(x), Str(y)) if op == BinOp::Add => Ok(Const::str(format!("{x}{y}"))),
            (Bytes(x), Bytes(y)) if op == BinOp::Add => Ok(Const::bytes([&**x, &**y].concat())),
            (List(x, _), List(y, _)) if op == BinOp::Add => Ok(Const::list([&**x, &**y].concat())),
            _ => match (number(a), number(b)) {
                (Some(x), Some(y)) => Ok(Float(match op {
                    BinOp::Add => x + y,
                    BinOp::Subtract => x - y,
                    _ => x * y,
                })),
                _ => Err(mismatch()),
            },
        },
        BinOp::Divide => match (number(a), number(b)) {
            (Some(_), Some(0.0)) => Err("division by zero".to_owned()),
            (Some(x), Some(y)) => Ok(Float(x / y)),
            _ => Err(mismatch()),
        },
        BinOp::FloorDivide | BinOp::Percent => match (a, b) {
            (Int(x), Int(y)) => {
                let (q, r) = floor_div_mod(*x, *y, op == BinOp::Percent)?;
                Ok(Int(if op == BinOp::FloorDivide { q } else { r }))
            }
            _ => match (number(a), number(b)) {
                (Some(_), Some(0.0)) => Err("division by zero".to_owned()),
                (Some(x), Some(y)) if op == BinOp::FloorDivide => Ok(Float((x / y).floor())),
                // The remainder takes the divisor's sign; `x - floor(x / y) * y` would lose
                // precision, or overflow, where the remainder itself is exact.
                (Some(x), Some(y)) => {
                    let r = x % y;
                    Ok(Float(if r != 0.0 && (r < 0.0) != (y < 0.0) {
                        r + y
                    } else {
                        r
                    }))
                }
                _ => Err(mismatch()),
            },
        },
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => match (a, b) {
            (Int(x), Int(y)) => Ok(Int(match op {
                BinOp::BitAnd => x & y,
                BinOp::BitOr => x | y,
                _ => x ^ y,
            })),
            _ => Err(mismatch()),
        },
        BinOp::LeftShift | BinOp::RightShift => match (a, b) {
            // As at run time (4.4), and unlike Starlark, a count outside 0 to 63 is an error.
            (Int(_), Int(n)) if !(0..64).contains(n) => {
                Err(format!("the shift count {n} is outside 0 to 63 (4.4)"))
            }
            (Int(x), Int(n)) if op == BinOp::RightShift => Ok(Int(x >> n)),
            (Int(x), Int(n)) => {
                // A Starlark integer grows rather than losing bits; the result must fit.
                let shifted = u32::try_from(*n)
                    .ok()
                    .filter(|&n| n < 64)
                    .and_then(|n| x.checked_shl(n).filter(|r| r >> n == *x));
                shifted.map(Int).ok_or_else(range)
            }
            _ => Err(mismatch()),
        },
    }
}

/// Whether `needle` occurs in `haystack`, in time linear in both (Knuth-Morris-Pratt), so a
/// repetitive constant cannot make the search quadratic.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    // fallback[i]: the length of the longest proper prefix of needle[..=i] that is also its
    // suffix.
    let mut fallback = vec![0usize; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle[i] != needle[k] {
            k = fallback[k - 1];
        }
        if needle[i] == needle[k] {
            k += 1;
        }
        fallback[i] = k;
    }
    let mut matched = 0;
    for &byte in haystack {
        while matched > 0 && byte != needle[matched] {
            matched = fallback[matched - 1];
        }
        if byte == needle[matched] {
            matched += 1;
            if matched == needle.len() {
                return true;
            }
        }
    }
    false
}

/// Starlark's total order on floats: NaN equals NaN and is greater than every other float.
fn float_order(x: f64, y: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Equal,
        (true, false) => Greater,
        (false, true) => Less,
        (false, false) => x.partial_cmp(&y).unwrap_or(Equal),
    }
}

/// The order of two numbers by their exact values, `None` for other values: an `int` and a
/// `float` are compared without rounding the `int` to the nearest `float`.
fn number_order(a: &Const, b: &Const) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Const::Int(x), Const::Int(y)) => Some(x.cmp(y)),
        (Const::Float(x), Const::Float(y)) => Some(float_order(*x, *y)),
        (Const::Int(x), Const::Float(y)) => Some(int_float_order(*x, *y)),
        (Const::Float(x), Const::Int(y)) => Some(int_float_order(*y, *x).reverse()),
        _ => None,
    }
}

/// The exact order of an `int` and a `float`, NaN the greatest.
fn int_float_order(x: i64, y: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    // 2^63, exact as a float.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if y.is_nan() || y >= LIMIT {
        return Less;
    }
    if y < -LIMIT {
        return Greater;
    }
    // Here y's integer part fits an i64 exactly.
    let whole = y.trunc();
    match x.cmp(&(whole as i64)) {
        Equal => 0.0.partial_cmp(&(y - whole)).unwrap_or(Equal),
        other => other,
    }
}

/// Starlark equality: numbers compare by value across `int` and `float`; other values of
/// different types are never equal.
fn equal(a: &Const, b: &Const) -> bool {
    match (a, b) {
        (Const::Int(_) | Const::Float(_), Const::Int(_) | Const::Float(_)) => match (a, b) {
            (Const::Int(x), Const::Int(y)) => x == y,
            _ => number_order(a, b).is_some_and(|o| o.is_eq()),
        },
        (Const::List(x, _), Const::List(y, _)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| equal(p, q))
        }
        // Keys are unique (a literal that repeats one is rejected), so equal lengths and every
        // entry of one found in the other make the dicts equal.
        (Const::Dict(x, _), Const::Dict(y, _)) => {
            let other: HashMap<&str, &Const> = y.iter().map(|(k, v)| (&**k, v)).collect();
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| other.get(&**k).is_some_and(|w| equal(v, w)))
        }
        _ => a == b,
    }
}

fn fold_index(object: &Const, index: &Const) -> Result<Const, String> {
    let position = |n: i64, len: usize| -> Result<usize, String> {
        let at = if n < 0 { n + len as i64 } else { n };
        usize::try_from(at)
            .ok()
            .filter(|&at| at < len)
            .ok_or_else(|| format!("index {n} is out of range"))
    };
    match (object, index) {
        (Const::List(items, _), Const::Int(n)) => Ok(items[position(*n, items.len())?].clone()),
        (Const::Bytes(bytes), Const::Int(n)) => {
            Ok(Const::Int(i64::from(bytes[position(*n, bytes.len())?])))
        }
        (Const::Dict(entries, _), Const::Str(key)) => entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("the dict has no key {key:?}")),
        _ => Err(format!(
            "a {} cannot be indexed by a {}",
            object.type_name(),
            index.type_name()
        )),
    }
}

/// The built-ins of 2.4 on constants. Only those whose result needs no further definition here
/// are folded; the rest wait for their own lowering.
fn fold_builtin(name: &str, args: &[Const]) -> Result<Const, String> {
    match (name, args) {
        ("len", [Const::Bytes(_) | Const::List(..) | Const::Dict(..)]) => {
            Ok(Const::Int(args[0].len() as i64))
        }
        ("abs", [Const::Int(n)]) => n
            .checked_abs()
            .map(Const::Int)
            .ok_or_else(|| "the result is outside the 64-bit integer range (4.4)".to_owned()),
        ("abs", [Const::Float(f)]) => Ok(Const::Float(f.abs())),
        ("bool", [value]) => Ok(Const::Bool(value.truth())),
        ("int", [Const::Int(n)]) => Ok(Const::Int(*n)),
        ("int", [Const::Bool(b)]) => Ok(Const::Int(i64::from(*b))),
        // The truncated value must lie in [-2^63, 2^63), where the conversion is exact.
        ("int", [Const::Float(f)])
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f.trunc()) =>
        {
            Ok(Const::Int(f.trunc() as i64))
        }
        ("float", [Const::Int(n)]) => Ok(Const::Float(*n as f64)),
        ("float", [Const::Float(f)]) => Ok(Const::Float(*f)),
        ("min" | "max", [_, _, ..]) => {
            let mut best = args[0].clone();
            // The first of equal values is kept, as in Starlark.
            let better = if name == "min" {
                BinOp::Less
            } else {
                BinOp::Greater
            };
            for arg in &args[1..] {
                if fold(better, arg, &best)? == Const::Bool(true) {
                    best = arg.clone();
                }
            }
            Ok(best)
        }
        _ => Err(format!(
            "`{name}` with these arguments is not lowered yet (2.4)"
        )),
    }
}

fn fold_method(s: &str, method: &str, args: &[Const]) -> Result<Const, String> {
    match (method, args) {
        ("upper", []) => Ok(Const::str(s.to_uppercase())),
        ("lower", []) => Ok(Const::str(s.to_lowercase())),
        ("strip", []) => Ok(Const::str(s.trim())),
        ("startswith", [Const::Str(p)]) => Ok(Const::Bool(s.starts_with(&**p))),
        ("endswith", [Const::Str(p)]) => Ok(Const::Bool(s.ends_with(&**p))),
        _ => Err(format!(
            "`.{method}` with these arguments is not lowered yet (2.4)"
        )),
    }
}

#[cfg(test)]
#[path = "lower_tests.rs"]
mod tests;

/// Whether `field` is a fixed-width `uint` of `bits` bits, a value sent or read as its own bits.
fn is_fixed_uint(field: &diag_ir::declaration::Field<'_>, bits: u8) -> bool {
    field.kind() == diag_ir::declaration::DataKind::Uint
        && field.length_kind() == LengthKind::Fixed
        && field.bit_length() == u32::from(bits)
        && field.shape_type() == diag_ir::declaration::Shape::NONE
}
