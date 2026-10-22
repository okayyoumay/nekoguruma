//! Lowering a checked Starlark procedure to a `diag_ir::Program` (subset 2.5, 4.1, 4.4;
//! ADR-254, ADR-304).
//!
//! The core of the lowering: top-level constants (folded at ingestion), top-level `def`s with
//! positional parameters, calls between them, locals, the operators the IR's instructions
//! cover, `and`, `or`, `not`, conditional expressions, `if`/`elif`/`else`, `for` loops
//! (ADR-305), and `diag.fail`, `diag.wait`, `diag.log` and `diag.capture`, and section functions,
//! each covered by a `Section` together with its own copies of the functions it reaches (ADR-301,
//! ADR-329). Every diagnostic primitive gets a one-instruction `Section` that carries its
//! idempotency, from `@idempotency` or the call's default (ADR-332). Every value that reaches
//! run time has one type, inferred at ingestion (ADR-304); an operation the IR cannot express
//! for its operands is `STAR_NOT_LOWERABLE`.
//!
//! A flash plan is expanded in place at its one call in `main`, with its `diag.flash_transfer`,
//! and gets its `FlashRecovery` (4.9, ADR-336). Every function it calls, directly or not, is
//! expanded in place at its call too (`Lowerer::inline_call`): its `Ret`s jump to the join, its
//! locals take fresh slots in the caller's frame, and a section function's pieces are laid over
//! its range.
//!
//! `diag.hmi` and `diag.record` lower to `HmiRequest`, `HmiRequestFromStack` and `RecordInput`
//! with an encoded `HmiTemplate` constant, and their answer is read by the keys `ok`,
//! `timed_out` and `value` (ADR-340).
//!
//! Not lowered yet, each its own backlog item: the built-ins and string methods on values known
//! only at run time that have no lowering (`abs`, `str`, `find`, ...) name the instruction they
//! wait for. Each is rejected with `STAR_NOT_LOWERABLE`, so nothing is lowered silently wrong.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use diag_ir::declaration::{DataKind, EcuVariant, FlashStep, LengthKind, Request};
use diag_ir::encode::{ParamValue, encode_request};
use diag_ir::hmi::{
    HmiParam, HmiTemplate, HmiValue, MAX_HMI_ID_LEN, MAX_HMI_KEY_LEN, MAX_HMI_PARAMS,
    encode_template,
};
use diag_ir::{
    FlashRecovery, IR_SCHEMA_VERSION, Idempotency, Interruptible, MAX_BYTES_LEN, NoApplication, Op,
    PreconditionKind, Preconditions, Program, ProgramError, RecoveryBoundaries, RecoveryRequired,
    RecoveryTiming, Section, SourceSpan,
};
use starlark_syntax::codemap::Span;
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignOp, AssignTargetP, AstArgument, AstExpr, AstLiteral, AstNoPayload, AstStmt,
    BinOp, DefP, ExprP, ForP, ParameterP, StmtP,
};
use starlark_syntax::syntax::uniplate::Visit;

use crate::profile::{Intent, Part, ResultMap, Source, StepKind, UDS};

use super::{
    Annotation, BUILTINS, Checked, FlashFacts, Pending, RequestConstant, RequestIndex,
    STRING_METHODS, SectionKind, StarCode, StarError, collect_locals, is_string_expression,
    resolve, to_u32,
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

/// The most specialized copies of functions one file may need (subset 2.6): a section's copies
/// of the functions it reaches count toward it (4.5, ADR-301).
const MAX_COPIES: usize = 1000;

/// The most instructions the specialized copies may hold together (subset 2.6, 4.5), so a large
/// function reached from many sections cannot make the converter build an unbounded program.
const MAX_COPIED_INSTRUCTIONS: usize = 1 << 22;

/// The `Fail` code of a loop that runs past its limit (subset 4.2, ADR-280).
const LOOP_LIMIT_EXCEEDED: &str = "LOOP_LIMIT_EXCEEDED";
/// The `Fail` code of a `write_uint` whose value does not fit its width (2.4).
const UINT_OUT_OF_RANGE: &str = "UINT_OUT_OF_RANGE";
/// The `Fail` code of a request param known only at run time whose value does not fit its
/// field (3.2).
const PARAM_OUT_OF_RANGE: &str = "PARAM_OUT_OF_RANGE";

/// A `params` value of `diag.request` or `diag.routine` known only at run time: its key, the
/// value, and where it is written.
struct RunParam {
    key: Rc<str>,
    value: Val,
    span: Span,
}

/// Where the profile's DTC records are and how each one is laid out (`Lowerer::dtc_layout`).
#[derive(Debug, Clone, Copy)]
struct DtcLayout {
    /// The mask's position among the response's top-level fields.
    mask: usize,
    /// The records field's position among them.
    position: usize,
    code_bytes: u8,
    status_bytes: u8,
    record_bytes: u8,
}

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

/// The message of a program that needs more copies of functions than the budgets of 2.6 allow.
fn specialization_limit() -> String {
    format!(
        "the sections and the flash plans' in-place expansions need more than {MAX_COPIES} copies \
         of the functions they reach, or more than {MAX_COPIED_INSTRUCTIONS} instructions in them \
         (2.6, 4.5, 4.9)"
    )
}

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
    super::on_parser_stack(|| lower_here(file, checked, variant, requests, MAX_COPIED_INSTRUCTIONS))
}

/// A lowered procedure and the warnings its lowering reported (such as
/// `STAR_LOOP_LIMIT_DEFAULTED`, subset 4.2), which do not reject it.
#[derive(Debug)]
pub struct Lowered {
    pub program: Program,
    pub warnings: Vec<StarError>,
}

/// The error for a program `validate` refused (ADR-336 item 4): `STAR_PLAN_INVALID` at the source
/// span of the instruction the error names. One that names a section, not an instruction, is
/// `STAR_NOT_LOWERABLE` at the call of the section function the section is a piece of, when it
/// was expanded in place, and otherwise at the source of its first instruction; one that names
/// neither is `STAR_NOT_LOWERABLE` at the start of the file. `spans` holds each instruction's source span.
fn invalid_program(
    checked: &Checked,
    program: &Program,
    spans: &[Span],
    origins: &[Option<Origin>],
    error: &ProgramError,
) -> Pending {
    let mut hint = format!("the lowered program is not valid: {error}");
    if let Some(span) = error.pc().and_then(|pc| spans.get(pc as usize)) {
        return Pending {
            code: StarCode::PlanInvalid,
            span: *span,
            hint,
        };
    }
    let section = match error {
        ProgramError::ContradictoryInterruptibility { section, .. }
        | ProgramError::UnsafeSectionInReplay { section, .. }
        | ProgramError::InvalidSection { section } => Some(*section),
        _ => None,
    };
    let start = section
        .and_then(|section| program.sections.get(section))
        .map(|section| section.start_pc);
    // The section function that produced the section, and its call when it was expanded in place.
    let mut origin = section
        .and_then(|section| origins.get(section))
        .cloned()
        .flatten();
    // A section function expanded in place lays its pieces over the plan's, so a primitive can
    // have a piece of each at one pc; the error then belongs to the innermost expansion there.
    if origin.as_ref().is_none_or(|origin| origin.call.is_none())
        && let Some(start) = start
    {
        let inner = program
            .sections
            .iter()
            .zip(origins)
            .filter(|(other, _)| other.start_pc == start)
            .filter_map(|(other, origin)| {
                origin
                    .as_ref()
                    .filter(|origin| origin.call.is_some())
                    .map(|origin| (other.end_pc - other.start_pc, origin))
            })
            .min_by_key(|(len, _)| *len)
            .map(|(_, origin)| origin.clone());
        if inner.is_some() {
            origin = inner;
        }
    }
    if let (Some(origin), Some(section)) = (&origin, section) {
        // The section's index in the program means nothing in the source; name its function.
        let name = &origin.function;
        hint = format!(
            "the lowered program is not valid: {}",
            error.to_string().replace(
                &format!("section {section}"),
                &format!("the section of `{name}`")
            )
        );
    }
    let at = origin
        .and_then(|origin| origin.call)
        .or_else(|| start.and_then(|start| spans.get(start as usize).copied()));
    let span = match at {
        Some(span) => span,
        None => {
            let span = checked.module.statement().span;
            Span::new(span.begin(), span.begin())
        }
    };
    Pending {
        code: StarCode::NotLowerable,
        span,
        hint,
    }
}

/// The section function behind a section of the lowered program: for one expanded in place, the
/// call it was expanded at.
#[derive(Clone)]
struct Origin {
    /// The section function's name.
    function: Rc<str>,
    call: Option<Span>,
}

/// A range of a copy's code whose pieces are cut as sections, with the section function's
/// attributes and its origin.
type Covered = (u32, u32, (Interruptible, u32), Origin);

fn lower_here<'a>(
    file: &str,
    checked: &'a Checked,
    variant: EcuVariant<'a>,
    requests: &'a RequestIndex,
    max_copied: usize,
) -> Result<Lowered, Vec<StarError>> {
    let mut lowerer = Lowerer::new(checked, variant, requests);
    lowerer.max_copied = max_copied;
    lowerer.constants();
    lowerer.globals_built = lowerer.built;
    lowerer.functions();
    lowerer.preconditions();
    lowerer.flash_plans();
    if lowerer.errors.is_empty() {
        lowerer.infer();
        lowerer.reject_if_exhausted();
        if let Some(span) = lowerer.inline_limit
            && lowerer.errors.is_empty()
        {
            lowerer.error(StarCode::SpecializationLimit, span, specialization_limit());
        }
    }
    if lowerer.errors.is_empty() {
        lowerer.emit_all();
        lowerer.reject_if_exhausted();
    }
    if !lowerer.errors.is_empty() {
        let errors = std::mem::take(&mut lowerer.errors);
        return Err(resolve(file, &checked.module, errors));
    }
    let (program, spans, origins) = lowerer.assemble();
    // `validate` still refuses what the source can ask for and the lowering does not look at:
    // a section function that requires intervention before the recovery point, an unsafe
    // section in a restartable plan's replay, the identity rows.
    if let Err(error) = program.validate() {
        return Err(resolve(
            file,
            &checked.module,
            vec![invalid_program(checked, &program, &spans, &origins, &error)],
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
    /// The `dtcs` of a `diag.read_dtc` result (subset 3.4, ADR-323): the response's DTC records
    /// as bytes, read by `len` and by an `int` index.
    DtcList,
    /// One DTC record of a `DtcList`: its bytes, read by the literal keys `code` and `status`.
    Dtc,
    /// The answer of `diag.hmi` or `diag.record` (subset 3.4, ADR-340): a status byte and the
    /// answer's text, read only by the literal keys `ok`, `timed_out` and `value`.
    HmiAnswer,
}

const TYS: [Ty; 10] = [
    Ty::Int,
    Ty::Float,
    Ty::Bool,
    Ty::Bytes,
    Ty::Str,
    Ty::Response,
    Ty::Fields,
    Ty::DtcList,
    Ty::Dtc,
    Ty::HmiAnswer,
];

impl Ty {
    fn bit(self) -> u16 {
        match self {
            Ty::Int => 1,
            Ty::Float => 2,
            Ty::Bool => 4,
            Ty::Bytes => 8,
            Ty::Str => 16,
            Ty::Response => 32,
            Ty::Fields => 64,
            Ty::DtcList => 128,
            Ty::Dtc => 256,
            Ty::HmiAnswer => 512,
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
            Ty::DtcList => "DTC records",
            Ty::Dtc => "DTC record",
            Ty::HmiAnswer => "HMI answer",
        }
    }
}

/// The types a value may have at run time, inferred at ingestion. Empty until something is
/// known about it. A response, and its fields, also carry the declared service they come from,
/// which decides what a field name means (ADR-320).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tys {
    bits: u16,
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
    /// A `diag.security_access` result, which has no `fields` (ADR-314 item 4).
    Key,
    /// A `diag.flash_transfer` result, the RequestTransferExit response, which has no `fields`
    /// (ADR-336 item 10).
    Exit,
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
            (Service::Key, Service::Key) => Service::Key,
            (Service::Exit, Service::Exit) => Service::Exit,
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
    /// A diagnostic primitive (`Op::is_diagnostic_primitive`) with the idempotency of the `diag`
    /// call it comes from, which the assembly turns into a one-instruction `Section` (ADR-332).
    /// A copy of a function's code carries it with the instruction.
    Primitive(Op, Idempotency),
    Jump(Label),
    JumpIfFalse(Label),
    Call(usize),
    Mark(Label),
    /// The first instruction after it begins the expansion of the flash plan in function `.0`
    /// (ADR-336 items 2 and 4); no instruction of its own.
    PlanBegin(usize),
    /// The pc after the last instruction of the plan's expansion; no instruction of its own.
    PlanEnd,
    /// The instruction after it is the boundary of the plan being expanded; no instruction of its
    /// own.
    Boundary(Boundary),
    /// The instructions up to the matching [`Ins::SectionEnd`] are the in-place expansion of the
    /// section function `.0` inside a flash plan (ADR-336 item 3); no instruction of its own.
    SectionBegin(usize),
    /// The end of the expansion a [`Ins::SectionBegin`] opened.
    SectionEnd,
}

/// Which boundary of a flash plan the next instruction is (ADR-336 item 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// The erase point: the erase `RoutineControl`, or the RequestDownload without an erase step.
    Erase,
    /// The first instruction of the statement marked `# @recoveryRequired`.
    RecoveryRequired,
    /// The RequestTransferExit.
    TransferExit,
}

impl Ins {
    /// Whether the entry stands for no instruction of the program: a label or a plan marker.
    fn is_pseudo(&self) -> bool {
        matches!(
            self,
            Ins::Mark(_)
                | Ins::PlanBegin(_)
                | Ins::PlanEnd
                | Ins::Boundary(_)
                | Ins::SectionBegin(_)
                | Ins::SectionEnd
        )
    }
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
    /// The attributes of a section function (4.5): its interruptibility and expected duration.
    section: Option<(Interruptible, u32)>,
    /// The 1-based line of the `def`, which the annotations name a function by.
    line: u32,
}

/// What the lowering knows of a flash plan once the variant's checks passed (4.9, ADR-336).
struct PlanInfo {
    /// The flash session the plan drives, and its stage (the stage is the session id).
    session: u32,
    max_resumes: u16,
    recovery_never: bool,
    /// Whether the session has an erase-memory step, so that `@erase` marks the erase point.
    erases: bool,
    /// Where the statement of `@erase` and that of `@recoveryRequired` begin (byte offsets).
    erase_at: Option<u32>,
    recovery_at: Option<u32>,
    timing: RecoveryTiming,
    version_read_retries: u8,
    no_application: Option<NoApplication>,
}

/// One copy of a function in the program (ADR-301): the function, and the section function
/// whose call it runs in, or `None` for the copy that runs outside every section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FuncCopy {
    func: usize,
    section: Option<usize>,
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
    /// The copies of the functions the program holds, in layout order (ADR-301).
    copies: Vec<FuncCopy>,
    /// `@idempotency` values by the byte offset where their statement begins (4.6, ADR-332).
    idempotency: HashMap<u32, Idempotency>,
    /// The annotated statements being lowered, innermost last: the value, and whether the
    /// statement has lowered to a diagnostic primitive yet. Calls to functions are not lowered
    /// inline, so a statement's frame never covers a called function's code.
    annotated: Vec<(Idempotency, bool)>,
    /// How many errors were reported or repeated, which `error` dedupes, outside inference.
    attempted: usize,
    /// The preconditions `# @preconditions` names, from the variant's table (4.9).
    preconditions: Preconditions,
    /// The flash plans, by the function that holds each (4.9).
    plans: HashMap<usize, PlanInfo>,
    /// The plan whose body is being expanded in place, if any: the functions it calls are
    /// expanded in place too ([`Lowerer::inline_call`]), its `diag.request`s default to
    /// `CheckState` (4.6) and it holds the one `diag.flash_transfer` (4.9).
    active_plan: Option<usize>,
    /// The functions being expanded in place, outermost first, so a call chain that closes on one
    /// of them cannot be expanded.
    inline_stack: Vec<usize>,
    /// The in-place expansions of functions made, and the instructions they hold, in the current
    /// pass over the function bodies: they count toward the copy limits with the sections' copies
    /// (subset 2.6, 4.5).
    inline_copies: usize,
    inline_size: usize,
    /// The instruction budget of [`Lowerer::inline_size`] and of the sections' copies; tests
    /// lower with a smaller one.
    max_copied: usize,
    /// How many run-time loops (`range`) enclose what is being lowered, the loops of the
    /// callers of an expansion included. An expansion inside one runs more than once.
    run_loops: u32,
    /// The first local each function may read before every path assigns it, if any, by
    /// function (4.9).
    maybe_unset: HashMap<usize, Option<String>>,
    /// Where an inference pass first found those limits passed, which the emission reports.
    inline_limit: Option<Span>,
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
            copies: Vec::new(),
            idempotency: checked
                .annotations
                .iter()
                .filter_map(|annotation| match annotation {
                    Annotation::Idempotency {
                        statement_at,
                        value,
                        ..
                    } => Some((
                        *statement_at,
                        match value {
                            super::Idempotency::Safe => Idempotency::Safe,
                            super::Idempotency::CheckState => Idempotency::CheckState,
                            super::Idempotency::Unsafe => Idempotency::Unsafe,
                        },
                    )),
                    _ => None,
                })
                .collect(),
            annotated: Vec::new(),
            attempted: 0,
            preconditions: Preconditions::default(),
            plans: HashMap::new(),
            active_plan: None,
            inline_stack: Vec::new(),
            inline_copies: 0,
            inline_size: 0,
            max_copied: MAX_COPIED_INSTRUCTIONS,
            run_loops: 0,
            maybe_unset: HashMap::new(),
            inline_limit: None,
        }
    }

    /// Records an error once per code and place: an unrolled body is lowered once per item and
    /// reports the same problem at the same place each time.
    fn error(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        if self.quiet == 0 {
            self.attempted += 1;
        }
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

    /// The preconditions `# @preconditions` names, taken from the variant's `preconditions.csv`;
    /// a name without a row is `STAR_UNKNOWN_PRECONDITION` (4.9, ADR-336 item 6). Without the
    /// annotation the program declares none.
    fn preconditions(&mut self) {
        let table = &self.requests.preconditions;
        let mut named = Preconditions::default();
        let mut missing = Vec::new();
        for annotation in &self.checked.annotations {
            let Annotation::Preconditions { span, kinds, .. } = annotation else {
                continue;
            };
            let mut absent = Vec::new();
            for &kind in kinds {
                let (row, slot, name) = match kind {
                    PreconditionKind::Voltage => {
                        (&table.voltage_mv, &mut named.voltage_mv, "voltage")
                    }
                    PreconditionKind::ExternalSupply => (
                        &table.external_supply,
                        &mut named.external_supply,
                        "external_supply",
                    ),
                    PreconditionKind::Ignition => {
                        (&table.ignition, &mut named.ignition, "ignition")
                    }
                    PreconditionKind::Engine => (&table.engine, &mut named.engine, "engine"),
                    PreconditionKind::VehicleSpeed => (
                        &table.vehicle_speed,
                        &mut named.vehicle_speed,
                        "vehicle_speed",
                    ),
                };
                match row {
                    Some(row) => *slot = Some(row.clone()),
                    None => absent.push(name),
                }
            }
            if !absent.is_empty() {
                missing.push((*span, absent));
            }
        }
        self.preconditions = named;
        for (span, absent) in missing {
            self.error(
                StarCode::UnknownPrecondition,
                span,
                format!(
                    "the variant's preconditions.csv has no row of kind {} (4.9)",
                    absent.join(", ")
                ),
            );
        }
    }

    /// What a flash plan needs of the variant (4.9, ADR-336 items 1, 2, 4 and 9): a UDS
    /// application layer, a flash session of the plan's id with the recovery facts of its
    /// `flash_sessions.csv` row, an `@erase` exactly when that session's steps erase memory, and,
    /// for a plan that allows a restart, all three `identity.csv` rows. It also records what the
    /// expansion of each plan that passes needs ([`PlanInfo`]).
    fn flash_plans(&mut self) {
        let checked = self.checked;
        let identity = &self.requests.identity;
        let absent: Vec<&str> = [
            ("vin", identity.vin.is_none()),
            (
                "hardware_part_number",
                identity.hardware_part_number.is_none(),
            ),
            ("software_version", identity.software_version.is_none()),
        ]
        .into_iter()
        .filter(|(_, absent)| *absent)
        .map(|(name, _)| name)
        .collect();
        let uds = self.requests.profile.layer == UDS;
        // What each plan's annotations say, by the line of its `def`, and each session of the
        // variant by id, taken once so the work is linear in plans, annotations and sessions.
        let mut first_erase: HashMap<u32, (Span, u32)> = HashMap::new();
        let mut on_site_from_entry: HashSet<u32> = HashSet::new();
        let mut last_marker: HashMap<u32, u32> = HashMap::new();
        for annotation in &checked.annotations {
            match annotation {
                Annotation::Erase {
                    def,
                    span,
                    statement_at,
                    ..
                } => {
                    first_erase.entry(*def).or_insert((*span, *statement_at));
                }
                Annotation::Section {
                    def,
                    kind: SectionKind::RecoveryRequired,
                    ..
                } => {
                    on_site_from_entry.insert(*def);
                }
                Annotation::RecoveryRequired {
                    def, statement_at, ..
                } => {
                    let last = last_marker.entry(*def).or_insert(*statement_at);
                    *last = (*last).max(*statement_at);
                }
                _ => {}
            }
        }
        let mut erasing: HashMap<u32, bool> = HashMap::new();
        for flash in self.variant.flash_sessions().into_iter().flatten() {
            erasing.entry(flash.id()).or_insert_with(|| {
                flash
                    .steps()
                    .into_iter()
                    .flatten()
                    .any(|step| step == FlashStep::EraseMemory)
            });
        }
        let mut transfers: Option<HashMap<u32, u32>> = None;
        let mut errors = Vec::new();
        let mut plans = Vec::new();
        let func_at: HashMap<u32, usize> = self
            .funcs
            .iter()
            .enumerate()
            .map(|(at, func)| (func.line, at))
            .collect();
        for annotation in &checked.annotations {
            let Annotation::FlashPlan {
                def,
                span,
                session,
                max_resumes,
                recovery_never,
                ..
            } = annotation
            else {
                continue;
            };
            if !uds {
                errors.push((
                    StarCode::NotInProfile,
                    *span,
                    format!(
                        "the {} profile of this variant's protocols has no flash plan: only UDS \
                         has one (4.9)",
                        self.requests.profile.layer
                    ),
                ));
                continue;
            }
            let Some(&erases) = erasing.get(session) else {
                errors.push((
                    StarCode::UnknownFlashSession,
                    *span,
                    format!("the variant declares no flash session {session} (4.9)"),
                ));
                continue;
            };
            let erase = first_erase.get(def).map(|(span, _)| *span);
            match (erases, erase) {
                (true, None) => errors.push((
                    StarCode::PlanShape,
                    *span,
                    format!(
                        "flash session {session} has an erase-memory step, so the plan needs \
                         an `@erase` before its `diag.routine` that erases (4.9)"
                    ),
                )),
                (false, Some(erase)) => errors.push((
                    StarCode::PlanShape,
                    erase,
                    format!(
                        "flash session {session} has no erase-memory step, so the plan has no \
                         `@erase` (4.9)"
                    ),
                )),
                _ => {}
            }
            // What the plan's expansion takes from the session's row (`csv-tables.md` 4.12).
            match self.requests.flash.iter().find(|f| f.session == *session) {
                Some(&FlashFacts {
                    timing: Some(timing),
                    version_read_retries: Some(version_read_retries),
                    no_application_nrc,
                    ..
                }) => {
                    if let Some(&func) = func_at.get(def) {
                        plans.push((
                            func,
                            PlanInfo {
                                session: *session,
                                max_resumes: u16::try_from(*max_resumes)
                                    .expect("the checker bounds maxResumes by 65535"),
                                recovery_never: *recovery_never,
                                erases,
                                erase_at: first_erase.get(def).map(|(_, at)| *at),
                                recovery_at: last_marker.get(def).copied(),
                                timing,
                                version_read_retries,
                                no_application: no_application_nrc.map(NoApplication::Nrc),
                            },
                        ));
                    }
                }
                _ => errors.push((
                    StarCode::PlanShape,
                    *span,
                    format!(
                        "flash session {session}'s row in flash_sessions.csv lacks a recovery \
                         fact a plan needs: session_timeout_ms, teardown_margin_ms, \
                         ecu_startup_ms, confirmation_window_ms and version_read_retries (4.9)"
                    ),
                )),
            }
            // The erase point: the `@erase` statement, or without an erase step the transfer's
            // statement, whose expansion begins with the RequestDownload. A marker moves the
            // point to a restartable place only strictly after it (`allows_restart`).
            let point = if erases {
                first_erase.get(def).map(|(_, at)| *at)
            } else {
                let transfers = transfers.get_or_insert_with(|| self.transfer_statements());
                transfers.get(def).copied()
            };
            let moved = point
                .zip(last_marker.get(def))
                .is_some_and(|(point, last)| *last > point);
            if !on_site_from_entry.contains(def) && (*recovery_never || moved) && !absent.is_empty()
            {
                errors.push((
                    StarCode::PlanShape,
                    *span,
                    format!(
                        "a plan that allows a restart needs the vin, hardware_part_number and \
                         software_version rows of the variant's identity.csv; missing: {} (4.9)",
                        absent.join(", ")
                    ),
                ));
            }
        }
        for (code, span, hint) in errors {
            self.error(code, span, hint);
        }
        self.plans.extend(plans);
    }

    /// For each top-level `def`, by its line, where the top-level statement that holds its
    /// `diag.flash_transfer` begins.
    fn transfer_statements(&self) -> HashMap<u32, u32> {
        let codemap = self
            .checked
            .module
            .file_span(self.checked.module.statement().span)
            .file;
        self.top_level()
            .into_iter()
            .filter_map(|stmt| {
                let StmtP::Def(plan) = &stmt.node else {
                    return None;
                };
                let at = super::Checker::statements(&plan.body)
                    .into_iter()
                    .find(|inner| {
                        super::direct_expr(inner)
                            .is_some_and(|e| super::is_diag_call(e, "flash_transfer").is_some())
                    })?
                    .span
                    .begin()
                    .get();
                Some((codemap.find_line(stmt.span.begin()) as u32 + 1, at))
            })
            .collect()
    }

    /// Numbers the top-level `def`s and gives each its slots and, for a section function, its
    /// section's attributes (4.5).
    fn functions(&mut self) {
        let codemap = self
            .checked
            .module
            .file_span(self.checked.module.statement().span)
            .file;
        // The checker attaches each `@section` to the top-level `def` on its `def` line.
        let sections: HashMap<u32, (Interruptible, u32)> = self
            .checked
            .annotations
            .iter()
            .filter_map(|annotation| match annotation {
                Annotation::Section {
                    def,
                    kind,
                    expected_millis,
                    ..
                } => Some((
                    *def,
                    (
                        match kind {
                            SectionKind::Interruptible => Interruptible::Yes,
                            SectionKind::Uninterruptible => Interruptible::No,
                            SectionKind::RecoveryRequired => Interruptible::RecoveryRequired,
                        },
                        expected_millis.unwrap_or(0),
                    ),
                )),
                _ => None,
            })
            .collect();
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
            let line = codemap.find_line(stmt.span.begin()) as u32 + 1;
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
                section: sections.get(&line).copied(),
                line,
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
            self.inline_copies = 0;
            self.inline_size = 0;
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
        self.inline_copies = 0;
        self.inline_size = 0;
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
        }
        self.plan_copies(main);
    }

    /// The copy a call to `callee` reaches from a copy that runs in `section`: a section
    /// function's call enters its own section, and any other function runs in the caller's.
    fn copy_for(&self, callee: usize, section: Option<usize>) -> FuncCopy {
        FuncCopy {
            func: callee,
            section: if self.funcs[callee].section.is_some() {
                Some(callee)
            } else {
                section
            },
        }
    }

    /// Lists the copies the program holds (ADR-301): starting from `main`,
    /// each call reaches the copy [`Lowerer::copy_for`] names, so every function a section
    /// reaches gets a copy that only that section runs, and a function also called outside every
    /// section keeps its own copy there. The copies a section needs count toward the budgets of
    /// 2.6, in copies and in instructions, before each is made; the call that would pass one is
    /// `STAR_SPECIALIZATION_LIMIT`.
    fn plan_copies(&mut self, main: usize) {
        let first = self.copy_for(main, None);
        let mut listed: HashSet<FuncCopy> = HashSet::from([first]);
        self.copies = vec![first];
        // The in-place expansions of the emission pass count with the copies (ADR-336 item 3).
        let mut specialized = usize::from(first.section.is_some()) + self.inline_copies;
        let mut copied = self.inline_size
            + if first.section.is_some() {
                self.funcs[main].code.len()
            } else {
                0
            };
        let mut next = 0;
        while next < self.copies.len() {
            let copy = self.copies[next];
            next += 1;
            let calls: Vec<(usize, Span)> = self.funcs[copy.func]
                .code
                .iter()
                .filter_map(|(ins, span)| match ins {
                    Ins::Call(callee) => Some((*callee, *span)),
                    _ => None,
                })
                .collect();
            for (callee, span) in calls {
                let target = self.copy_for(callee, copy.section);
                if copy.section.is_some() && target.section != copy.section {
                    // The checker rejects a section function called inside a section.
                    self.error(
                        StarCode::BadAnnotation,
                        span,
                        "a section function is called inside a section (4.5)",
                    );
                    continue;
                }
                if listed.contains(&target) {
                    continue;
                }
                if target.section.is_some() {
                    let size = self.funcs[callee].code.len();
                    if specialized == MAX_COPIES || copied + size > self.max_copied {
                        self.error(StarCode::SpecializationLimit, span, specialization_limit());
                        return;
                    }
                    specialized += 1;
                    copied += size;
                }
                listed.insert(target);
                self.copies.push(target);
            }
        }
    }

    /// Lowers one function: its entry label, its body, and the closing `Ret`.
    fn function(&mut self, id: usize) -> Code {
        let span = self.funcs[id].def.name.span;
        let mut code: Code = vec![(Ins::Mark(self.funcs[id].label), span)];
        self.body(id, &mut code);
        // Unreachable after a function that returns on every path, but it keeps every label a
        // jump can name inside the code.
        code.push((Ins::Op(Op::Ret), span));
        code
    }

    /// Lowers the body of function `id` into `code`, in the function's own frame: the stores of
    /// the parameters from the stack, then the statements, each `return` as a `Ret`. A call
    /// lowers it with an entry label and a closing `Ret` ([`Self::function`]); a flash plan's
    /// expansion lowers it in place ([`Self::inline_call`]).
    fn body(&mut self, id: usize, code: &mut Code) {
        let def = self.funcs[id].def;
        let span = def.name.span;
        self.funcs[id].temps = self.funcs[id].slots.len() as u16;
        // The caller pushed the arguments in order.
        for at in (0..self.funcs[id].params.len()).rev() {
            code.push((Ins::Op(Op::StoreLocal(at as u16)), span));
        }
        for (index, stmt) in super::Checker::statements(&def.body)
            .into_iter()
            .enumerate()
        {
            // An annotated docstring is lowered, so that the annotation is reported as one that
            // covers no primitive.
            if index == 0
                && is_string_expression(stmt)
                && !self.idempotency.contains_key(&stmt.span.begin().get())
            {
                continue;
            }
            self.stmt(id, stmt, code);
        }
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
    }

    // ------------------------------------------------------------ Statements

    /// Lowers a statement. One with an `@idempotency` annotation sets the idempotency of every
    /// primitive it lowers to, the nested statements of an `if` or `for` included, until a
    /// nested annotation takes over (4.6, ADR-332). It is `STAR_BAD_ANNOTATION` when the
    /// statement lowers to none, so an annotation never does nothing.
    fn stmt(&mut self, id: usize, stmt: &AstStmt, code: &mut Code) {
        // A list of statements begins where its first statement does, which holds the annotation.
        let value = match stmt.node {
            StmtP::Statements(_) => None,
            _ => self.idempotency.get(&stmt.span.begin().get()).copied(),
        };
        let Some(value) = value else {
            return self.statement(id, stmt, code);
        };
        let attempted = self.attempted;
        self.annotated.push((value, false));
        self.statement(id, stmt, code);
        let (_, lowered) = self.annotated.pop().expect("the frame pushed above");
        // Another error means the statement was not lowered in full, and a spent budget that
        // it was cut short: they are reported instead.
        if !lowered && self.attempted == attempted && self.exhausted.is_none() {
            self.error(
                StarCode::BadAnnotation,
                stmt.span,
                "`@idempotency` annotates a statement that lowers to no `diag` call, so it has \
                 nothing to apply to; the functions the statement calls keep their own (4.6)",
            );
        }
    }

    fn statement(&mut self, id: usize, stmt: &AstStmt, code: &mut Code) {
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
        self.run_loops += 1;
        self.stmt(id, body, code);
        self.run_loops -= 1;
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
                if let Some(args) = self.ecu_info_call(id, object) {
                    return self.call_diag_keyed(id, "ecu_info", &args, Some(index), span);
                }
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
                    Val::Run(tys, code) if tys.one() == Some(Ty::Dtc) => {
                        let literal = matches!(&index.node, ExprP::Literal(AstLiteral::String(_)));
                        let key = if literal {
                            b
                        } else {
                            Val::Run(Tys::default(), Vec::new())
                        };
                        self.dtc_key(id, code, key, span)
                    }
                    Val::Run(tys, code) if tys.one() == Some(Ty::HmiAnswer) => {
                        let literal = matches!(&index.node, ExprP::Literal(AstLiteral::String(_)));
                        let key = if literal {
                            b
                        } else {
                            Val::Run(Tys::default(), Vec::new())
                        };
                        self.hmi_key(code, key, span)
                    }
                    Val::Run(tys, code) if tys.one() == Some(Ty::DtcList) => {
                        self.dtc_at(id, code, b, span)
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
            Ty::DtcList | Ty::Dtc => {
                self.not_lowerable(
                    span,
                    "the truth of DTC records is not lowered; test `len(dtcs)` instead (3.4)",
                );
                return None;
            }
            Ty::HmiAnswer => {
                self.not_lowerable(
                    span,
                    "an HMI answer is always true; test `ans[\"ok\"]` instead (3.4)",
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
            .any(|v| matches!(v, Val::Run(tys, _) if [Ty::Response, Ty::Fields, Ty::DtcList, Ty::Dtc, Ty::HmiAnswer].into_iter().any(|ty| tys.has(ty))))
        {
            // Starlark would compare the dict, which a procedure that left out `["raw"]` gets
            // wrong silently (ADR-317 item 3).
            self.not_lowerable(
                span,
                format!(
                    "`{}` does not take a response, DTC records, a DTC record or an HMI \
                     answer: read it through its keys, such as `res[\"raw\"]`, \
                     `dtc[\"code\"]` or `ans[\"ok\"]`, or count the records with `len` (3.4)",
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
            // A Starlark integer grows rather than losing bits, so a result past 64 bits stops
            // the run, as the IR's checked shift does (4.4).
            (BinOp::LeftShift, Ty::Int, Ty::Int) => {
                code.push(at(Op::ShlChecked));
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
            Service::Key => {
                self.error(
                    StarCode::NotInProfile,
                    span,
                    format!(
                        "a `diag.security_access` result has no fields, so `{key}` is not one; \
                         its `raw` is the key response (3.4, ADR-314)"
                    ),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
            Service::Exit => {
                self.error(
                    StarCode::NotInProfile,
                    span,
                    format!(
                        "a `diag.flash_transfer` result has no fields, so `{key}` is not one; \
                         its `raw` is the RequestTransferExit response (3.4, ADR-336)"
                    ),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
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
        if profiled && &*key == "dtcs" {
            return self.dtc_records(code, service, span);
        }
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

    /// The layout of the DTC records the profile's `diag.read_dtc` reads (ADR-314 item 4):
    /// each record's code and status widths, and its size in bytes.
    fn dtc_layout(&self) -> DtcLayout {
        let map = self
            .requests
            .profile
            .call(Intent::ReadDtc)
            .and_then(|call| {
                call.steps.iter().find_map(|step| match step.result {
                    ResultMap::ReadDtc {
                        mask,
                        dtcs,
                        code_bits,
                        status_bits,
                    } => Some((mask, dtcs, code_bits, status_bits)),
                    _ => None,
                })
            });
        let Some((mask, dtcs, code_bits, status_bits)) = map else {
            unreachable!("only a `diag.read_dtc` the profile defines is lowered")
        };
        // `Profile::validate` makes both widths whole bytes.
        DtcLayout {
            mask: usize::from(mask.0),
            position: usize::from(dtcs.0),
            code_bytes: code_bits / 8,
            status_bytes: status_bits / 8,
            record_bytes: code_bits / 8 + status_bits / 8,
        }
    }

    /// `res["fields"]["dtcs"]` of a `diag.read_dtc` result from `service` (subset 3.4, ADR-323):
    /// `code` pushes the response bytes. The records are the bytes from the records field's
    /// position to the end of the message, which `dtc_response_fits` checked at ingestion to be
    /// a repeat of whole, unpadded records. The mask is decoded first and dropped: decoding
    /// checks the whole message against the declaration, so a negative response, one to another
    /// service, or one whose last record is cut short stops the run as any field read does
    /// (ADR-316). The records are then whole.
    fn dtc_records(&mut self, code: Code, service: u32, span: Span) -> Val {
        let layout = self.dtc_layout();
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
        // The fields before the records follow each other from the end of the prefix, each of a
        // fixed length (`dtc_response_fits`).
        let start_bits = fields[..layout.position]
            .iter()
            .fold((1 + prefix as u64) * 8, |at, field| {
                at + u64::from(field.bit_length())
            });
        let (Some(mask), true) = (fields.get(layout.mask), start_bits % 8 == 0) else {
            self.not_lowerable(
                span,
                "the DTC records do not start on a byte boundary, so they cannot be read (3.4)",
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let start = i64::try_from(start_bits / 8).unwrap_or(i64::MAX);
        let mask = mask.id();
        let mut code = code;
        code.extend(
            [
                Ins::Op(Op::Dup),
                Ins::Op(Op::DecodeField {
                    service,
                    field: mask,
                }),
                Ins::Op(Op::Pop),
                Ins::Op(Op::PushI64(start)),
                Ins::Op(Op::PushI64(i64::MAX)),
                Ins::Op(Op::Slice),
            ]
            .map(|ins| (ins, span)),
        );
        Val::Run(
            Tys {
                bits: Ty::DtcList.bit(),
                service: Service::Dtcs(service),
            },
            code,
        )
    }

    /// `dtcs[i]` (subset 3.4, ADR-323): `code` pushes the records; the record is the `i`-th
    /// record's bytes. An index outside the records, a negative one included, stops the run as
    /// an index out of range, as one into bytes does: the record's first byte is read before
    /// it is sliced. The records are whole (`dtc_records`), so a record that starts inside them
    /// ends inside them.
    fn dtc_at(&mut self, id: Option<usize>, code: Code, index: Val, span: Span) -> Val {
        let index = match self.single(index, span, "an index") {
            Some((Ty::Int, index)) => index,
            Some((other, _)) => {
                self.not_lowerable(
                    span,
                    format!(
                        "DTC records are indexed by an int, not {} (3.4)",
                        other.name()
                    ),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
            None => return Val::Run(Tys::default(), Vec::new()),
        };
        let Some(id) = id else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let size = i64::from(self.dtc_layout().record_bytes);
        let start = self.temp(id, span);
        let mut code = code;
        code.extend(index);
        code.extend(
            [
                Op::PushI64(size),
                Op::Mul,
                Op::StoreLocal(start),
                // `IndexGet` leaves the records below the byte, which only checks the start.
                Op::LoadLocal(start),
                Op::IndexGet,
                Op::Pop,
                Op::LoadLocal(start),
                Op::Dup,
                Op::PushI64(size),
                Op::Add,
                Op::Slice,
            ]
            .map(|op| (Ins::Op(op), span)),
        );
        Val::Run(Tys::of(Ty::Dtc), code)
    }

    /// `rec["code"]` or `rec["status"]` of a DTC record (subset 3.4, ADR-323): `code` pushes the
    /// record's bytes, and the field is read as a big-endian `uint` of the profile's width.
    fn dtc_key(&mut self, id: Option<usize>, code: Code, key: Val, span: Span) -> Val {
        let Val::Const(Const::Str(key)) = key else {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                span,
                "a DTC record is read with a string literal key (3.4, 4.1)",
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let layout = self.dtc_layout();
        let (offset, bytes) = match &*key {
            "code" => (0, layout.code_bytes),
            "status" => (layout.code_bytes, layout.status_bytes),
            other => {
                self.error(
                    StarCode::NotInProfile,
                    span,
                    format!("a DTC record has `code` and `status`, not `{other}` (3.4)"),
                );
                return Val::Run(Tys::default(), Vec::new());
            }
        };
        let Some(id) = id else {
            return Val::Run(Tys::default(), Vec::new());
        };
        let record = self.temp(id, span);
        let mut code = code;
        code.push((Ins::Op(Op::StoreLocal(record)), span));
        code.push((Ins::Op(Op::PushI64(0)), span));
        for byte in offset..offset + bytes {
            code.extend(
                [
                    Op::PushI64(8),
                    Op::Shl,
                    Op::LoadLocal(record),
                    Op::PushI64(i64::from(byte)),
                    Op::IndexGet,
                    Op::Swap,
                    Op::Pop,
                    Op::BitOr,
                ]
                .map(|op| (Ins::Op(op), span)),
            );
        }
        // The slot would otherwise keep a copy of the record in the VM state until the function
        // returns.
        code.extend([Op::PushI64(0), Op::StoreLocal(record)].map(|op| (Ins::Op(op), span)));
        Val::Run(Tys::of(Ty::Int), code)
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

    /// A built-in of 2.4 on values known only at run time: `len` of bytes, `int` of a number and
    /// `float` of a number, through the IR's conversions, and `read_uint` and `write_uint`. The
    /// others are rejected with the instruction they wait for.
    fn run_builtin(&mut self, id: Option<usize>, name: &str, values: Vec<Val>, span: Span) -> Val {
        let failed = || Val::Run(Tys::default(), Vec::new());
        if let "read_uint" | "write_uint" = name {
            return self.run_uint(id, name, values, span);
        }
        if let ("len", [Val::Run(tys, _)]) = (name, values.as_slice())
            && tys.one() == Some(Ty::DtcList)
        {
            let Some(Val::Run(_, mut code)) = values.into_iter().next() else {
                unreachable!("matched above")
            };
            // The records are whole (`dtc_records`), so their number is their length over a
            // record's size. `Length` leaves the bytes below the length.
            let size = i64::from(self.dtc_layout().record_bytes);
            code.extend(
                [Op::Length, Op::Swap, Op::Pop, Op::PushI64(size), Op::Div]
                    .map(|op| (Ins::Op(op), span)),
            );
            return Val::Run(Tys::of(Ty::Int), code);
        }
        if let ("len", [_]) = (name, values.as_slice()) {
            let value = values.into_iter().next().expect("one argument");
            let Some((ty, mut code)) = self.single(value, span, "`len`") else {
                return failed();
            };
            if ty == Ty::Bytes {
                // `Length` leaves the bytes below the length.
                code.extend([Op::Length, Op::Swap, Op::Pop].map(|op| (Ins::Op(op), span)));
                return Val::Run(Tys::of(Ty::Int), code);
            }
            self.not_lowerable(
                span,
                format!(
                    "`len` of a {} known only at run time {} (2.4)",
                    ty.name(),
                    waits_for(name)
                ),
            );
            return failed();
        }
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
                            "`{name}` of a {} known only at run time {} (2.4)",
                            ty.name(),
                            waits_for(name)
                        ),
                    );
                    return failed();
                }
            };
            return Val::Run(Tys::of(result), code);
        }
        self.not_lowerable(
            span,
            format!(
                "`{name}` on a value known only at run time {} (2.4)",
                waits_for(name)
            ),
        );
        failed()
    }

    /// The arguments of `read_uint` and `write_uint` that do not depend on the operands (2.4):
    /// their number, and that `bits` is a constant multiple of 8 from 8 to 56. Other built-ins
    /// pass. Reports an error and gives `false` otherwise.
    fn check_uint_bits(&mut self, name: &str, values: &[Val], span: Span) -> bool {
        let arguments = match name {
            "read_uint" => 3,
            "write_uint" => 4,
            _ => return true,
        };
        if values.len() != arguments {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                format!(
                    "`{name}` takes {arguments} arguments, not {} (2.4)",
                    values.len()
                ),
            );
            return false;
        }
        match &values[2] {
            Val::Const(Const::Int(bits)) if (8..=56).contains(bits) && bits % 8 == 0 => true,
            Val::Const(_) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    span,
                    format!("`{name}` takes `bits` as a multiple of 8 from 8 to 56 (2.4)"),
                );
                false
            }
            Val::Run(..) | Val::Void(_) => {
                self.error(
                    StarCode::NonConstantArgument,
                    span,
                    format!("`bits` of `{name}` is a constant, so the bytes can be unrolled (2.4)"),
                );
                false
            }
        }
    }

    /// `read_uint(b, offset, bits)` and `write_uint(b, offset, bits, v)` on operands known only
    /// at run time (2.4): `bits` is a constant (checked by the caller), so the bytes are read or
    /// written by code unrolled once per byte, most significant first. A byte past the end of `b`
    /// is the VM's `IndexOutOfRange` from `IndexGet` or `IndexSet`, which a negative offset also
    /// gives; a `v` of `write_uint` outside 0 to 2^`bits` - 1 ends the procedure with `Fail`
    /// code `UINT_OUT_OF_RANGE`.
    fn run_uint(&mut self, id: Option<usize>, name: &str, values: Vec<Val>, span: Span) -> Val {
        let failed = || Val::Run(Tys::default(), Vec::new());
        let Some(id) = id else {
            self.not_lowerable(
                span,
                format!("`{name}` needs a function to hold its operands"),
            );
            return failed();
        };
        let Some(Val::Const(Const::Int(bits))) = values.get(2) else {
            unreachable!("`bits` is checked to be a constant before lowering")
        };
        let (bits, bytes) = (*bits, *bits / 8);
        let write = name == "write_uint";
        // The operands, as the types they must have.
        let mut operands = Vec::new();
        for (at, value) in values.into_iter().enumerate() {
            if at == 2 {
                continue;
            }
            let want = if at == 0 { Ty::Bytes } else { Ty::Int };
            let Some((ty, code)) = self.single(value, span, &format!("`{name}`")) else {
                return failed();
            };
            if ty != want {
                self.not_lowerable(
                    span,
                    format!(
                        "an operand of `{name}` must be {}, not {} (2.4)",
                        want.name(),
                        ty.name()
                    ),
                );
                return failed();
            }
            operands.push(code);
        }
        let slots: Vec<u16> = operands.iter().map(|_| self.temp(id, span)).collect();
        let mut code: Code = Vec::new();
        for (operand, &slot) in operands.into_iter().zip(&slots) {
            code.extend(operand);
            code.push((Ins::Op(Op::StoreLocal(slot)), span));
        }
        let (bytes_slot, offset_slot) = (slots[0], slots[1]);
        let mut ops = Vec::new();
        // The offset of byte `i`, which `IndexGet` and `IndexSet` check.
        let at = |i: i64| {
            let mut ops = vec![Op::LoadLocal(offset_slot)];
            if i > 0 {
                ops.extend([Op::PushI64(i), Op::Add]);
            }
            ops
        };
        if write {
            let value_slot = slots[2];
            // `Shr` is logical, so a negative value leaves its high bits set as well.
            let ok = self.label();
            code.extend(
                [
                    Ins::Op(Op::LoadLocal(value_slot)),
                    Ins::Op(Op::PushI64(bits)),
                    Ins::Op(Op::Shr),
                    Ins::Op(Op::PushI64(0)),
                    Ins::Op(Op::CmpEq),
                    Ins::Op(Op::Not),
                    Ins::JumpIfFalse(ok),
                ]
                .map(|ins| (ins, span)),
            );
            let failure = self.pool(UINT_OUT_OF_RANGE.as_bytes());
            let detail = self.pool(
                format!(
                    "`write_uint` on line {} was given a value that does not fit {bits} bits",
                    self.line_of(span)
                )
                .as_bytes(),
            );
            code.extend(
                [
                    Ins::Op(Op::PushBytes(detail)),
                    Ins::Op(Op::Fail { code: failure }),
                    Ins::Mark(ok),
                ]
                .map(|ins| (ins, span)),
            );
            ops.push(Op::LoadLocal(bytes_slot));
            for i in 0..bytes {
                ops.extend(at(i));
                ops.extend([
                    Op::LoadLocal(value_slot),
                    Op::PushI64(8 * (bytes - 1 - i)),
                    Op::Shr,
                    Op::PushI64(0xff),
                    Op::BitAnd,
                    // Leaves the bytes with the byte written.
                    Op::IndexSet,
                ]);
            }
        } else {
            ops.push(Op::PushI64(0));
            for i in 0..bytes {
                if i > 0 {
                    ops.extend([Op::PushI64(8), Op::Shl]);
                }
                ops.push(Op::LoadLocal(bytes_slot));
                ops.extend(at(i));
                // `IndexGet` leaves the bytes below the byte read.
                ops.extend([Op::IndexGet, Op::Swap, Op::Pop, Op::BitOr]);
            }
        }
        // The slot would otherwise keep a copy of the bytes in the VM state until the function
        // returns.
        ops.extend([Op::PushI64(0), Op::StoreLocal(bytes_slot)]);
        code.extend(ops.into_iter().map(|op| (Ins::Op(op), span)));
        Val::Run(Tys::of(if write { Ty::Bytes } else { Ty::Int }), code)
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
                    if !self.check_uint_bits(name, &values, span) {
                        return Val::Run(Tys::default(), Vec::new());
                    }
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
                        None => self.run_builtin(id, name, values_copy, span),
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
                                "`.{}` on a value known only at run time {} (2.4)",
                                method.node,
                                waits_for(&method.node)
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
        if self.plans.contains_key(&callee) {
            return self.expand_plan(id, callee, args, span);
        }
        if self.active_plan.is_some() {
            return self.inline_call(id, callee, args, span);
        }
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

    /// A call inside a flash plan's expansion (4.9, ADR-336 item 3): the callee is lowered in
    /// place instead of called, so the plan holds no `Call` or `Ret` (ADR-245 item 4). The
    /// arguments are evaluated in the caller and left on the stack, the callee's body stores them
    /// into its parameters, and each `return` of the body jumps to the join after it, with the
    /// value on the stack where a call would leave it. The callee's locals and temporaries take
    /// fresh slots in the caller's frame after the ones it has used so far, so a function called
    /// twice has two sets. A section function's expansion is bracketed by
    /// [`Ins::SectionBegin`] and [`Ins::SectionEnd`], which the assembly turns into the section's
    /// pieces over the range, entered by fallthrough. What a statement around the call says does
    /// not reach in: its `@idempotency`, its loops' `break` and `continue`, and the constants an
    /// unrolled loop of the caller binds. A call chain that closes on a function being expanded
    /// is recursion, which the checker rejects already (4.1). An expansion inside a run-time
    /// loop runs more than once with the locals of the pass before, so a callee that may read a
    /// local before assigning it is refused there ([`Self::maybe_unset_local`]).
    fn inline_call(
        &mut self,
        id: Option<usize>,
        callee: usize,
        args: &[&AstExpr],
        span: Span,
    ) -> Val {
        let failed = || Val::Run(Tys::default(), Vec::new());
        let Some(caller) = id else {
            self.not_lowerable(
                span,
                "a flash plan's calls are expanded in a function (4.9)",
            );
            return failed();
        };
        if self.active_plan == Some(callee) || self.inline_stack.contains(&callee) {
            self.not_lowerable(
                span,
                format!(
                    "`{}` calls itself, directly or not, and a function a flash plan calls is \
                     expanded in place, which a recursive call cannot be (4.1, 4.9)",
                    self.funcs[callee].name
                ),
            );
            return failed();
        }
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
            return failed();
        }
        let mut code = Vec::new();
        for (at, arg) in args.iter().enumerate() {
            let value = self.expr(id, arg);
            let Some((tys, more)) = self.force(value, arg.span) else {
                return failed();
            };
            code.extend(more);
            self.widen_slot(callee, at as u16, tys);
        }
        if self.run_loops > 0
            && let Some(name) = self.maybe_unset_local(callee)
        {
            self.not_lowerable(
                span,
                format!(
                    "a function a flash plan calls inside a loop reads `{name}` before every path \
                     assigns it; a call would fail there, and the plan's expansion would read the \
                     previous pass's value (4.9)"
                ),
            );
            return failed();
        }
        // The copies of the sections and the expansions share the budgets of 2.6, checked before
        // the body is lowered so that a chain of calls that doubles at each level stops early.
        if self.inline_copies >= MAX_COPIES || self.inline_size > self.max_copied {
            self.inline_limit.get_or_insert(span);
            self.error(StarCode::SpecializationLimit, span, specialization_limit());
            return failed();
        }
        self.inline_copies += 1;
        let annotated = std::mem::take(&mut self.annotated);
        let loops = std::mem::take(&mut self.loops);
        let bound = std::mem::take(&mut self.bound);
        self.inline_stack.push(callee);
        let (before, mut body): (usize, Code) = (self.inline_size, Vec::new());
        self.body(callee, &mut body);
        self.inline_stack.pop();
        self.annotated = annotated;
        self.loops = loops;
        self.bound = bound;
        // The body holds the expansions nested in it, which charged themselves already: this
        // expansion adds only its own instructions, so each is counted once however deep the
        // chain of calls.
        let nested = self.inline_size - before;
        let held = body.iter().filter(|(ins, _)| !ins.is_pseudo()).count();
        self.inline_size += held.saturating_sub(nested);
        if self.inline_size > self.max_copied {
            self.inline_limit.get_or_insert(span);
            self.error(StarCode::SpecializationLimit, span, specialization_limit());
            return failed();
        }
        // The callee's frame moves into the caller's, after the slots the caller has used.
        let (base, size) = (self.funcs[caller].temps, self.funcs[callee].temps);
        let Some(end) = base.checked_add(size) else {
            self.not_lowerable(
                span,
                "the locals and temporaries of a function a flash plan calls do not fit what is \
                 left of the caller's 65,535 locals (4.9)",
            );
            return failed();
        };
        let frame = &mut self.funcs[caller];
        frame.temps = end;
        while frame.slot_tys.len() < usize::from(end) {
            frame.slot_tys.push(Tys::of(Ty::Int));
        }
        let join = self.label();
        for (ins, _) in &mut body {
            match ins {
                Ins::Op(Op::LoadLocal(slot) | Op::StoreLocal(slot)) => *slot += base,
                Ins::Op(Op::Ret) => *ins = Ins::Jump(join),
                _ => {}
            }
        }
        let section = self.funcs[callee].section.is_some();
        if section {
            code.push((Ins::SectionBegin(callee), span));
        }
        code.extend(body);
        code.push((Ins::Mark(join), span));
        if section {
            code.push((Ins::SectionEnd, span));
        }
        if self.funcs[callee].returns_value {
            Val::Run(self.funcs[callee].ret, code)
        } else {
            Val::Void(code)
        }
    }

    /// The first local of `callee`, or of a function it calls, that a path of its body may read
    /// before the path assigns it (4.9), if any. A call fails at such a read ("undefined
    /// variable"), but an expansion's slots keep the value of the pass before.
    fn maybe_unset_local(&mut self, callee: usize) -> Option<String> {
        if let Some(known) = self.maybe_unset.get(&callee) {
            return known.clone();
        }
        let func = &self.funcs[callee];
        let mut unset = Unset {
            locals: &func.slots,
            globals: &self.globals,
            found: None,
            continues: Vec::new(),
            breaks: 0,
        };
        let mut state = Some(func.params.iter().cloned().collect::<HashSet<String>>());
        unset.stmt(&func.def.body, &mut state);
        let found = unset.found;
        self.maybe_unset.insert(callee, found.clone());
        found
    }

    /// The call of the flash plan `plan` in `main` (4.9, ADR-336 items 2 and 4): its body is
    /// lowered in place instead of called, between [`Ins::PlanBegin`] and [`Ins::PlanEnd`], with
    /// markers at the statements of `@erase` and `@recoveryRequired`. The plan has no frame of its
    /// own: its locals and temporaries take fresh slots in `main`'s frame, after the ones `main`
    /// has used so far. The statement the call stands in does not annotate what the plan holds,
    /// and a loop around it does not reach into it.
    fn expand_plan(
        &mut self,
        id: Option<usize>,
        plan: usize,
        args: &[&AstExpr],
        span: Span,
    ) -> Val {
        let none = || Val::Void(Vec::new());
        let main = self.by_name.get("main").copied();
        if id.is_none() || id != main {
            self.not_lowerable(
                span,
                "a flash plan is called from `main` only, where it is expanded (4.9)",
            );
            return none();
        }
        let main = id.expect("checked above");
        if !args.is_empty() || !self.funcs[plan].params.is_empty() {
            self.not_lowerable(
                span,
                "a flash plan that takes parameters is not lowered yet: its call passes none (4.9)",
            );
            return none();
        }
        let (erase_at, recovery_at) = {
            let info = &self.plans[&plan];
            (info.erase_at, info.recovery_at)
        };
        let def = self.funcs[plan].def;
        self.funcs[plan].temps = self.funcs[plan].slots.len() as u16;
        let annotated = std::mem::take(&mut self.annotated);
        let loops = std::mem::take(&mut self.loops);
        self.active_plan = Some(plan);
        let mut body: Code = vec![(Ins::PlanBegin(plan), span)];
        for (index, stmt) in super::Checker::statements(&def.body)
            .into_iter()
            .enumerate()
        {
            if index == 0
                && is_string_expression(stmt)
                && !self.idempotency.contains_key(&stmt.span.begin().get())
            {
                if recovery_at == Some(stmt.span.begin().get()) {
                    self.no_instruction_to_mark(stmt.span);
                }
                continue;
            }
            let mut code = Vec::new();
            self.stmt(plan, stmt, &mut code);
            let at = stmt.span.begin().get();
            if erase_at == Some(at) {
                // The statement is the one `diag.routine`, whose `RoutineControl` is the erase.
                if let Some(erase) = code.iter().position(|(ins, _)| {
                    matches!(ins, Ins::Primitive(Op::RoutineControl { .. }, _))
                }) {
                    code.insert(erase, (Ins::Boundary(Boundary::Erase), stmt.span));
                }
            }
            if recovery_at == Some(at) {
                if code.iter().any(|(ins, _)| !ins.is_pseudo()) {
                    code.insert(0, (Ins::Boundary(Boundary::RecoveryRequired), stmt.span));
                } else {
                    self.no_instruction_to_mark(stmt.span);
                }
            }
            body.extend(code);
        }
        self.active_plan = None;
        self.annotated = annotated;
        self.loops = loops;
        body.push((Ins::PlanEnd, span));
        // Move the plan's frame into `main`'s.
        let size = self.funcs[plan].temps;
        let base = self.funcs[main].temps;
        let Some(end) = base.checked_add(size) else {
            self.not_lowerable(
                span,
                "a flash plan's locals and temporaries do not fit what is left of `main`'s \
                 65,535 locals (4.9)",
            );
            return none();
        };
        let frame = &mut self.funcs[main];
        frame.temps = end;
        while frame.slot_tys.len() < usize::from(end) {
            frame.slot_tys.push(Tys::of(Ty::Int));
        }
        for (ins, _) in &mut body {
            if let Ins::Op(Op::LoadLocal(slot) | Op::StoreLocal(slot)) = ins {
                *slot += base;
            }
        }
        Val::Void(body)
    }

    /// `diag.flash_transfer(session)` inside a flash plan's expansion (subset 3.2, 4.9, ADR-336
    /// items 10, 11 and 13): the RequestDownload built from the session's one segment, a loop
    /// over its blocks that reads each with `FlashBlock` and sends it with `FlashTransfer`, and
    /// the RequestTransferExit, whose response is the call's value, without `fields`. The
    /// RequestDownload's response is dropped. The segment's format byte gives the widths of the
    /// address and the length in the request (ISO 14229-1 (2026) 14.2.2.3); a value that does not
    /// fit, a block size above what one `Bytes` value takes less the two bytes the
    /// TransferData request adds, and more blocks than the write-job journal numbers are
    /// `STAR_NOT_LOWERABLE`, as is a session with no segment or with several.
    fn diag_flash_transfer(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let failed = || {
            Val::Run(
                Tys {
                    bits: Ty::Response.bit(),
                    service: Service::Exit,
                },
                Vec::new(),
            )
        };
        let (Some(plan), Some(id)) = (self.active_plan, id) else {
            self.error(
                StarCode::PlanShape,
                span,
                "`diag.flash_transfer` belongs in a flash plan function, a top-level function \
                 annotated `# @flashPlan` (4.9)",
            );
            return failed();
        };
        if id != plan {
            self.error(
                StarCode::PlanShape,
                span,
                "`diag.flash_transfer` is a statement of the flash plan itself, not of a function \
                 it calls (4.9)",
            );
            return failed();
        }
        let [session_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.flash_transfer` takes a session id (3.1)",
            );
            return failed();
        };
        let (session, erases) = {
            let info = &self.plans[&plan];
            (info.session, info.erases)
        };
        let named = self.constant_int(
            Some(id),
            session_arg,
            "flash_transfer",
            "session_id",
            0..=i64::from(u32::MAX),
        );
        let Some(named) = named else {
            return failed();
        };
        if named != i64::from(session) {
            self.error(
                StarCode::PlanShape,
                session_arg.span,
                format!(
                    "`diag.flash_transfer` names session {named}, but `@flashPlan` names \
                     session {session} (4.9)"
                ),
            );
            return failed();
        }
        // The session exists in the variant: `flash_plans` checked it.
        let declared = self
            .variant
            .flash_sessions()
            .and_then(|sessions| sessions.iter().find(|s| s.id() == session));
        let segments = declared.and_then(|d| d.segments());
        let count = segments.map_or(0, |segments| segments.len());
        if count != 1 {
            self.not_lowerable(
                span,
                if count == 0 {
                    format!("flash session {session} declares no segment to transfer (4.12)")
                } else {
                    format!(
                        "flash session {session} declares {count} segments, and a plan transfers \
                         one: a plan holds one RequestDownload and one RequestTransferExit, which \
                         `Program::validate` enforces (ADR-336 item 13)"
                    )
                },
            );
            return failed();
        }
        let segment = segments.expect("one segment").get(0);
        let (start, length, block_size) = (
            segment.start_address(),
            segment.length(),
            u64::from(segment.block_size()),
        );
        let (data_format, format) = (segment.data_format(), segment.address_and_length_format());
        let (address_width, length_width) = (usize::from(format & 0x0F), usize::from(format >> 4));
        let (Some(address), Some(size)) = (
            be_in_width(start, address_width),
            be_in_width(length, length_width),
        ) else {
            self.not_lowerable(
                span,
                format!(
                    "the segment's start address {start:#x} or length {length:#x} does not fit \
                     the {address_width} and {length_width} bytes (each 1 or more) its \
                     address_and_length_format {format:#04x} gives (ISO 14229-1 (2026) 14.2.2.3, csv-tables 4.12)"
                ),
            );
            return failed();
        };
        // The TransferData request is the counter, then the block (ADR-336 item 11).
        // Only the longest block that exists counts: a block size above the length is one block.
        if block_size == 0 || block_size.min(length) > (MAX_BYTES_LEN - 2) as u64 {
            self.not_lowerable(
                span,
                format!(
                    "the segment's longest block, {} bytes (block_size {block_size}), is not 1 to \
                     {}, the most one TransferData payload holds (csv-tables 4.12, ADR-336 \
                     item 11)",
                    block_size.min(length),
                    MAX_BYTES_LEN - 2
                ),
            );
            return failed();
        }
        let blocks = length.div_ceil(block_size);
        if blocks > u64::from(u32::MAX) {
            self.not_lowerable(
                span,
                format!(
                    "the segment needs {blocks} blocks, more than the 2^32 - 1 the write-job \
                     journal numbers; use a larger block_size (ADR-336 item 11)"
                ),
            );
            return failed();
        }
        let operand: Vec<u8> = [data_format, format]
            .into_iter()
            .chain(address)
            .chain(size)
            .collect();
        let operand = self.pool(&operand);
        let exit = self.pool(&[]);
        let i = self.temp(id, span);
        let (head, end) = (self.label(), self.label());
        let mut code: Code = Vec::new();
        code.push((Ins::Op(Op::PushBytes(operand)), span));
        if !erases {
            // Without an erase step the RequestDownload is the erase point (ADR-336 item 4).
            code.push((Ins::Boundary(Boundary::Erase), span));
        }
        code.extend(
            [
                Ins::Op(Op::ServiceRequest { service: 0x34 }),
                // Its response is dropped (ADR-336 item 10).
                Ins::Op(Op::Pop),
                Ins::Op(Op::PushI64(0)),
                Ins::Op(Op::StoreLocal(i)),
                Ins::Mark(head),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::PushI64(blocks as i64)),
                Ins::Op(Op::CmpLt),
                Ins::JumpIfFalse(end),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::FlashBlock {
                    session,
                    segment: 0,
                }),
                // The host numbers the blocks itself, so the operand is the same for each.
                Ins::Op(Op::FlashTransfer { block: 0 }),
                Ins::Op(Op::LoadLocal(i)),
                Ins::Op(Op::PushI64(1)),
                Ins::Op(Op::Add),
                Ins::Op(Op::StoreLocal(i)),
                Ins::Jump(head),
                Ins::Mark(end),
                Ins::Op(Op::PushBytes(exit)),
                Ins::Boundary(Boundary::TransferExit),
                Ins::Op(Op::ServiceRequest { service: 0x37 }),
            ]
            .map(|ins| (ins, span)),
        );
        Val::Run(
            Tys {
                bits: Ty::Response.bit(),
                service: Service::Exit,
            },
            code,
        )
    }

    /// `# @recoveryRequired` marks the first instruction of its statement (4.9), so a statement
    /// that emits none (a docstring, `pass`, a constant expression, a branch that folds away)
    /// cannot carry it: the point would silently fall back to the erase, which is weaker than
    /// what the author declared.
    fn no_instruction_to_mark(&mut self, span: Span) {
        self.error(
            StarCode::BadAnnotation,
            span,
            "`@recoveryRequired` marks the first instruction of its statement, and this statement \
             emits none; put it on a statement that does something (4.9)",
        );
    }

    /// Lowers `diag.{name}(...)` and gives each diagnostic primitive it emits the call's
    /// idempotency (ADR-332): the innermost `@idempotency` around the call, or else the default
    /// of 4.6 for the Starlark call, whatever instructions it lowers to. A primitive an argument
    /// emitted (an inner `diag` call) keeps its own.
    fn call_diag(&mut self, id: Option<usize>, name: &str, args: &[&AstExpr], span: Span) -> Val {
        self.call_diag_keyed(id, name, args, None, span)
    }

    /// [`Self::call_diag`] for a call that a literal key reads (`diag.ecu_info()["vin"]`, 3.2),
    /// `key` being the key's expression.
    fn call_diag_keyed(
        &mut self,
        id: Option<usize>,
        name: &str,
        args: &[&AstExpr],
        key: Option<&AstExpr>,
        span: Span,
    ) -> Val {
        let value = self.call_diag_untagged(id, name, args, key, span);
        let annotation = self.annotated.last().map(|(value, _)| *value);
        let in_plan = self.active_plan.is_some();
        let tag = |code: Code, lowered: &mut bool| -> Code {
            code.into_iter()
                .map(|(ins, span)| match ins {
                    Ins::Op(op) if op.is_diagnostic_primitive() => {
                        *lowered = true;
                        let idempotency =
                            annotation.unwrap_or_else(|| default_idempotency(name, &op, in_plan));
                        (Ins::Primitive(op, idempotency), span)
                    }
                    other => (other, span),
                })
                .collect()
        };
        let mut lowered = false;
        let value = match value {
            Val::Const(value) => Val::Const(value),
            Val::Run(tys, code) => Val::Run(tys, tag(code, &mut lowered)),
            Val::Void(code) => Val::Void(tag(code, &mut lowered)),
        };
        if lowered {
            // The primitive is in every annotated statement around the call.
            for (_, covers) in &mut self.annotated {
                *covers = true;
            }
        }
        value
    }

    fn call_diag_untagged(
        &mut self,
        id: Option<usize>,
        name: &str,
        args: &[&AstExpr],
        key: Option<&AstExpr>,
        span: Span,
    ) -> Val {
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
                let value = self.expr(id, millis);
                // A `diag.wait` duration known only at run time is the stack operand of
                // `WaitFromStack`, which checks its range when it runs (3.1).
                if name == "wait" && !matches!(value, Val::Const(_)) {
                    let Some((ty, mut code)) =
                        self.single(value, millis.span, "a `diag.wait` duration")
                    else {
                        return Val::Void(Vec::new());
                    };
                    if ty != Ty::Int {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            millis.span,
                            format!(
                                "`{what}` of `diag.wait` is an int, not a {} (3.1)",
                                ty.name()
                            ),
                        );
                        return Val::Void(Vec::new());
                    }
                    code.push((Ins::Op(Op::WaitFromStack), span));
                    return Val::Void(code);
                }
                let Some(millis) =
                    self.constant_int_of(value, millis, name, what, 0..=i64::from(u32::MAX))
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
                let message_val = self.expr(id, message);
                // A message known only at run time is the stack operand of `LogFromStack`; a
                // constant one is pooled for `Log` (3.1).
                if !matches!(message_val, Val::Const(_)) {
                    let Some((ty, mut code)) =
                        self.single(message_val, message.span, "a `diag.log` message")
                    else {
                        return Val::Void(Vec::new());
                    };
                    if ty != Ty::Str {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            message.span,
                            format!("a `diag.log` message is a str, not a {}", ty.name()),
                        );
                        return Val::Void(Vec::new());
                    }
                    let Some(level) = level else {
                        return Val::Void(Vec::new());
                    };
                    code.push((
                        Ins::Op(Op::LogFromStack {
                            // In range by the check above.
                            level: level as u8,
                        }),
                        span,
                    ));
                    return Val::Void(code);
                }
                let message = match message_val {
                    Val::Const(Const::Str(text)) => Some(text),
                    Val::Const(other) => {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            message.span,
                            format!("a `diag.log` message is a str, not a {}", other.type_name()),
                        );
                        None
                    }
                    _ => unreachable!("a run-time message returned above"),
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
            "routine" if self.requests.profile.call(Intent::Routine).is_some() => {
                self.diag_routine(id, args, span)
            }
            "security_access" if self.requests.profile.call(Intent::SecurityAccess).is_some() => {
                self.diag_security_access(id, args, span)
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
            "precondition" => self.diag_precondition(id, args, span),
            "ecu_info" => self.diag_ecu_info(id, args, key, span),
            // UDS only (ADR-336 item 9).
            "flash_transfer" if self.requests.profile.layer != UDS => {
                self.error(
                    StarCode::NotInProfile,
                    span,
                    format!(
                        "the {} profile of this variant's protocols has no `diag.flash_transfer`: \
                         only UDS has one (3.2, 4.9)",
                        self.requests.profile.layer
                    ),
                );
                Val::Run(Tys::default(), Vec::new())
            }
            "flash_transfer" => self.diag_flash_transfer(id, args, span),
            "hmi" | "record" => self.diag_hmi(id, name, args, span),
            _ => {
                self.not_lowerable(span, format!("`diag.{name}` is not lowered yet (3.2)"));
                Val::Run(Tys::default(), Vec::new())
            }
        }
    }

    /// `diag.hmi(form_id, params)` and `diag.record(template_id)` (subset 3.1, 3.2, ADR-340
    /// item 4): the form is an encoded `HmiTemplate` in the constant pool, whose constant
    /// parameters are in it and whose run-time ones are slots filled from the stack by
    /// `HmiRequestFromStack`, their code emitted in source order before it. The value is the
    /// answer ([`Ty::HmiAnswer`]).
    fn diag_hmi(&mut self, id: Option<usize>, name: &str, args: &[&AstExpr], span: Span) -> Val {
        let answer = || Val::Run(Tys::of(Ty::HmiAnswer), Vec::new());
        let record = name == "record";
        let (id_arg, params_arg) = match (record, args) {
            (false, [form, params]) => (*form, Some(*params)),
            (true, [template]) => (*template, None),
            _ => {
                let takes = if record {
                    "a template id"
                } else {
                    "a form id and a params dict"
                };
                self.error(
                    StarCode::UnsupportedSyntax,
                    span,
                    format!("`diag.{name}` takes {takes} (3.1)"),
                );
                return answer();
            }
        };
        let mut failed = false;
        let form_id = match self.expr(id, id_arg) {
            Val::Const(Const::Str(text)) if (1..=MAX_HMI_ID_LEN).contains(&text.len()) => {
                Some(text)
            }
            Val::Const(Const::Str(_)) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    id_arg.span,
                    format!("the id of `diag.{name}` is 1 to {MAX_HMI_ID_LEN} bytes (3.1)"),
                );
                None
            }
            Val::Const(other) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    id_arg.span,
                    format!(
                        "the id of `diag.{name}` is a str, not a {} (3.1)",
                        other.type_name()
                    ),
                );
                None
            }
            _ => {
                self.error(
                    StarCode::NonConstantArgument,
                    id_arg.span,
                    format!("the id of `diag.{name}` is a string constant (3.1)"),
                );
                None
            }
        };
        failed |= form_id.is_none();
        let mut params = Vec::new();
        let mut code: Code = Vec::new();
        let mut runtime = false;
        if let Some(arg) = params_arg {
            if let ExprP::Dict(pairs) = &arg.node {
                if pairs.len() > MAX_HMI_PARAMS {
                    self.error(
                        StarCode::ArgumentOutOfRange,
                        arg.span,
                        format!("`diag.hmi` takes at most {MAX_HMI_PARAMS} params (3.1)"),
                    );
                    failed = true;
                }
                let mut keys = HashSet::new();
                for (key, value) in pairs {
                    let key_ok = self.hmi_param_key(id, key, &mut keys);
                    let param = match self.expr(id, value) {
                        Val::Const(Const::Int(n)) => Some(HmiParam::Const(HmiValue::Int(n))),
                        Val::Const(Const::Bool(b)) => Some(HmiParam::Const(HmiValue::Bool(b))),
                        Val::Const(Const::Str(text)) => {
                            Some(HmiParam::Const(HmiValue::Text(text.to_string())))
                        }
                        Val::Const(other) => {
                            self.hmi_param_type(value.span, other.type_name());
                            None
                        }
                        run => match self.single(run, value.span, "a `diag.hmi` param") {
                            Some((Ty::Int | Ty::Bool | Ty::Str, value_code)) => {
                                code.extend(value_code);
                                runtime = true;
                                Some(HmiParam::Runtime)
                            }
                            Some((ty, _)) => {
                                self.hmi_param_type(value.span, ty.name());
                                None
                            }
                            None => None,
                        },
                    };
                    match (key_ok, param) {
                        (Some(key), Some(param)) => params.push((key, param)),
                        _ => failed = true,
                    }
                }
            } else {
                self.error(
                    StarCode::UnsupportedSyntax,
                    arg.span,
                    "the params of `diag.hmi` are a dict literal (3.1)",
                );
                failed = true;
            }
        }
        let Some(form_id) = form_id.filter(|_| !failed) else {
            return answer();
        };
        let template = HmiTemplate {
            id: form_id.to_string(),
            params,
        };
        let bytes = match encode_template(&template) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.error(
                    StarCode::ArgumentOutOfRange,
                    span,
                    format!("the form of `diag.{name}` is refused: {error} (3.1)"),
                );
                return answer();
            }
        };
        let index = self.pool(&bytes);
        let op = match (runtime, record) {
            (true, _) => Op::HmiRequestFromStack { form: index },
            (false, false) => Op::HmiRequest { form: index },
            (false, true) => Op::RecordInput { template: index },
        };
        code.push((Ins::Op(op), span));
        Val::Run(Tys::of(Ty::HmiAnswer), code)
    }

    /// The name of one `diag.hmi` param: a string literal that is an identifier of at most
    /// [`MAX_HMI_KEY_LEN`] bytes and not in `seen`, or `None` after an error.
    fn hmi_param_key(
        &mut self,
        id: Option<usize>,
        key: &AstExpr,
        seen: &mut HashSet<Rc<str>>,
    ) -> Option<String> {
        let literal = matches!(&key.node, ExprP::Literal(AstLiteral::String(_)));
        let Val::Const(Const::Str(text)) = self.expr(id, key) else {
            self.error(
                StarCode::UnsupportedSyntax,
                key.span,
                "a `diag.hmi` param name is a string literal (3.1)",
            );
            return None;
        };
        if !literal {
            self.error(
                StarCode::UnsupportedSyntax,
                key.span,
                "a `diag.hmi` param name is a string literal (3.1)",
            );
            return None;
        }
        let mut bytes = text.bytes();
        let identifier = bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !identifier {
            self.error(
                StarCode::UnsupportedSyntax,
                key.span,
                format!(
                    "the `diag.hmi` param name {text:?} is not an identifier \
                     ([A-Za-z_][A-Za-z0-9_]*) (3.1)"
                ),
            );
            return None;
        }
        if text.len() > MAX_HMI_KEY_LEN {
            self.error(
                StarCode::ArgumentOutOfRange,
                key.span,
                format!("a `diag.hmi` param name is at most {MAX_HMI_KEY_LEN} bytes (3.1)"),
            );
            return None;
        }
        if !seen.insert(text.clone()) {
            self.error(
                StarCode::ArgumentOutOfRange,
                key.span,
                format!("the `diag.hmi` param name {text:?} is given twice (3.1)"),
            );
            return None;
        }
        Some(text.to_string())
    }

    fn hmi_param_type(&mut self, span: Span, found: &str) {
        self.error(
            StarCode::UnsupportedSyntax,
            span,
            format!("a `diag.hmi` param is an int, a bool or a str, not a {found} (3.1)"),
        );
    }

    /// `ans["ok"]`, `ans["timed_out"]` or `ans["value"]` of a `diag.hmi` or `diag.record` answer
    /// (subset 3.4, ADR-340 item 3): `code` pushes the answer bytes, whose first byte is the
    /// status and the rest the text. Each read is total over any bytes.
    fn hmi_key(&mut self, code: Code, key: Val, span: Span) -> Val {
        let Val::Const(Const::Str(key)) = key else {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                span,
                "an HMI answer is read with a string literal key (3.4, 4.1)",
            );
            return Val::Run(Tys::default(), Vec::new());
        };
        let mut code = code;
        let status = match &*key {
            "ok" => Some(diag_ir::hmi::ANSWERED),
            "timed_out" => Some(diag_ir::hmi::TIMED_OUT),
            _ => None,
        };
        if let Some(status) = status {
            let expected = self.pool(&[status]);
            code.extend(
                [
                    Op::PushI64(0),
                    Op::PushI64(1),
                    Op::Slice,
                    Op::PushBytes(expected),
                    Op::CmpEq,
                ]
                .map(|op| (Ins::Op(op), span)),
            );
            return Val::Run(Tys::of(Ty::Bool), code);
        }
        if &*key == "value" {
            code.extend(
                [Op::PushI64(1), Op::PushI64(i64::MAX), Op::Slice].map(|op| (Ins::Op(op), span)),
            );
            return Val::Run(Tys::of(Ty::Str), code);
        }
        self.error(
            StarCode::UnsupportedSyntax,
            span,
            format!("an HMI answer has the keys `ok`, `timed_out` and `value`, not `{key}` (3.4)"),
        );
        Val::Run(Tys::default(), Vec::new())
    }

    /// The arguments of `object` when it is a call to `diag.ecu_info`, which an index reads
    /// directly (3.2, ADR-339); `None` for any other expression.
    fn ecu_info_call<'e>(
        &self,
        id: Option<usize>,
        object: &'e AstExpr,
    ) -> Option<Vec<&'e AstExpr>> {
        let ExprP::Call(function, args) = &object.node else {
            return None;
        };
        let ExprP::Dot(module, method) = &function.node else {
            return None;
        };
        let is_diag = matches!(&module.node, ExprP::Identifier(m) if m.node.ident == "diag")
            && !id.is_some_and(|id| self.funcs[id].slots.contains_key("diag"))
            && !self.globals.contains_key("diag")
            && !self.by_name.contains_key("diag");
        if !is_diag || method.node != "ecu_info" {
            return None;
        }
        // A non-positional argument is no argument the call takes: it is refused as the wrong
        // arguments below, by its count.
        Some(
            args.args
                .iter()
                .map(|arg| match &arg.node {
                    ArgumentP::Positional(expr) | ArgumentP::Named(_, expr) => expr,
                    ArgumentP::Args(expr) | ArgumentP::KwArgs(expr) => expr,
                })
                .collect(),
        )
    }

    /// `diag.ecu_info()["key"]` (subset 3.2, ADR-339): the variant's short name for `variant`,
    /// a string constant; for `vin`, `part_number` and `software_version`, a read of the DID
    /// the variant's `identity.csv` row of that kind names (`vin`, `hardware_part_number`,
    /// `software_version`, 4.13), lowered like a service source of `diag.precondition`: a
    /// send-receive of the DID read and a decode of its field. Each key read is a request of its
    /// own at the point of access. The value is a `str` for an ASCII field and an `int` for an
    /// unsigned integer one, as the row's field is declared. The call has no value of its own,
    /// so it is refused unless a literal key reads it.
    fn diag_ecu_info(
        &mut self,
        id: Option<usize>,
        args: &[&AstExpr],
        key: Option<&AstExpr>,
        span: Span,
    ) -> Val {
        let none = || Val::Run(Tys::default(), Vec::new());
        if !args.is_empty() {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.ecu_info` takes no arguments (3.1)",
            );
            return none();
        }
        let Some(key_arg) = key else {
            self.not_lowerable(
                span,
                "`diag.ecu_info()` has no value of its own: read a key of it with a string                  literal, `diag.ecu_info()[\"vin\"]` (3.2)",
            );
            return none();
        };
        if !matches!(&key_arg.node, ExprP::Literal(AstLiteral::String(_))) {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                key_arg.span,
                "`diag.ecu_info()` is read with a string literal key (3.2, 4.1)",
            );
            return none();
        }
        let Val::Const(Const::Str(key)) = self.expr(id, key_arg) else {
            return none();
        };
        let (label, source) = match &*key {
            "variant" => {
                let Some(name) = self.variant.short_name() else {
                    self.not_lowerable(
                        key_arg.span,
                        "the variant declares no short name for `diag.ecu_info()[\"variant\"]` (3.2)",
                    );
                    return none();
                };
                return Val::Const(Const::str(name));
            }
            "vin" => ("vin", self.requests.identity.vin),
            "part_number" => (
                "hardware_part_number",
                self.requests.identity.hardware_part_number,
            ),
            "software_version" => ("software_version", self.requests.identity.software_version),
            other => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    key_arg.span,
                    format!(
                        "`diag.ecu_info()` has the keys `variant`, `vin`, `part_number` and \
                         `software_version`, not `{other}` (3.2)"
                    ),
                );
                return none();
            }
        };
        let Some(source) = source else {
            self.error(
                StarCode::UnknownIdentity,
                key_arg.span,
                format!("the variant's identity.csv has no row of kind {label} (3.2)"),
            );
            return none();
        };
        let diag_ir::Source::EcuService {
            service_id,
            field_id,
        } = source
        else {
            self.not_lowerable(
                key_arg.span,
                format!("the {label} identity row is not a service source (3.2)"),
            );
            return none();
        };
        let request = self
            .service_at
            .get(&service_id)
            .and_then(|&at| self.variant.services()?.get(at).request());
        let Some(request) = request else {
            self.not_lowerable(
                key_arg.span,
                format!(
                    "the {label} identity row names a service the variant does not declare \
                     with a request (3.2)"
                ),
            );
            return none();
        };
        if request.params().is_some_and(|params| !params.is_empty()) {
            self.not_lowerable(
                key_arg.span,
                format!(
                    "the {label} identity row names a request with fields, which sends no fixed \
                     bytes (3.2)"
                ),
            );
            return none();
        }
        let field_kind = self
            .variant
            .services()
            .and_then(|services| services.iter().find(|s| s.id() == service_id))
            .and_then(|s| s.positive())
            .and_then(|positive| positive.fields())
            .and_then(|fields| fields.iter().find(|f| f.id() == field_id))
            .map(|field| diag_ir::decode::scalar_kind(&field));
        let ty = match field_kind {
            Some(Ok(diag_ir::decode::ScalarKind::Integer)) => Ty::Int,
            Some(Ok(diag_ir::decode::ScalarKind::Text)) => Ty::Str,
            _ => {
                self.not_lowerable(
                    key_arg.span,
                    format!(
                        "the {label} identity row is not an integer or text field of a declared \
                         positive response (3.2)"
                    ),
                );
                return none();
            }
        };
        let mut bytes = vec![request.sid()];
        bytes.extend_from_slice(request.prefix().map(|p| p.bytes()).unwrap_or_default());
        let operand = self.pool(&bytes);
        Val::Run(
            Tys::of(ty),
            vec![
                (Ins::Op(Op::PushBytes(operand)), span),
                (
                    Ins::Op(Op::SendReceive {
                        service: service_id,
                    }),
                    span,
                ),
                (
                    Ins::Op(Op::DecodeField {
                        service: service_id,
                        field: field_id,
                    }),
                    span,
                ),
            ],
        )
    }

    /// `diag.precondition(name)` (subset 3.2, ADR-338): the current value of the precondition
    /// `name` names, an `int` read from the default-session source of the variant's
    /// `preconditions.csv` row of that kind, chosen here. The name is a string constant (the
    /// spelling `# @preconditions` uses) and need not appear in that annotation, which declares
    /// what the program checks before it runs, not what it reads during the run. A service
    /// source lowers to a send-receive of the DID read and a decode of its field, the
    /// instructions a `diag.request` and a field read lower to; a runtime input to
    /// `Op::ReadInput`.
    fn diag_precondition(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let none = || Val::Run(Tys::of(Ty::Int), Vec::new());
        let [name_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.precondition` takes the name of a precondition (3.1)",
            );
            return none();
        };
        let name = match self.expr(id, name_arg) {
            Val::Const(Const::Str(name)) => name,
            Val::Const(other) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    name_arg.span,
                    format!(
                        "`name` of `diag.precondition` is a str, not a {} (3.1)",
                        other.type_name()
                    ),
                );
                return none();
            }
            _ => {
                self.error(
                    StarCode::NonConstantArgument,
                    name_arg.span,
                    "`name` of `diag.precondition` must be known at ingestion: it selects the \
                     source the value is read from (3.1)",
                );
                return none();
            }
        };
        let Some(kind) = super::precondition_kind(&name) else {
            self.error(
                StarCode::ArgumentOutOfRange,
                name_arg.span,
                format!(
                    "`{name}` is not a precondition; `diag.precondition` takes `voltage`, \
                     `externalSupply`, `ignition`, `engine` or `vehicleSpeed` (3.2)"
                ),
            );
            return none();
        };
        let table = &self.requests.preconditions;
        let row = match kind {
            PreconditionKind::Voltage => &table.voltage_mv,
            PreconditionKind::ExternalSupply => &table.external_supply,
            PreconditionKind::Ignition => &table.ignition,
            PreconditionKind::Engine => &table.engine,
            PreconditionKind::VehicleSpeed => &table.vehicle_speed,
        };
        let Some(row) = row else {
            self.error(
                StarCode::UnknownPrecondition,
                name_arg.span,
                format!("the variant's preconditions.csv has no row of kind {name} (3.2)"),
            );
            return none();
        };
        let Some(source) = row.default_session else {
            self.error(
                StarCode::UnknownPrecondition,
                name_arg.span,
                format!(
                    "the variant's preconditions.csv row of kind {name} has no default-session \
                     source to read the value from (3.2)"
                ),
            );
            return none();
        };
        match source {
            diag_ir::Source::RuntimeInput(input) => Val::Run(
                Tys::of(Ty::Int),
                vec![(Ins::Op(Op::ReadInput { input }), span)],
            ),
            diag_ir::Source::EcuService {
                service_id,
                field_id,
            } => {
                let request = self
                    .service_at
                    .get(&service_id)
                    .and_then(|&at| self.variant.services()?.get(at).request());
                let Some(request) = request else {
                    self.not_lowerable(
                        name_arg.span,
                        format!(
                            "the source of the {name} precondition is a service the variant \
                             does not declare with a request (3.2)"
                        ),
                    );
                    return none();
                };
                // A DID read: the SID and the identifier, which the request's prefix holds.
                if request.params().is_some_and(|params| !params.is_empty()) {
                    self.not_lowerable(
                        name_arg.span,
                        format!(
                            "the source of the {name} precondition is a request with fields, \
                             which sends no fixed bytes (3.2)"
                        ),
                    );
                    return none();
                }
                // The value is an `int`: the field must decode to an integer, as the table's
                // source rules ask (`csv-tables.md` 4.14), not to the float a numeric
                // conversion would give.
                let field_kind = self
                    .variant
                    .services()
                    .and_then(|services| services.iter().find(|s| s.id() == service_id))
                    .and_then(|s| s.positive())
                    .and_then(|positive| positive.fields())
                    .and_then(|fields| fields.iter().find(|f| f.id() == field_id))
                    .map(|field| diag_ir::decode::scalar_kind(&field));
                match field_kind {
                    Some(Ok(diag_ir::decode::ScalarKind::Integer)) => {}
                    Some(Ok(_) | Err(_)) | None => {
                        self.not_lowerable(
                            name_arg.span,
                            format!(
                                "the source of the {name} precondition is not an integer field \
                                 of a declared positive response (3.2)"
                            ),
                        );
                        return none();
                    }
                }
                let mut bytes = vec![request.sid()];
                bytes.extend_from_slice(request.prefix().map(|p| p.bytes()).unwrap_or_default());
                let operand = self.pool(&bytes);
                Val::Run(
                    Tys::of(Ty::Int),
                    vec![
                        (Ins::Op(Op::PushBytes(operand)), span),
                        (
                            Ins::Op(Op::SendReceive {
                                service: service_id,
                            }),
                            span,
                        ),
                        (
                            Ins::Op(Op::DecodeField {
                                service: service_id,
                                field: field_id,
                            }),
                            span,
                        ),
                    ],
                )
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
        let (params, run) = match self.request_params(id, "request", sid, None, params_arg) {
            None => (None, Vec::new()),
            Some((Val::Const(Const::Dict(entries, _)), run)) => (Some(entries), run),
            Some((Val::Const(other), _)) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    params_arg.span,
                    format!(
                        "`params` of `diag.request` is a dict, not a {} (3.1)",
                        other.type_name()
                    ),
                );
                (None, Vec::new())
            }
            Some(_) => {
                self.error(
                    StarCode::NonConstantArgument,
                    params_arg.span,
                    "`params` of `diag.request` must be a dict known at ingestion: they select \
                     the service (3.1)",
                );
                (None, Vec::new())
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
        let Some(mut code) = self.request_operand(
            id,
            "request",
            &request,
            values,
            run,
            &[],
            params_arg.span,
            span,
        ) else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        code.push((
            Ins::Op(Op::ServiceRequest {
                service: u16::from(sid),
            }),
            span,
        ));
        Val::Run(Tys::from_service(Ty::Response, service), code)
    }

    /// The code that pushes the operand of `diag.{function}`'s `request`: the bytes after the
    /// SID that [`encode_request`] writes from `values`, without the leading `skip` bytes, which
    /// the instruction sends itself. A param in `run` is known only at run time (3.2): its value
    /// is computed in source order and kept in a temporary, each must fit its field or the
    /// procedure ends with `Fail` code `PARAM_OUT_OF_RANGE`, and its bits are then written into
    /// the operand encoded with 0 in its place. Only a fixed-length `uint` field takes such a
    /// value (ADR-341). Where each of the value's bits goes is found by encoding it alone, so the layout
    /// is `encode_request`'s, byte order and bit position included. Errors about the params
    /// point at `params_span`, and the operand's code at the call's `span`. `None` once an error
    /// is reported.
    #[expect(
        clippy::too_many_arguments,
        reason = "what both callers pass: the selected request, its values and the call's parts"
    )]
    fn request_operand(
        &mut self,
        id: Option<usize>,
        function: &str,
        request: &Request<'a>,
        values: HashMap<&str, ParamValue<'_>>,
        run: Vec<RunParam>,
        skip: &[u8],
        params_span: Span,
        span: Span,
    ) -> Option<Code> {
        let keys: Vec<Rc<str>> = run.iter().map(|param| param.key.clone()).collect();
        let mut values = values;
        let mut fields = Vec::new();
        for param in &run {
            let field = request
                .params()
                .into_iter()
                .flatten()
                .find(|field| field.short_name() == Some(&*param.key));
            let width = field
                .filter(|field| {
                    field.kind() == DataKind::Uint && field.length_kind() == LengthKind::Fixed
                })
                .map(|field| field.bit_length())
                .filter(|width| (1..=64).contains(width));
            let Some(width) = width else {
                self.not_lowerable(
                    param.span,
                    format!(
                        "the `{}` param of `diag.{function}` is known only at run time, which only \
                         a fixed-length uint field takes for now (3.2)",
                        param.key
                    ),
                );
                return None;
            };
            fields.push(width);
        }
        let encode = |this: &mut Self, values: &HashMap<&str, ParamValue<'_>>| match encode_request(
            request, values,
        ) {
            Ok(operand) if operand.starts_with(skip) => Some(operand[skip.len()..].to_vec()),
            Ok(_) => unreachable!("the selected request's prefix starts with them"),
            Err(e) => {
                this.error(
                    StarCode::ArgumentOutOfRange,
                    params_span,
                    format!("the request of `diag.{function}` cannot be encoded: {e} (3.1)"),
                );
                None
            }
        };
        let base = encode(self, &values)?;
        if run.is_empty() {
            let operand = self.pool(&base);
            return Some(vec![(Ins::Op(Op::PushBytes(operand)), span)]);
        }
        let Some(id) = id else {
            self.not_lowerable(
                span,
                format!("`diag.{function}` needs a function to hold its run-time params"),
            );
            return None;
        };
        // Per operand byte, the runs of value bits written into it: (param, first value bit,
        // number of bits, first bit in the byte).
        let mut writes: std::collections::BTreeMap<usize, Vec<(usize, u32, u32, u32)>> =
            Default::default();
        for (at, (param, &width)) in run.iter().zip(&fields).enumerate() {
            // A value is never negative, so bit 63 of a 64-bit field stays 0.
            for bit in 0..width.min(63) {
                // Each probe encodes the request and scans it (ADR-341 item 3).
                if !self.charge(base.len().saturating_mul(2), param.span) {
                    return None;
                }
                values.insert(&keys[at], ParamValue::Int(1 << bit));
                let probe = encode(self, &values)?;
                let changed: Vec<(usize, u32)> = (0..base.len())
                    .flat_map(|byte| (0..8).map(move |b| (byte, b)))
                    .filter(|&(byte, b)| (base[byte] ^ probe[byte]) >> b & 1 == 1)
                    .collect();
                let [(byte, b)] = changed[..] else {
                    self.not_lowerable(
                        param.span,
                        format!(
                            "the `{}` field of `diag.{function}`'s request is not laid out one \
                             bit per value bit (3.2)",
                            param.key
                        ),
                    );
                    return None;
                };
                let runs = writes.entry(byte).or_default();
                match runs.last_mut() {
                    Some((p, first, len, pos))
                        if *p == at && *first + *len == bit && *pos + *len == b =>
                    {
                        *len += 1
                    }
                    _ => runs.push((at, bit, 1, b)),
                }
            }
            values.insert(&keys[at], ParamValue::Int(0));
        }
        let mut code: Code = Vec::new();
        let mut slots = Vec::new();
        for param in run {
            let span = param.span;
            let (ty, value) = self.single(
                param.value,
                span,
                &format!("the `{}` param of `diag.{function}`", param.key),
            )?;
            if ty != Ty::Int {
                self.error(
                    StarCode::UnsupportedSyntax,
                    span,
                    format!(
                        "the `{}` param of `diag.{function}` is for a uint field, so it is an int, \
                         not a {} (3.2)",
                        param.key,
                        ty.name()
                    ),
                );
                return None;
            }
            let slot = self.temp(id, span);
            code.extend(value);
            code.push((Ins::Op(Op::StoreLocal(slot)), span));
            slots.push((slot, param.key, span));
        }
        let failure = self.pool(PARAM_OUT_OF_RANGE.as_bytes());
        for (&(slot, ref key, span), &width) in slots.iter().zip(&fields) {
            // `Shr` is logical, so a negative value leaves its high bits set as well.
            let ok = self.label();
            let detail = self.pool(
                format!(
                    "the `{key}` param of `diag.{function}` on line {} was given a value that does \
                     not fit its {width}-bit field",
                    self.line_of(span)
                )
                .as_bytes(),
            );
            code.extend(
                [
                    Ins::Op(Op::LoadLocal(slot)),
                    Ins::Op(Op::PushI64(i64::from(width.min(63)))),
                    Ins::Op(Op::Shr),
                    Ins::Op(Op::PushI64(0)),
                    Ins::Op(Op::CmpEq),
                    Ins::Op(Op::Not),
                    Ins::JumpIfFalse(ok),
                    Ins::Op(Op::PushBytes(detail)),
                    Ins::Op(Op::Fail { code: failure }),
                    Ins::Mark(ok),
                ]
                .map(|ins| (ins, span)),
            );
        }
        let operand = self.pool(&base);
        let mut ops = vec![Op::PushBytes(operand)];
        for (byte, runs) in writes {
            ops.extend([Op::PushI64(byte as i64), Op::PushI64(i64::from(base[byte]))]);
            for (at, first, len, pos) in runs {
                ops.push(Op::LoadLocal(slots[at].0));
                if first > 0 {
                    ops.extend([Op::PushI64(i64::from(first)), Op::Shr]);
                }
                ops.extend([Op::PushI64((1 << len) - 1), Op::BitAnd]);
                if pos > 0 {
                    ops.extend([Op::PushI64(i64::from(pos)), Op::Shl]);
                }
                ops.push(Op::BitOr);
            }
            // Leaves the operand with the byte written.
            ops.push(Op::IndexSet);
        }
        code.extend(ops.into_iter().map(|op| (Ins::Op(op), span)));
        Some(code)
    }

    /// The `params` of `diag.{function}(sid, ...)` as a value, `function` being `request` or
    /// `routine`. For `routine`, `selected` is the sub-function and identifier the call names, if
    /// both are known, and only the generated routine services whose request starts with them
    /// count. A dict literal is built here, so that a value known only at run time is reported
    /// for what it is: under a key some such service with the SID holds as a constant it is
    /// needed to select the service (csv-tables 5.3); under another key it is a field's value,
    /// which [`Self::request_operand`] encodes at run time: the dict then holds 0 under that key,
    /// and the value is returned beside it, in source order. `None` once the dict literal's
    /// errors are reported.
    fn request_params(
        &mut self,
        id: Option<usize>,
        function: &str,
        sid: Option<i64>,
        selected: Option<&[u8]>,
        arg: &AstExpr,
    ) -> Option<(Val, Vec<RunParam>)> {
        let routine = function == "routine";
        let ExprP::Dict(pairs) = &arg.node else {
            return Some((self.expr(id, arg), Vec::new()));
        };
        let mut entries = Vec::new();
        let mut run = Vec::new();
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
                known_at_run_time => {
                    let constant = self.requests.services.iter().any(|service| {
                        Some(i64::from(service.sid)) == sid
                            && service.routine == routine
                            && selected.is_none_or(|selected| {
                                self.request_prefix(service.service_id)
                                    .is_some_and(|prefix| prefix.starts_with(selected))
                            })
                            && service.constants.iter().any(|(name, _)| **name == *key)
                    });
                    if constant {
                        complete = false;
                        self.error(
                            StarCode::NonConstantArgument,
                            value.span,
                            format!(
                                "the `{key}` param of `diag.{function}` selects the service, \
                                 so it must be known at ingestion (3.4)"
                            ),
                        );
                    } else {
                        // A field's value: 0 stands in for it while the service is selected
                        // and the rest of the request encoded.
                        entries.push((key.clone(), Const::Int(0)));
                        run.push(RunParam {
                            key,
                            value: known_at_run_time,
                            span: value.span,
                        });
                    }
                }
            }
        }
        if !complete {
            return None;
        }
        // Anything but a dict means `bounded` refused it and reported why.
        match self.bounded(Const::dict(entries), arg.span) {
            value @ Val::Const(Const::Dict(..)) => Some((value, run)),
            _ => None,
        }
    }

    /// `diag.security_access(level)` through the variant's profile (ADR-309 items 2 and 4,
    /// ADR-314): the seed request, the `SecurityAccess` step that turns the seed into a key
    /// through the host, then the send-key request carrying that key. The profile's requests are
    /// not decoded through declared services, so each is a `ServiceRequest` of the template's SID
    /// whose payload the template gives. A negative seed response ends the call before the key
    /// step, as a failed `diag.request` does, and is its value; otherwise the value is the
    /// send-key response, positive or negative. The result has no `fields` (ADR-314 item 4).
    fn diag_security_access(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let key_result = Tys {
            bits: Ty::Response.bit(),
            service: Service::Key,
        };
        let failed = Val::Run(key_result, Vec::new());
        let [level_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.security_access` takes a security level (3.1)",
            );
            return failed;
        };
        let level = self
            .constant_int(id, level_arg, "security_access", "level", 0x01..=0x7D)
            .filter(|&level| {
                // The seed is requested with an odd sub-function, the key sent with the next one.
                let odd = level % 2 == 1;
                if !odd {
                    self.error(
                        StarCode::ArgumentOutOfRange,
                        level_arg.span,
                        format!(
                            "`level` of `diag.security_access` is {level}, which is even; a \
                             seed is requested with an odd level (3.1)"
                        ),
                    );
                }
                odd
            });
        let profile = self.requests.profile;
        let call = profile
            .call(Intent::SecurityAccess)
            .expect("the caller checked");
        let [seed_step, key_step, send_step] = call.steps else {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile's `diag.security_access` is not the seed request, the key \
                     step and the send-key request, the only form lowered yet (3.2)",
                    profile.layer
                ),
            );
            return failed;
        };
        let ResultMap::Seed { after_bytes } = seed_step.result else {
            unreachable!("a validated profile reads the seed from the step before the key step")
        };
        if seed_step.declared || send_step.declared || key_step.kind != StepKind::SecurityKey {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile's `diag.security_access` decodes a step through a declared \
                     service, which is not lowered yet (3.2)",
                    profile.layer
                ),
            );
            return failed;
        }
        let (Some(level), Some(id)) = (level, id) else {
            return failed;
        };
        // In range by the checks above.
        let level = level as u8;
        let args = [
            (Source::Level, u64::from(level)),
            (Source::LevelPlusOne, u64::from(level) + 1),
        ];
        let (seed_payload, false) = template_payload(&seed_step.request, &args) else {
            unreachable!("a validated profile sends the key only in the send-key request")
        };
        let (key_prefix, true) = template_payload(&send_step.request, &args) else {
            unreachable!("a validated profile sends the key in the send-key request")
        };
        let seed_payload = self.pool(&seed_payload);
        let response = self.temp(id, span);
        let end = self.label();
        let mut code: Code = [
            Ins::Op(Op::PushBytes(seed_payload)),
            Ins::Op(Op::ServiceRequest {
                service: u16::from(seed_step.request.sid),
            }),
            Ins::Op(Op::StoreLocal(response)),
            Ins::Op(Op::LoadLocal(response)),
        ]
        .map(|ins| (ins, span))
        .to_vec();
        code.extend(self.response_ok(span));
        code.push((Ins::JumpIfFalse(end), span));
        if !key_prefix.is_empty() {
            let key_prefix = self.pool(&key_prefix);
            code.push((Ins::Op(Op::PushBytes(key_prefix)), span));
        }
        code.extend(
            [
                Ins::Op(Op::LoadLocal(response)),
                Ins::Op(Op::PushI64(i64::from(after_bytes))),
                Ins::Op(Op::PushI64(i64::MAX)),
                Ins::Op(Op::Slice),
                Ins::Op(Op::SecurityAccess { level }),
            ]
            .map(|ins| (ins, span)),
        );
        if !key_prefix.is_empty() {
            code.push((Ins::Op(Op::Concat), span));
        }
        code.extend(
            [
                Ins::Op(Op::ServiceRequest {
                    service: u16::from(send_step.request.sid),
                }),
                Ins::Op(Op::StoreLocal(response)),
                Ins::Mark(end),
                Ins::Op(Op::LoadLocal(response)),
                // The slot would otherwise keep a copy of the response in the VM state, and its
                // journaled checkpoints, until the function returns.
                Ins::Op(Op::PushI64(0)),
                Ins::Op(Op::StoreLocal(response)),
            ]
            .map(|ins| (ins, span)),
        );
        Val::Run(key_result, code)
    }

    /// `diag.routine(routine_id, sub, params)` through the variant's profile (ADR-309 items 2 to
    /// 4, ADR-314): on UDS the profile's one `RoutineControl` step, which recovery relies on
    /// (ADR-309 item 6). The template's sub-function and routine identifier select the variant's
    /// generated routine service, whose request prefix starts with them; `params` then gives the
    /// service's own constants and fields by name, as for `diag.request` (csv-tables 5.3). The
    /// instruction sends the sub-function and identifier itself, so its operand is the rest of
    /// the request. The result reads the declared names of the response (ADR-314 item 4).
    fn diag_routine(&mut self, id: Option<usize>, args: &[&AstExpr], span: Span) -> Val {
        let [rid_arg, sub_arg, params_arg] = args else {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`diag.routine` takes a routine identifier, a sub-function and params (3.1)",
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        let rid = self.constant_int(id, rid_arg, "routine", "routine_id", 0..=0xFFFF);
        let sub = self.constant_int(id, sub_arg, "routine", "sub", 0..=0xFF);
        // In range by the checks above.
        let selected = rid
            .zip(sub)
            .map(|(rid, sub)| [&[sub as u8][..], &(rid as u16).to_be_bytes()].concat());
        let (params, run) =
            match self.request_params(id, "routine", Some(0x31), selected.as_deref(), params_arg) {
                None => (None, Vec::new()),
                Some((Val::Const(Const::Dict(entries, _)), run)) => (Some(entries), run),
                Some((Val::Const(other), _)) => {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        params_arg.span,
                        format!(
                            "`params` of `diag.routine` is a dict, not a {} (3.1)",
                            other.type_name()
                        ),
                    );
                    (None, Vec::new())
                }
                Some(_) => {
                    self.error(
                        StarCode::NonConstantArgument,
                        params_arg.span,
                        "`params` of `diag.routine` must be a dict known at ingestion (3.1)",
                    );
                    (None, Vec::new())
                }
            };
        let profile = self.requests.profile;
        let call = profile.call(Intent::Routine).expect("the caller checked");
        let [step] = call.steps else {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile's `diag.routine` takes several steps, which are not lowered \
                     yet (3.2)",
                    profile.layer
                ),
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        if step.kind != StepKind::RoutineControl {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile sends `diag.routine` with the generic send-receive, which is \
                     not lowered yet (3.2)",
                    profile.layer
                ),
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        }
        // The selection below builds exactly this template's prefix: the sub-function, then the
        // 16-bit identifier, then `params`.
        let expected = [
            Part::Select {
                bits: 8,
                arg: Source::Sub,
            },
            Part::Select {
                bits: 16,
                arg: Source::Rid,
            },
            Part::Params,
        ];
        if step.request.parts != expected {
            self.not_lowerable(
                span,
                format!(
                    "the {} profile's `diag.routine` request is not the sub-function, the 16-bit \
                     routine identifier and `params`, the only one lowered yet (3.2)",
                    profile.layer
                ),
            );
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        }
        let (Some(rid), Some(sub), Some(params)) = (rid, sub, params) else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        // In range by the checks above.
        let (rid, sub) = (rid as u16, sub as u8);
        let selected: Vec<u8> = [&[sub][..], &rid.to_be_bytes()].concat();
        let Some((service, request, constants)) = self.select_routine(&selected, &params) else {
            let mut keys: Vec<&str> = params.iter().map(|(key, _)| &**key).collect();
            keys.sort_unstable();
            self.error(
                StarCode::UnknownService,
                span,
                format!(
                    "no single routine service of this variant has routine identifier \
                     0x{rid:04X}, sub-function 0x{sub:02X}, takes the params {} and has the \
                     constants they give (csv-tables 5.3)",
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
                            "the `{key}` param of `diag.routine` is an int, float, bytes or str, \
                             not a {} (3.1)",
                            other.type_name()
                        ),
                    );
                    return Val::Run(Tys::of(Ty::Response), Vec::new());
                }
            };
            values.insert(&**key, value);
        }
        // The instruction sends the sub-function and the identifier before its operand.
        let Some(mut code) = self.request_operand(
            id,
            "routine",
            &request,
            values,
            run,
            &selected,
            params_arg.span,
            span,
        ) else {
            return Val::Run(Tys::of(Ty::Response), Vec::new());
        };
        code.push((Ins::Op(Op::RoutineControl { routine: rid, sub }), span));
        Val::Run(Tys::from_service(Ty::Response, service), code)
    }

    /// The declared request prefix of the service `service` (a `Service.id`).
    fn request_prefix(&self, service: u32) -> Option<&'a [u8]> {
        let at = *self.service_at.get(&service)?;
        let request = self.variant.services()?.get(at).request()?;
        Some(request.prefix().map(|p| p.bytes()).unwrap_or_default())
    }

    /// The one routine service of this variant whose request prefix starts with `selected` (the
    /// sub-function and routine identifier) and whose other constants and fields `params` names
    /// exactly, as `diag.request` selects (csv-tables 5.3), with the names of those constants.
    fn select_routine(
        &self,
        selected: &[u8],
        params: &[(Rc<str>, Const)],
    ) -> Option<(u32, Request<'a>, HashSet<&'a str>)> {
        let services = self.variant.services()?;
        let mut found = None;
        for indexed in &self.requests.services {
            if !indexed.routine {
                continue;
            }
            let Some(request) = self
                .service_at
                .get(&indexed.service_id)
                .and_then(|&at| services.get(at).request())
            else {
                continue;
            };
            let prefix = request.prefix().map(|p| p.bytes()).unwrap_or_default();
            if request.sid() != 0x31 || !prefix.starts_with(selected) {
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
                        // A field after one whose length is not fixed has no known start, so
                        // the records' start could not be counted (`dtc_records`).
                        let Some(start) = *next else {
                            return Some((field, false));
                        };
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
            "dtcs" => unreachable!("`dtcs` is read by `dtc_records`"),
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
        let value = self.expr(id, arg);
        self.constant_int_of(value, arg, function, what, range)
    }

    /// [`Lowerer::constant_int`] for an argument already lowered to `value`.
    fn constant_int_of(
        &mut self,
        value: Val,
        arg: &AstExpr,
        function: &str,
        what: &str,
        range: std::ops::RangeInclusive<i64>,
    ) -> Option<i64> {
        match value {
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

    /// Lays out the copies of the functions, `main`'s body first and ending in a `Ret`, and
    /// resolves labels, calls and source positions. Every diagnostic primitive gets a `Section`
    /// over that instruction alone, with the idempotency of its `diag` call. Each copy that runs
    /// in a section is covered, without a gap, by such sections and by `CheckState` pieces
    /// between them (ADR-329), all with that section function's interruptibility and expected
    /// duration (ADR-301). A primitive outside every section is `Yes` with duration 0. The
    /// sections never overlap (ADR-332).
    fn assemble(&mut self) -> (Program, Vec<Span>, Vec<Option<Origin>>) {
        // Labels are unique within a function's body, so a copy's are told apart by its index.
        let index: HashMap<FuncCopy, usize> = self
            .copies
            .iter()
            .enumerate()
            .map(|(at, copy)| (*copy, at))
            .collect();
        // `main`'s copy is the first and begins at pc 0 (ADR-336 item 5): the program runs it in
        // the top-level frame, and its closing `Ret` ends the program.
        let mut code: Vec<(Ins, Span, usize)> = Vec::new();
        // The sections, in layout order and so by start pc (ADR-332).
        let mut sections: Vec<Section> = Vec::new();
        // Where each flash plan's expansion begins and ends, and its boundaries (ADR-336 item 4).
        let mut plan_ranges: Vec<PlanRange> = Vec::new();
        // The section function, and for one expanded in place its call, that produced each
        // section, to locate a refusal that names one of them.
        let mut origins: Vec<Option<Origin>> = Vec::new();
        let mut pc = 0u32;
        for (at, copy) in self.copies.iter().enumerate() {
            let start = pc;
            let mut primitives: Vec<(u32, Idempotency)> = Vec::new();
            let mut open: Option<PlanRange> = None;
            let first_plan = plan_ranges.len();
            // The section functions expanded in place in a plan: the ones still open, and the
            // ranges of those closed, each with the function's attributes (ADR-336 item 3).
            let mut expanding: Vec<(u32, usize, Span)> = Vec::new();
            let mut expanded: Vec<Covered> = Vec::new();
            for (ins, span) in &self.funcs[copy.func].code {
                match ins {
                    Ins::PlanBegin(plan) => {
                        open = Some(PlanRange {
                            plan: *plan,
                            entry: pc,
                            end: pc,
                            erase: None,
                            recovery: None,
                            exit: None,
                        });
                    }
                    Ins::PlanEnd => {
                        if let Some(mut range) = open.take() {
                            range.end = pc;
                            plan_ranges.push(range);
                        }
                    }
                    Ins::Boundary(boundary) => {
                        if let Some(range) = &mut open {
                            let slot = match boundary {
                                Boundary::Erase => &mut range.erase,
                                Boundary::RecoveryRequired => &mut range.recovery,
                                Boundary::TransferExit => &mut range.exit,
                            };
                            slot.get_or_insert(pc);
                        }
                    }
                    Ins::SectionBegin(function) => expanding.push((pc, *function, *span)),
                    Ins::SectionEnd => {
                        if let Some((from, function, call)) = expanding.pop()
                            && from < pc
                        {
                            let attributes =
                                self.funcs[function].section.expect("a section function");
                            expanded.push((
                                from,
                                pc,
                                attributes,
                                Origin {
                                    function: Rc::from(self.funcs[function].name.as_str()),
                                    call: Some(call),
                                },
                            ));
                        }
                    }
                    Ins::Primitive(_, idempotency) => primitives.push((pc, *idempotency)),
                    _ => {}
                }
                if !ins.is_pseudo() {
                    pc += 1;
                }
                code.push((ins.clone(), *span, at));
            }
            // Outside every section a primitive's section is as interruptible as the code
            // around it; inside one it keeps the section function's attributes.
            let (interruptible, expected_millis) = match copy.section {
                Some(section) => self.funcs[section].section.expect("a section function"),
                None => (Interruptible::Yes, 0),
            };
            let section =
                |start_pc, end_pc, (interruptible, expected_millis), idempotency| Section {
                    start_pc,
                    end_pc,
                    interruptible,
                    idempotency,
                    expected_millis,
                };
            // The ranges whose pieces are cut around the primitives: the copy's own when it runs in
            // a section, and each plan's expansion, which the plan function's section covers
            // whole, inlined code included (ADR-336 items 3 and 4). The pieces between the
            // primitives cover none and say nothing about idempotency.
            let mut covered: Vec<Covered> = Vec::new();
            if let Some(function) = copy.section {
                covered.push((
                    start,
                    pc,
                    (interruptible, expected_millis),
                    Origin {
                        function: Rc::from(self.funcs[function].name.as_str()),
                        call: None,
                    },
                ));
            }
            for range in &plan_ranges[first_plan..] {
                // A plan function without a section is refused by the checker.
                let attributes = self.funcs[range.plan]
                    .section
                    .unwrap_or((Interruptible::No, 0));
                covered.push((
                    range.entry,
                    range.end,
                    attributes,
                    Origin {
                        function: Rc::from(self.funcs[range.plan].name.as_str()),
                        call: None,
                    },
                ));
            }
            // A section function expanded in a plan has its own pieces over its range, inside
            // the plan's: a reader that combines the covering sections takes the plan's
            // uninterruptibility with the function's attributes (ADR-336 item 3, ADR-330).
            covered.extend(expanded);
            let mut pieces: Vec<(Section, Option<Origin>)> = Vec::new();
            for (from, to, attributes, origin) in &covered {
                let (from, to, attributes) = (*from, *to, *attributes);
                let origin = Some(origin.clone());
                let mut cursor = from;
                for &(primitive, idempotency) in &primitives {
                    if primitive < from || primitive >= to {
                        continue;
                    }
                    if cursor < primitive {
                        pieces.push((
                            section(cursor, primitive, attributes, Idempotency::CheckState),
                            origin.clone(),
                        ));
                    }
                    pieces.push((
                        section(primitive, primitive + 1, attributes, idempotency),
                        origin.clone(),
                    ));
                    cursor = primitive + 1;
                }
                if cursor < to {
                    pieces.push((
                        section(cursor, to, attributes, Idempotency::CheckState),
                        origin,
                    ));
                }
            }
            for &(primitive, idempotency) in &primitives {
                if !covered
                    .iter()
                    .any(|&(from, to, _, _)| (from..to).contains(&primitive))
                {
                    pieces.push((
                        section(
                            primitive,
                            primitive + 1,
                            (interruptible, expected_millis),
                            idempotency,
                        ),
                        None,
                    ));
                }
            }
            pieces.sort_by_key(|(piece, _)| piece.start_pc);
            for (piece, origin) in pieces {
                sections.push(piece);
                origins.push(origin);
            }
        }
        // Positions of labels, counting only real instructions.
        let mut positions: HashMap<(usize, Label), u32> = HashMap::new();
        let mut pc = 0u32;
        for (ins, _, at) in &code {
            match ins {
                Ins::Mark(label) => {
                    positions.insert((*at, *label), pc);
                }
                other if other.is_pseudo() => {}
                _ => pc += 1,
            }
        }
        let mut ops = Vec::with_capacity(pc as usize);
        let mut spans = Vec::with_capacity(pc as usize);
        for (ins, span, at) in code {
            let op = match ins {
                Ins::Mark(_)
                | Ins::PlanBegin(_)
                | Ins::PlanEnd
                | Ins::Boundary(_)
                | Ins::SectionBegin(_)
                | Ins::SectionEnd => continue,
                Ins::Op(op) | Ins::Primitive(op, _) => op,
                Ins::Jump(label) => Op::Jump(positions[&(at, label)]),
                Ins::JumpIfFalse(label) => Op::JumpIfFalse(positions[&(at, label)]),
                Ins::Call(callee) => {
                    let caller = self.copies.get(at).and_then(|copy| copy.section);
                    let target = index[&self.copy_for(callee, caller)];
                    Op::Call(positions[&(target, self.funcs[callee].label)])
                }
            };
            ops.push(op);
            spans.push(span);
        }
        let source_map = self.source_map(&spans);
        let flash = plan_ranges
            .iter()
            .map(|range| self.flash_recovery(range))
            .collect();
        let program = Program {
            schema_version: IR_SCHEMA_VERSION,
            code: ops,
            constants: std::mem::take(&mut self.pool),
            sections,
            source_map,
            identity: self.requests.identity.clone(),
            preconditions: self.preconditions.clone(),
            flash,
        };
        (program, spans, origins)
    }

    /// The recovery plan of the flash plan laid out as `range` (ADR-336 items 1, 4 and 7): the
    /// session's recovery facts, the boundaries the layout found, and where on-site intervention
    /// begins. A boundary the layout did not find is placed at the entry, which `validate`
    /// refuses with the pc of the instruction there.
    fn flash_recovery(&self, range: &PlanRange) -> FlashRecovery {
        let info = &self.plans[&range.plan];
        let erase = range.erase.unwrap_or(range.entry);
        let from_entry = matches!(
            self.funcs[range.plan].section,
            Some((Interruptible::RecoveryRequired, _))
        );
        let recovery_required = if from_entry {
            RecoveryRequired::FromPc(range.entry)
        } else if info.recovery_never {
            RecoveryRequired::Never
        } else {
            RecoveryRequired::FromPc(range.recovery.unwrap_or(erase))
        };
        FlashRecovery {
            flash_session: info.session,
            stage: info.session,
            max_resumes: info.max_resumes,
            recovery_required,
            boundaries: RecoveryBoundaries {
                entry_pc: range.entry,
                erase_pc: erase,
                transfer_exit_pc: range.exit.unwrap_or(range.entry),
                post_transfer_end_pc: range.end,
            },
            timing: info.timing,
            version_read_retries: info.version_read_retries,
            no_application: info.no_application,
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

/// Where one flash plan's expansion lies in the laid-out code, and its boundaries (pcs).
struct PlanRange {
    /// The function that holds the plan.
    plan: usize,
    entry: u32,
    /// Exclusive: the pc after the last instruction.
    end: u32,
    erase: Option<u32>,
    recovery: Option<u32>,
    exit: Option<u32>,
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

/// The idempotency of a primitive that `diag.{call}` lowered to, when no `@idempotency` applies
/// (subset 4.6). It follows the Starlark call, not the instruction: every instruction of
/// `security_access` is `CheckState`. A `request` follows its service identifier, which the
/// request instruction holds: a read of data or of DTC information (0x22, 0x19), a
/// DiagnosticSessionControl (0x10) and a TesterPresent (0x3E) are `Safe` (a repeat re-enters the
/// session and is harmless in any state, ADR-336 item 8), any other service `Unsafe`, except
/// inside a flash plan's expansion (`in_plan`), where the plan's redo rules govern a repeat and
/// it is `CheckState` (ADR-336 item 8). Every call of the table is listed, and a name not in it
/// is `Unsafe`, so a call that starts to lower cannot become `Safe` by default.
fn default_idempotency(call: &str, op: &Op, in_plan: bool) -> Idempotency {
    match (call, op) {
        (
            "request",
            Op::ServiceRequest {
                service: 0x22 | 0x19 | 0x10 | 0x3E,
            },
        ) => Idempotency::Safe,
        ("request", _) if in_plan => Idempotency::CheckState,
        ("request", _) => Idempotency::Unsafe,
        (
            "read_dtc" | "ecu_info" | "precondition" | "wait" | "capture" | "log" | "hmi"
            | "record",
            _,
        ) => Idempotency::Safe,
        ("security_access" | "routine" | "flash_transfer", _) => Idempotency::CheckState,
        _ => Idempotency::Unsafe,
    }
}

/// `value` as `width` big-endian bytes, or `None` when it does not fit: a width of 0 is refused
/// (a RequestDownload carries an address and a size), and one above 8 bytes pads with zeros.
fn be_in_width(value: u64, width: usize) -> Option<Vec<u8>> {
    let bytes = value.to_be_bytes();
    if width == 0 {
        None
    } else if width >= bytes.len() {
        let mut padded = vec![0; width - bytes.len()];
        padded.extend(bytes);
        Some(padded)
    } else if value >> (8 * width) == 0 {
        Some(bytes[bytes.len() - width..].to_vec())
    } else {
        None
    }
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

/// A conservative definite-assignment analysis of one function body (4.9): the first local that
/// some path may read before it assigns it. A path state is the set of names assigned on every
/// way to it, or `None` once the path has ended (a `return`, `break`, `continue` or
/// `diag.fail`), which assigns everything for what follows. Parameters are assigned at the start;
/// an `if` keeps what both branches assign (a branch that ends does not count against it); a loop
/// body assigns nothing for the code after the loop, since it may not run, and its variable is
/// assigned inside the body only; the exception is a `range` loop whose bounds are constants known
/// at ingestion and that runs at least once, with no `break` in its body: what every path to the
/// end of an iteration (a `continue` included) assigned holds after it. A name that is not a local (a constant, a builtin) is never
/// reported.
struct Unset<'f> {
    locals: &'f HashMap<String, u16>,
    /// The constants of the top level, which a `range` bound may name.
    globals: &'f HashMap<String, Const>,
    found: Option<String>,
    /// For each loop being walked, what the paths that `continue` it assigned, joined.
    continues: Vec<Assigned>,
    /// The `break`s seen so far, whichever loop they leave.
    breaks: usize,
}

type Assigned = Option<HashSet<String>>;

impl Unset<'_> {
    fn read(&mut self, name: &str, state: &Assigned) {
        if let Some(assigned) = state
            && self.found.is_none()
            && self.locals.contains_key(name)
            && !assigned.contains(name)
        {
            self.found = Some(name.to_owned());
        }
    }

    fn expr(&mut self, expr: &AstExpr, state: &Assigned) {
        if let ExprP::Identifier(ident) = &expr.node {
            self.read(&ident.node.ident, state);
        }
        expr.visit_expr(|inner| self.expr(inner, state));
    }

    fn assign(name: &str, state: &mut Assigned) {
        if let Some(assigned) = state {
            assigned.insert(name.to_owned());
        }
    }

    fn join(state: &mut Assigned, other: Assigned) {
        *state = match (state.take(), other) {
            (Some(a), Some(b)) => Some(a.intersection(&b).cloned().collect()),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
    }

    /// The value of `expr` when it is an int fixed at ingestion: a literal, a top-level constant
    /// that no local hides, or a fold of those (the lowering folds the same).
    fn const_int(&self, expr: &AstExpr) -> Option<i64> {
        match &expr.node {
            ExprP::Literal(AstLiteral::Int(int)) => match &int.node {
                TokenInt::I32(n) => Some(i64::from(*n)),
                TokenInt::BigInt(big) => i64::try_from(big).ok(),
            },
            ExprP::Identifier(ident) => {
                let name = &ident.node.ident;
                match self.globals.get(name) {
                    Some(Const::Int(n)) if !self.locals.contains_key(name) => Some(*n),
                    _ => None,
                }
            }
            ExprP::Minus(inner) => self.const_int(inner)?.checked_neg(),
            ExprP::Op(lhs, op, rhs) => {
                let (a, b) = (self.const_int(lhs)?, self.const_int(rhs)?);
                match fold(*op, &Const::Int(a), &Const::Int(b)) {
                    Ok(Const::Int(n)) => Some(n),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether `over` is a `range` call whose bounds and step are constants and that yields at
    /// least one value.
    fn runs_at_least_once(&self, over: &AstExpr) -> bool {
        let ExprP::Call(function, args) = &over.node else {
            return false;
        };
        if !matches!(&function.node, ExprP::Identifier(f) if f.node.ident == "range")
            || self.locals.contains_key("range")
            || self.globals.contains_key("range")
        {
            return false;
        }
        let mut bounds = Vec::new();
        for arg in &args.args {
            match &arg.node {
                ArgumentP::Positional(expr) => match self.const_int(expr) {
                    Some(n) => bounds.push(n),
                    None => return false,
                },
                _ => return false,
            }
        }
        match bounds[..] {
            [stop] => 0 < stop,
            [start, stop] => start < stop,
            [start, stop, step] => (step > 0 && start < stop) || (step < 0 && start > stop),
            _ => false,
        }
    }

    fn stmt(&mut self, stmt: &AstStmt, state: &mut Assigned) {
        match &stmt.node {
            StmtP::Statements(list) => {
                for inner in list {
                    self.stmt(inner, state);
                }
            }
            StmtP::Expression(expr) => {
                self.expr(expr, state);
                if is_diag_fail(expr) {
                    *state = None;
                }
            }
            StmtP::Assign(assign) => {
                self.expr(&assign.rhs, state);
                if let AssignTargetP::Identifier(name) = &assign.lhs.node {
                    Self::assign(&name.node.ident, state);
                }
            }
            StmtP::AssignModify(target, _, rhs) => {
                if let AssignTargetP::Identifier(name) = &target.node {
                    self.read(&name.node.ident, state);
                }
                self.expr(rhs, state);
                if let AssignTargetP::Identifier(name) = &target.node {
                    Self::assign(&name.node.ident, state);
                }
            }
            StmtP::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, state);
                }
                *state = None;
            }
            StmtP::Break => {
                self.breaks += 1;
                *state = None;
            }
            StmtP::Continue => {
                if let Some(joined) = self.continues.last_mut() {
                    Self::join(joined, state.take());
                }
                *state = None;
            }
            StmtP::If(cond, then) => {
                self.expr(cond, state);
                let mut taken = state.clone();
                self.stmt(then, &mut taken);
                Self::join(state, taken);
            }
            StmtP::IfElse(cond, branches) => {
                self.expr(cond, state);
                let (mut then, mut otherwise) = (state.clone(), state.clone());
                self.stmt(&branches.0, &mut then);
                self.stmt(&branches.1, &mut otherwise);
                Self::join(&mut then, otherwise);
                *state = then;
            }
            StmtP::For(for_) => {
                self.expr(&for_.over, state);
                let mut inside = state.clone();
                if let AssignTargetP::Identifier(name) = &for_.var.node {
                    Self::assign(&name.node.ident, &mut inside);
                }
                let breaks = self.breaks;
                self.continues.push(None);
                self.stmt(&for_.body, &mut inside);
                let continued = self.continues.pop().flatten();
                if self.runs_at_least_once(&for_.over) && self.breaks == breaks {
                    // The end of an iteration: the body's fall-through and its `continue`s.
                    Self::join(&mut inside, continued);
                    *state = inside;
                }
            }
            StmtP::Pass | StmtP::Def(_) | StmtP::Load(_) => {}
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
        ("read_uint", [Const::Bytes(b), Const::Int(offset), Const::Int(bits)]) => {
            let field = uint_field(b.len(), *offset, *bits)?;
            Ok(Const::Int(
                b[field]
                    .iter()
                    .fold(0, |acc, &byte| (acc << 8) | i64::from(byte)),
            ))
        }
        (
            "write_uint",
            [
                Const::Bytes(b),
                Const::Int(offset),
                Const::Int(bits),
                Const::Int(v),
            ],
        ) => {
            let field = uint_field(b.len(), *offset, *bits)?;
            if !(0..1i64 << bits).contains(v) {
                return Err(format!("the value {v} does not fit {bits} bits"));
            }
            let mut out = b.to_vec();
            let width = field.len();
            out[field].copy_from_slice(&v.to_be_bytes()[8 - width..]);
            Ok(Const::bytes(out))
        }
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
            "`{name}` with these arguments is not evaluated at ingestion, and on values known \
             only at run time {} (2.4)",
            waits_for(name)
        )),
    }
}

/// The range of bytes a `read_uint` or `write_uint` of `bits` bits at `offset` covers in bytes
/// of length `len`, or why it is out of range (2.4). `bits` is checked before this.
fn uint_field(len: usize, offset: i64, bits: i64) -> Result<std::ops::Range<usize>, String> {
    let width = usize::try_from(bits / 8).expect("bits is checked");
    usize::try_from(offset)
        .ok()
        .and_then(|start| Some(start..start.checked_add(width)?))
        .filter(|field| field.end <= len)
        .ok_or_else(|| format!("{bits} bits at offset {offset} are out of range of {len} bytes"))
}

/// What a built-in or string method of 2.4 on a value known only at run time waits for, as the
/// end of the sentence `... {waits_for}`. Those that lower give no message here.
fn waits_for(name: &str) -> &'static str {
    match name {
        "len" => "needs a character-count instruction (a string is UTF-8 bytes at run time)",
        "abs" => "needs an absolute-value instruction",
        "bool" => "needs a truth-conversion instruction",
        "min" | "max" => "needs a minimum and maximum instruction",
        "str" => "needs an integer-to-text conversion instruction",
        "type" => "needs a type-name instruction",
        "range" => "is only lowered as the sequence of a `for`",
        "to_hex" => "needs a bytes-to-hex-text instruction",
        "from_hex" => "needs a hex-text-to-bytes instruction",
        "int" | "float" => "needs a text-to-number conversion instruction",
        "find" => "needs a string-search instruction",
        "startswith" | "endswith" => "needs a prefix and suffix comparison instruction",
        "upper" | "lower" => "needs a case-mapping instruction",
        "strip" => "needs a trimming instruction",
        "format" => "needs a text-formatting instruction",
        "join" => "needs a text-join instruction",
        _ => "is not a built-in of 2.4",
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
            "`.{method}` with these arguments is not evaluated at ingestion, and on values known \
             only at run time {} (2.4)",
            waits_for(method)
        )),
    }
}

#[cfg(test)]
#[path = "lower_tests.rs"]
mod tests;

/// The payload of a profile request `template` that no declared service decodes: its parts
/// after the SID, each constant or argument at its width, from `args`. The key, which only the
/// host's key step gives, is not part of it; `true` says it follows the payload, as the
/// argument that runs to the end of the request.
fn template_payload(template: &crate::profile::Request, args: &[(Source, u64)]) -> (Vec<u8>, bool) {
    let mut payload = Vec::new();
    let mut key = false;
    for part in template.parts {
        let (bits, value) = match *part {
            Part::Const { bits, value } => (bits, value),
            Part::Arg {
                arg: Source::Key, ..
            } => {
                key = true;
                continue;
            }
            Part::Arg { bits, arg } | Part::Select { bits, arg } => (
                bits,
                args.iter()
                    .find(|(source, _)| *source == arg)
                    .map(|&(_, value)| value)
                    .expect("the call gives each source its template uses"),
            ),
            Part::Params => {
                unreachable!("a validated profile gives `diag.security_access` no params")
            }
        };
        let width = usize::from(bits / 8);
        payload.extend_from_slice(&value.to_be_bytes()[8 - width..]);
    }
    (payload, key)
}

/// Whether `field` is a fixed-width `uint` of `bits` bits, a value sent or read as its own bits:
/// most significant byte first, as the profiles' values are (ADR-314 item 2).
fn is_fixed_uint(field: &diag_ir::declaration::Field<'_>, bits: u8) -> bool {
    field.kind() == diag_ir::declaration::DataKind::Uint
        && (bits <= 8 || field.byte_order() == diag_ir::declaration::ByteOrder::Big)
        && field.length_kind() == LengthKind::Fixed
        && field.bit_length() == u32::from(bits)
        && field.shape_type() == diag_ir::declaration::Shape::NONE
}
