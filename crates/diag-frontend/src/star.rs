//! Parsing a Starlark procedure and checking it against the subset (`docs/starlark-subset.md`
//! sections 2, 4.1, 4.2, 4.5, 4.6 and 5; ADR-254).
//!
//! [`check`] parses a `.star` file with `starlark_syntax` (only its parser and AST, never an
//! evaluator, section 6) and walks the AST with an allowlist: every node kind the subset accepts
//! is checked, and any other is rejected. All violations are listed; a syntax error stops the
//! parser, so a file that does not parse reports that one error. The comment annotations are
//! taken from the comment tokens the parser's lexer reports, never from raw source lines.
//!
//! The checker sees the syntax only. What needs the declaration part or the instruction set (a
//! `fields` key, a declared flash session, an instruction the IR lacks, the range of a constant
//! argument) is left to the lowering. Computed dict keys (2.3) are found on the expressions known
//! to be dicts without type inference: dict literals, the responses of `diag.request`,
//! `diag.routine` and `diag.read_dtc`, a response's `"fields"`, and names assigned from those.
//! A name assigned on only some paths (one branch of an `if`, the body of a `for`) counts as a
//! dict after it when it is one on any path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde::Serialize;

use crate::profile::Profile;
use diag_ir::{IdentitySources, PreconditionKind, Preconditions};
use starlark_syntax::codemap::Span;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignTargetP, AstArgument, AstAssignTarget, AstExpr, AstLiteral, AstNoPayload,
    AstParameter, AstStmt, BinOp, ClauseP, DefP, ExprP, ForClauseP, ParameterP, StmtP,
};
use starlark_syntax::syntax::{AstModule, Dialect, DialectTypes};

mod bounds;
mod lower;

pub use bounds::{
    MAX_BLOCK_DEPTH, MAX_BRACKET_DEPTH, MAX_ELIF_CHAIN, MAX_EXPRESSION_DEPTH, MAX_SOURCE_BYTES,
    PARSER_STACK_BYTES, on_parser_stack,
};
pub use lower::{Lowered, lower};

/// The services of one variant as `diag.request` selects among them (`csv-tables.md` 5.3,
/// ADR-313): what the IR does not keep, the names of each request's constants, beside the id of
/// the IR service the call then encodes and sends. The frontend builds it from the tables.
#[derive(Debug, Clone)]
pub struct RequestIndex {
    pub services: Vec<IndexedService>,
    /// The profile of the application layer the variant's protocols name, through which its
    /// protocol-neutral calls are lowered (ADR-309 item 3).
    pub profile: &'static Profile,
    /// The variant's `identity.csv`, which every program lowered for it carries (`csv-tables.md`
    /// 4.13, ADR-336 item 1). The services and fields are those of the declaration part.
    pub identity: IdentitySources,
    /// The variant's `preconditions.csv`; a program carries the ones its `# @preconditions` names
    /// (`csv-tables.md` 4.14, subset 4.9).
    pub preconditions: Preconditions,
}

/// No services and no restart facts, over UDS.
impl Default for RequestIndex {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            profile: &crate::profile::UDS_PROFILE,
            identity: IdentitySources::default(),
            preconditions: Preconditions::default(),
        }
    }
}

/// One service of a [`RequestIndex`].
#[derive(Debug, Clone)]
pub struct IndexedService {
    /// `Service.id` in the variant's declaration part.
    pub service_id: u32,
    pub sid: u8,
    /// A service `routines.csv` generates, which `diag.request` never selects.
    pub routine: bool,
    /// The request's constants by name, in order: the generated ones (`did` of a DID service)
    /// first, then those of the request's fields.
    pub constants: Vec<(String, RequestConstant)>,
}

/// The value of a request constant (`csv-tables.md` 5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestConstant {
    Uint(u64),
    Bytes(Vec<u8>),
}

/// The precondition a name of `# @preconditions` and of `diag.precondition` stands for (4.9,
/// 3.2); `None` for any other name. These spellings differ from `preconditions.csv`'s.
fn precondition_kind(name: &str) -> Option<PreconditionKind> {
    Some(match name {
        "voltage" => PreconditionKind::Voltage,
        "externalSupply" => PreconditionKind::ExternalSupply,
        "ignition" => PreconditionKind::Ignition,
        "engine" => PreconditionKind::Engine,
        "vehicleSpeed" => PreconditionKind::VehicleSpeed,
        _ => return None,
    })
}

/// The error codes of `docs/starlark-subset.md` that the checker and the lowering report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum StarCode {
    /// The file does not parse (section 5).
    #[serde(rename = "STAR_SYNTAX_ERROR")]
    SyntaxError,
    /// No `def main():` without parameters (2.1).
    #[serde(rename = "STAR_NO_MAIN")]
    NoMain,
    /// `lambda` or a nested `def` (2.3).
    #[serde(rename = "STAR_CLOSURE_NOT_ALLOWED")]
    ClosureNotAllowed,
    /// A function used as a value (2.3).
    #[serde(rename = "STAR_FUNCTION_VALUE_NOT_ALLOWED")]
    FunctionValueNotAllowed,
    /// A construct the subset does not accept (2.3, 2.5).
    #[serde(rename = "STAR_UNSUPPORTED_SYNTAX")]
    UnsupportedSyntax,
    /// `load` (2.3).
    #[serde(rename = "STAR_LOAD_NOT_ALLOWED")]
    LoadNotAllowed,
    /// `while` or recursion (2.3, 4.1).
    #[serde(rename = "STAR_UNBOUNDED_NOT_ALLOWED")]
    UnboundedNotAllowed,
    /// A top-level statement other than a constant assignment or a `def` (2.3).
    #[serde(rename = "STAR_TOPLEVEL_STATEMENT_NOT_ALLOWED")]
    ToplevelStatementNotAllowed,
    /// A dict indexed with a computed key (2.3, 4.1).
    #[serde(rename = "STAR_DYNAMIC_KEY_NOT_ALLOWED")]
    DynamicKeyNotAllowed,
    /// The `fail` built-in (2.3).
    #[serde(rename = "STAR_FAIL_NOT_ALLOWED")]
    FailNotAllowed,
    /// A built-in, method or `diag` function the subset does not provide (2.3, 2.4, 3.1).
    #[serde(rename = "STAR_BUILTIN_NOT_ALLOWED")]
    BuiltinNotAllowed,
    /// A malformed, unknown or unpaired annotation (4.2, 4.5, 4.6, 4.9).
    #[serde(rename = "STAR_BAD_ANNOTATION")]
    BadAnnotation,
    /// A name in `# @preconditions` that has no row in the variant's `preconditions.csv` (4.9).
    #[serde(rename = "STAR_UNKNOWN_PRECONDITION")]
    UnknownPrecondition,
    /// A `diag.ecu_info()` key whose value the variant's `identity.csv` has no row for (3.2).
    #[serde(rename = "STAR_UNKNOWN_IDENTITY")]
    UnknownIdentity,
    /// An annotation value outside its range (4.2, 4.5).
    #[serde(rename = "STAR_ARGUMENT_OUT_OF_RANGE")]
    ArgumentOutOfRange,
    /// A file larger than [`MAX_SOURCE_BYTES`] (2.8).
    #[serde(rename = "STAR_SOURCE_TOO_LARGE")]
    SourceTooLarge,
    /// A file nested past one of the bounds of 2.8.
    #[serde(rename = "STAR_NESTING_TOO_DEEP")]
    NestingTooDeep,
    /// An argument that must be known at ingestion is not (3.1).
    #[serde(rename = "STAR_NON_CONSTANT_ARGUMENT")]
    NonConstantArgument,
    /// A construct whose lowering needs an instruction the IR does not have, or that the
    /// lowering does not cover yet (2.5, 4.4).
    #[serde(rename = "STAR_NOT_LOWERABLE")]
    NotLowerable,
    /// A lowered program that `Program::validate` refuses for an instruction it names, reported
    /// at that instruction's source (4.5, ADR-336 item 4). A refusal that names none is
    /// `STAR_NOT_LOWERABLE`.
    #[serde(rename = "STAR_PLAN_INVALID")]
    PlanInvalid,
    /// Functions that need more specialized copies than the budget of 2.6 allows (4.5).
    #[serde(rename = "STAR_SPECIALIZATION_LIMIT")]
    SpecializationLimit,
    /// A `diag` function with no IR instruction yet (3.2).
    #[serde(rename = "STAR_API_NOT_AVAILABLE")]
    ApiNotAvailable,
    /// A `diag.request` call that no declared service of the variant matches, or that several
    /// match (3.4; `csv-tables.md` 5.3).
    #[serde(rename = "STAR_UNKNOWN_SERVICE")]
    UnknownService,
    /// A protocol-neutral call the profile of the variant's application layer does not define, a
    /// result name it does not map, or a declared response that does not fit its map (3.2, 3.4,
    /// ADR-309 item 3, ADR-314).
    #[serde(rename = "STAR_NOT_IN_PROFILE")]
    NotInProfile,
    /// A warning: a `range` loop with a bound known only at run time has no `@maxIterations`, so
    /// the default limit applies (4.2).
    #[serde(rename = "STAR_LOOP_LIMIT_DEFAULTED")]
    LoopLimitDefaulted,
}

impl StarCode {
    /// The code as the specification writes it, such as `STAR_NO_MAIN`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SyntaxError => "STAR_SYNTAX_ERROR",
            Self::NoMain => "STAR_NO_MAIN",
            Self::ClosureNotAllowed => "STAR_CLOSURE_NOT_ALLOWED",
            Self::FunctionValueNotAllowed => "STAR_FUNCTION_VALUE_NOT_ALLOWED",
            Self::UnsupportedSyntax => "STAR_UNSUPPORTED_SYNTAX",
            Self::LoadNotAllowed => "STAR_LOAD_NOT_ALLOWED",
            Self::UnboundedNotAllowed => "STAR_UNBOUNDED_NOT_ALLOWED",
            Self::ToplevelStatementNotAllowed => "STAR_TOPLEVEL_STATEMENT_NOT_ALLOWED",
            Self::DynamicKeyNotAllowed => "STAR_DYNAMIC_KEY_NOT_ALLOWED",
            Self::FailNotAllowed => "STAR_FAIL_NOT_ALLOWED",
            Self::BuiltinNotAllowed => "STAR_BUILTIN_NOT_ALLOWED",
            Self::BadAnnotation => "STAR_BAD_ANNOTATION",
            Self::UnknownPrecondition => "STAR_UNKNOWN_PRECONDITION",
            Self::UnknownIdentity => "STAR_UNKNOWN_IDENTITY",
            Self::ArgumentOutOfRange => "STAR_ARGUMENT_OUT_OF_RANGE",
            Self::SourceTooLarge => "STAR_SOURCE_TOO_LARGE",
            Self::NestingTooDeep => "STAR_NESTING_TOO_DEEP",
            Self::NonConstantArgument => "STAR_NON_CONSTANT_ARGUMENT",
            Self::NotLowerable => "STAR_NOT_LOWERABLE",
            Self::PlanInvalid => "STAR_PLAN_INVALID",
            Self::SpecializationLimit => "STAR_SPECIALIZATION_LIMIT",
            Self::ApiNotAvailable => "STAR_API_NOT_AVAILABLE",
            Self::UnknownService => "STAR_UNKNOWN_SERVICE",
            Self::NotInProfile => "STAR_NOT_IN_PROFILE",
            Self::LoopLimitDefaulted => "STAR_LOOP_LIMIT_DEFAULTED",
        }
    }
}

impl std::fmt::Display for StarCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One violation, in the form of section 5.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct StarError {
    pub code: StarCode,
    /// The file's name.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
    /// The source line, without its line end; of a line longer than [`SNIPPET_CHARS`]
    /// characters, only that many around the column.
    pub snippet: String,
    /// What is wrong, for a person.
    pub hint: String,
}

/// The `interruptible` value of a `@section` annotation (4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Interruptible,
    Uninterruptible,
    RecoveryRequired,
}

/// The value of an `@idempotency` annotation (4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    Safe,
    CheckState,
    Unsafe,
}

/// A checked annotation, by the 1-based line of its comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// `@maxIterations n`, attached to the `for` statement that follows (4.2).
    MaxIterations {
        line: u32,
        /// The line of the `for`.
        for_line: u32,
        limit: u32,
    },
    /// `@section kind [expected=millis]`, which makes the whole top-level `def` that follows it a
    /// section, together with every function that `def` calls (4.5, ADR-301).
    Section {
        line: u32,
        /// The line of the `def`.
        def: u32,
        kind: SectionKind,
        expected_millis: Option<u32>,
    },
    /// `@idempotency value`, attached to the statement that follows (4.6).
    Idempotency {
        line: u32,
        /// The byte offset where that statement begins: of the statements that share its line,
        /// only the first is annotated.
        statement_at: u32,
        value: Idempotency,
    },
    /// `@preconditions name ...`, directly before `def main()` (4.9).
    Preconditions {
        line: u32,
        /// The comment, where a name without a row in the variant's table is reported.
        span: Span,
        /// The preconditions the procedure requires, in the order written.
        kinds: Vec<PreconditionKind>,
    },
}

/// A procedure that passed the checks: its AST and annotations, for the lowering. The AST nests
/// no deeper than the bounds of 2.8 allow, which a recursive walk over it can rely on only on a
/// thread with a [`PARSER_STACK_BYTES`] stack ([`on_parser_stack`]); dropping it needs much less.
#[derive(Debug)]
pub struct Checked {
    pub module: AstModule,
    /// In source order.
    pub annotations: Vec<Annotation>,
}

/// The built-in functions of 2.4.
const BUILTINS: &[&str] = &[
    "len",
    "range",
    "int",
    "float",
    "str",
    "bool",
    "abs",
    "min",
    "max",
    "type",
    "to_hex",
    "from_hex",
    "read_uint",
    "write_uint",
];

/// The string methods of 2.4.
const STRING_METHODS: &[&str] = &[
    "startswith",
    "endswith",
    "find",
    "upper",
    "lower",
    "strip",
    "format",
    "join",
];

/// The functions of the `diag` module (3.1).
const DIAG_FUNCTIONS: &[&str] = &[
    "request",
    "read_dtc",
    "routine",
    "security_access",
    "flash_transfer",
    "wait",
    "hmi",
    "record",
    "capture",
    "log",
    "ecu_info",
    "precondition",
    "fail",
];

/// The `diag` functions whose result is a response dict (3.4).
const DIAG_RESPONSES: &[&str] = &["request", "routine", "read_dtc"];

/// Built-ins 2.3 names as not allowed.
const FORBIDDEN_BUILTINS: &[&str] = &["print", "getattr", "hasattr", "dir"];

/// The other Starlark built-in functions, which 2.4 leaves out: calling one is rejected as not a
/// built-in of 2.4, and naming one as a value as a function value (2.3).
const EXCLUDED_BUILTINS: &[&str] = &[
    "all",
    "any",
    "bytes",
    "chr",
    "dict",
    "enumerate",
    "hash",
    "list",
    "ord",
    "repr",
    "reversed",
    "set",
    "sorted",
    "tuple",
    "zip",
];

/// The dialect the file is parsed with: everything the subset rejects with its own code is
/// parsed, so that the checker can report it with that code rather than as a syntax error.
fn dialect() -> Dialect {
    Dialect {
        enable_def: true,
        enable_lambda: true,
        enable_load: true,
        enable_keyword_only_arguments: true,
        enable_positional_only_arguments: true,
        enable_types: DialectTypes::ParseOnly,
        enable_load_reexport: false,
        enable_top_level_stmt: true,
        enable_f_strings: true,
        ..Dialect::Standard
    }
}

/// [`check`] on a procedure file's bytes as stored. A file past the size bound of 2.8 reports
/// that first, as [`check`] does; a file within it that is not UTF-8 is not parsed and reports
/// `STAR_SYNTAX_ERROR` where its first byte that is not UTF-8 is (2.1).
pub fn check_file(file: &str, bytes: &[u8]) -> Result<Checked, Vec<StarError>> {
    if bytes.len() > MAX_SOURCE_BYTES {
        // Placed at the bound, within the bytes before it that are UTF-8.
        let head = &bytes[..MAX_SOURCE_BYTES];
        let valid = match std::str::from_utf8(head) {
            Ok(valid) => valid,
            Err(error) => std::str::from_utf8(&head[..error.valid_up_to()]).unwrap_or_default(),
        };
        return Err(vec![located(
            StarCode::SourceTooLarge,
            file,
            valid,
            valid.len(),
            format!("the file is larger than {MAX_SOURCE_BYTES} bytes (2.8)"),
        )]);
    }
    match std::str::from_utf8(bytes) {
        Ok(source) => check(file, source),
        Err(error) => {
            // The bytes before the error are valid UTF-8, so the error is placed at their end.
            let valid = std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default();
            Err(vec![located(
                StarCode::SyntaxError,
                file,
                valid,
                valid.len(),
                "the file is not UTF-8 (2.1)".to_owned(),
            )])
        }
    }
}

/// Parses `source`, the contents of the procedure file `file`, and checks it against the subset.
/// A file past the size or nesting bounds of 2.8 is refused before it is parsed; any other is
/// parsed and checked on a thread of its own ([`on_parser_stack`]).
pub fn check(file: &str, source: &str) -> Result<Checked, Vec<StarError>> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(vec![located(
            StarCode::SourceTooLarge,
            file,
            source,
            MAX_SOURCE_BYTES,
            format!("the file is larger than {MAX_SOURCE_BYTES} bytes (2.8)"),
        )]);
    }
    on_parser_stack(|| check_bounded(file, source))
}

/// [`check`] on a file within the size bound.
fn check_bounded(file: &str, source: &str) -> Result<Checked, Vec<StarError>> {
    let dialect = dialect();
    if let Err(exceeded) = bounds::prescan(source, &dialect) {
        return Err(vec![located(
            StarCode::NestingTooDeep,
            file,
            source,
            exceeded.at,
            exceeded.hint,
        )]);
    }
    let module = match AstModule::parse(file, source.to_owned(), &dialect) {
        Ok(module) => module,
        Err(error) => return Err(vec![syntax_error(file, source, &error)]),
    };
    let mut checker = Checker {
        module: &module,
        errors: Vec::new(),
        defs: HashSet::new(),
        def_ids: HashMap::new(),
        next_function: 1,
        globals: HashSet::new(),
        global_kinds: HashMap::new(),
        calls: Vec::new(),
        components: Vec::new(),
        quiet: 0,
        loops: Vec::new(),
        loop_states: HashMap::new(),
    };
    checker.top_level();
    checker.recursion();
    let annotations = checker.annotations();
    let pending = checker.errors;
    if pending.is_empty() {
        return Ok(Checked {
            module,
            annotations,
        });
    }
    Err(resolve(file, &module, pending))
}

/// A violation found by the walk, located by its span until all are resolved together.
struct Pending {
    code: StarCode,
    span: Span,
    hint: String,
}

/// Turns the violations into errors in source order. The character column is counted in one
/// sweep over the sorted positions, from the previous error on the same line, so many errors on
/// one long line cost time linear in the line rather than one count from its start each.
fn resolve(file: &str, module: &AstModule, mut pending: Vec<Pending>) -> Vec<StarError> {
    // Stable, so errors at one position keep the order they were found in.
    pending.sort_by_key(|p| p.span.begin());
    let codemap = module.file_span(module.statement().span).file;
    let source = codemap.source();
    let mut current = usize::MAX;
    let (mut byte, mut column) = (0, 0);
    pending
        .into_iter()
        .map(|p| {
            let line = codemap.find_line(p.span.begin());
            if line != current {
                current = line;
                byte = codemap.line_span(line).begin().get() as usize;
                column = 0;
            }
            let at = p.span.begin().get() as usize;
            column += source[byte..at].chars().count();
            byte = at;
            let text = codemap.source_line(line);
            let line_start = codemap.line_span(line).begin().get() as usize;
            StarError {
                code: p.code,
                file: file.to_owned(),
                line: to_u32(line + 1),
                column: to_u32(column + 1),
                snippet: snippet(text, at - line_start),
                hint: p.hint,
            }
        })
        .collect()
}

/// An error at the byte `at` of `source`, located without a parse.
fn located(code: StarCode, file: &str, source: &str, at: usize, hint: String) -> StarError {
    let mut at = at.min(source.len());
    while !source.is_char_boundary(at) {
        at -= 1;
    }
    let line_start = source[..at].rfind('\n').map_or(0, |n| n + 1);
    let line_end = source[at..].find('\n').map_or(source.len(), |n| at + n);
    let text = source[line_start..line_end].trim_end_matches('\r');
    StarError {
        code,
        file: file.to_owned(),
        line: to_u32(source[..line_start].matches('\n').count() + 1),
        column: to_u32(source[line_start..at].chars().count() + 1),
        snippet: snippet(text, at - line_start),
        hint,
    }
}

/// The one error of a file that does not parse. `while` is a reserved word that the parser
/// refuses, so it is reported with the code 2.3 gives it.
fn syntax_error(file: &str, source: &str, error: &starlark_syntax::Error) -> StarError {
    let Some(span) = error.span() else {
        return StarError {
            code: StarCode::SyntaxError,
            file: file.to_owned(),
            line: 1,
            column: 1,
            snippet: snippet(source.lines().next().unwrap_or_default(), 0),
            hint: error.without_diagnostic().to_string(),
        };
    };
    let resolved = span.resolve_span();
    let text = span.file.source_span(span.span);
    let (code, hint) = if text == "while" {
        (
            StarCode::UnboundedNotAllowed,
            "`while` is not allowed; use a bounded `for` with `break` (4.2)".to_owned(),
        )
    } else {
        (
            StarCode::SyntaxError,
            error.without_diagnostic().to_string(),
        )
    };
    StarError {
        code,
        file: file.to_owned(),
        line: to_u32(resolved.begin.line + 1),
        column: to_u32(resolved.begin.column + 1),
        snippet: snippet(
            span.file.source_line(resolved.begin.line),
            line_offset(&span.file, span.span),
        ),
        hint,
    }
}

/// The longest snippet an error carries, in characters, so that many violations on one long line
/// do not each copy the whole line.
pub const SNIPPET_CHARS: usize = 160;

/// The part of `line` an error at the byte `offset` in it shows: all of it when it is short,
/// otherwise [`SNIPPET_CHARS`] characters around the offset. Only those characters are visited,
/// so many errors on one long line cost what their snippets do.
fn snippet(line: &str, offset: usize) -> String {
    if line.len() <= SNIPPET_CHARS {
        return line.to_owned();
    }
    let mut offset = offset.min(line.len());
    while !line.is_char_boundary(offset) {
        offset -= 1;
    }
    let before: Vec<usize> = line[..offset]
        .char_indices()
        .rev()
        .take(SNIPPET_CHARS)
        .map(|(at, _)| at)
        .collect();
    let after: Vec<usize> = line[offset..]
        .char_indices()
        .take(SNIPPET_CHARS + 1)
        .map(|(at, _)| offset + at)
        .collect();
    let after_chars = after.len().min(SNIPPET_CHARS);
    // Half the window before the offset, more when the line ends sooner after it.
    let back = before
        .len()
        .min((SNIPPET_CHARS / 2).max(SNIPPET_CHARS - after_chars));
    let start = if back == 0 { offset } else { before[back - 1] };
    let end = after
        .get(SNIPPET_CHARS - back)
        .copied()
        .unwrap_or(line.len());
    line[start..end].to_owned()
}

/// The byte offset of `span`'s start within its line.
fn line_offset(file: &starlark_syntax::codemap::CodeMap, span: Span) -> usize {
    let line = file.find_line(span.begin());
    (span.begin().get() - file.line_span(line).begin().get()) as usize
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A call from one function to another, for the recursion check (4.1). Functions are numbered,
/// so a call holds no name but the callee's as the call writes it, for the hint.
struct Call {
    caller: usize,
    callee: usize,
    written: String,
    span: Span,
}

struct Checker<'a> {
    module: &'a AstModule,
    errors: Vec<Pending>,
    /// The names of the top-level `def`s.
    defs: HashSet<String>,
    /// The number of each top-level `def` in the call graph.
    def_ids: HashMap<String, usize>,
    /// The number the next function gets in the call graph; 0 is the top level.
    next_function: usize,
    /// The top-level constants.
    globals: HashSet<String>,
    /// What the top-level constants are known to be.
    global_kinds: HashMap<String, Kinds>,
    calls: Vec<Call>,
    /// The strongly connected component of each function in the call graph, once
    /// [`Checker::recursion`] has run.
    components: Vec<usize>,
    /// Above 0 while a loop body is walked only to find what its names are bound to: nothing is
    /// reported then.
    quiet: u32,
    /// The bindings `break` and `continue` carry, for each loop being walked, innermost last.
    loops: Vec<Jumps>,
    /// What each loop's fixpoint has found so far, by the loop's position. A loop nested in
    /// another is walked again on each pass of the outer one; its state there only grows, so it
    /// is kept and extended rather than computed afresh, which bounds the passes of every loop
    /// by the number of bindings that can change, whatever the nesting depth.
    loop_states: HashMap<u32, LoopState>,
}

/// The bindings a loop's `break`s and `continue`s carry; `None` while none is reachable.
#[derive(Default)]
struct Jumps {
    breaks: Option<Bindings>,
    continues: Option<Bindings>,
}

/// What a loop's fixpoint found: the bindings before the loop it covers, at the start of a pass,
/// at its `break`s and after the loop.
#[derive(Clone, Default)]
struct LoopState {
    entry: Bindings,
    head: Bindings,
    breaks: Option<Bindings>,
    exit: Bindings,
}

/// Whether every binding of `part` is in `whole`.
fn covers(whole: &HashMap<String, Kinds>, part: &HashMap<String, Kinds>) -> bool {
    part.iter().all(|(name, kinds)| {
        let have = whole.get(name).copied().unwrap_or_default();
        have.union(*kinds) == have
    })
}

/// Adds `kinds` to the bindings of a jump, which are unset while no jump is reachable.
fn join_jump(into: &mut Option<Bindings>, kinds: Bindings) {
    match into {
        Some(into) => merge(into, kinds),
        None => *into = Some(kinds),
    }
}

/// What an expression is known to be, for the computed-key check (2.3). A name can be bound to
/// different things on different paths, so both are kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Kinds {
    /// A response of `diag.request`, `diag.routine` or `diag.read_dtc` (3.4).
    response: bool,
    /// Any other dict: a literal or a response's `"fields"`.
    dict: bool,
}

impl Kinds {
    const RESPONSE: Self = Self {
        response: true,
        dict: false,
    };
    const DICT: Self = Self {
        response: false,
        dict: true,
    };

    fn is_dict(self) -> bool {
        self.response || self.dict
    }

    fn union(self, other: Self) -> Self {
        Self {
            response: self.response || other.response,
            dict: self.dict || other.dict,
        }
    }
}

/// What the locals are bound to at one point. Shared until changed, so that the copies a branch
/// or a loop takes of the bindings before it cost nothing while nothing in it rebinds a name.
type Bindings = Rc<HashMap<String, Kinds>>;

/// Adds the bindings of `other`, a state reached along another path, to `into`. Nothing is
/// copied when `other` adds nothing.
fn merge(into: &mut Bindings, other: Bindings) {
    if Rc::ptr_eq(into, &other) || other.is_empty() {
        return;
    }
    if into.is_empty() {
        *into = other;
        return;
    }
    if covers(into, &other) {
        return;
    }
    let into = Rc::make_mut(into);
    for (name, kinds) in other.iter() {
        let entry = into.entry(name.clone()).or_default();
        *entry = entry.union(*kinds);
    }
}

/// Removes `name` from the bindings, copying them only when it is there.
fn unbind(bindings: &mut Bindings, name: &str) {
    if bindings.contains_key(name) {
        Rc::make_mut(bindings).remove(name);
    }
}

/// The names one function, lambda or comprehension binds, linked to those of the one it is
/// nested in. Each holds only its own names, so entering a nested scope copies nothing.
#[derive(Default, Clone)]
struct Frame {
    /// The parameters and the names it assigns, which hide the names of the scopes around it,
    /// the top-level names and the built-ins (2.2).
    locals: HashSet<String>,
    /// Its nested `def`s, already reported where they are defined (2.3), with the number each
    /// has in the call graph.
    closures: HashMap<String, usize>,
    parent: Option<Rc<Frame>>,
}

/// What a name means in a scope, found from the innermost frame out.
enum Lookup {
    Local,
    Closure(usize),
    /// Not bound in any enclosing function: a top-level name, a built-in or nothing.
    Outside,
}

/// The names of the function being checked.
#[derive(Default, Clone)]
struct Scope {
    /// The number of the function in the call graph (for a lambda or comprehension, of the one
    /// it is in).
    function: usize,
    frame: Rc<Frame>,
    /// What the locals are known to be at the point being checked; a local missing here is not a
    /// dict.
    kinds: Bindings,
    /// The point being checked follows a `return`, `break` or `continue` on every path, so its
    /// bindings reach nothing. Its syntax is still checked.
    dead: bool,
}

impl Scope {
    fn resolve(&self, name: &str) -> Lookup {
        let mut frame = Some(&self.frame);
        while let Some(current) = frame {
            if current.locals.contains(name) {
                return Lookup::Local;
            }
            if let Some(&id) = current.closures.get(name) {
                return Lookup::Closure(id);
            }
            frame = current.parent.as_ref();
        }
        Lookup::Outside
    }

    fn is_local(&self, name: &str) -> bool {
        matches!(self.resolve(name), Lookup::Local)
    }

    /// Joins the state `(kinds, dead)` reached along another path into this one.
    fn join(&mut self, kinds: Bindings, dead: bool) {
        if dead {
            return;
        }
        if self.dead {
            self.kinds = kinds;
            self.dead = false;
        } else {
            merge(&mut self.kinds, kinds);
        }
    }
}

impl Scope {
    /// The scope of the function numbered `function` nested in `outer`, with its parameters and,
    /// for a `def`, its body; its nested `def`s are numbered from `next_function`.
    fn enter(
        outer: &Scope,
        function: usize,
        params: &[AstParameter],
        body: Option<&AstStmt>,
        next_function: &mut usize,
    ) -> Self {
        let mut own = HashSet::new();
        for param in params {
            if let ParameterP::Normal(ident, ..)
            | ParameterP::Args(ident, _)
            | ParameterP::KwArgs(ident, _) = &param.node
            {
                own.insert(ident.node.ident.clone());
            }
        }
        let mut nested = HashSet::new();
        if let Some(body) = body {
            collect_locals(body, &mut own, &mut nested);
        }
        // A name both assigned and defined by a nested `def` is a local.
        let closures = nested
            .into_iter()
            .filter(|inner| !own.contains(inner))
            .map(|inner| {
                let id = *next_function;
                *next_function += 1;
                (inner, id)
            })
            .collect();
        let mut kinds = outer.kinds.clone();
        for local in &own {
            unbind(&mut kinds, local);
        }
        Scope {
            function,
            frame: Rc::new(Frame {
                locals: own,
                closures,
                parent: Some(outer.frame.clone()),
            }),
            kinds,
            dead: false,
        }
    }
}

impl Checker<'_> {
    fn error(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        if self.quiet > 0 {
            return;
        }
        self.errors.push(Pending {
            code,
            span,
            hint: hint.into(),
        });
    }

    /// The statements of a block, with the groupings (`a = 1; b = 2` on one line) flattened.
    fn statements(stmt: &AstStmt) -> Vec<&AstStmt> {
        match &stmt.node {
            StmtP::Statements(list) => list.iter().flat_map(Self::statements).collect(),
            _ => vec![stmt],
        }
    }

    /// The top level holds constant assignments and `def`s only (2.1, 2.3); `main` must exist.
    fn top_level(&mut self) {
        let module = self.module;
        let top = Self::statements(module.statement());
        // The names first, so that a call to a function defined further down resolves. A
        // top-level name is bound once, so each has one meaning throughout the file.
        let mut bound = HashSet::new();
        for stmt in &top {
            let name = match &stmt.node {
                StmtP::Def(def) => {
                    self.defs.insert(def.name.node.ident.clone());
                    let id = self.next_function;
                    self.next_function += 1;
                    self.def_ids.insert(def.name.node.ident.clone(), id);
                    &def.name
                }
                StmtP::Assign(assign) => match &assign.lhs.node {
                    AssignTargetP::Identifier(name) => {
                        self.globals.insert(name.node.ident.clone());
                        name
                    }
                    _ => continue,
                },
                _ => continue,
            };
            if !bound.insert(name.node.ident.clone()) {
                self.error(
                    StarCode::UnsupportedSyntax,
                    name.span,
                    format!(
                        "`{}` is already bound at the top level; a top-level name is bound once \
                         (2.1)",
                        name.node.ident
                    ),
                );
            }
        }
        // In order, so that a constant assigned from another takes what that one is known to be.
        for stmt in &top {
            if let StmtP::Assign(assign) = &stmt.node
                && let AssignTargetP::Identifier(name) = &assign.lhs.node
            {
                let kinds = self.kinds(&assign.rhs, &Scope::default());
                if kinds.is_dict() {
                    self.global_kinds.insert(name.node.ident.clone(), kinds);
                } else {
                    self.global_kinds.remove(&name.node.ident);
                }
            }
        }
        let mut has_main = false;
        for (at, stmt) in top.iter().enumerate() {
            match &stmt.node {
                StmtP::Def(def) => {
                    let name = def.name.node.ident.clone();
                    if name == "main" && def.params.is_empty() {
                        has_main = true;
                    }
                    if name == "main" && !def.params.is_empty() {
                        self.error(
                            StarCode::NoMain,
                            def.name.span,
                            "`main` takes no parameters (2.1)",
                        );
                    }
                    let id = self.def_ids[&name];
                    self.function(&Scope::default(), id, def);
                }
                StmtP::Assign(assign) => {
                    if !matches!(&assign.lhs.node, AssignTargetP::Identifier(_)) {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            assign.lhs.span,
                            "a top-level constant is assigned to a single name (2.1)",
                        );
                        self.target_parts(&assign.lhs, &mut Scope::default());
                    }
                    if let Some(ty) = &assign.ty {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            ty.span,
                            "type annotations are not supported (2.3)",
                        );
                    }
                    self.constant(&assign.rhs);
                }
                StmtP::Load(_) => self.error(
                    StarCode::LoadNotAllowed,
                    stmt.span,
                    "`load` is not allowed: one file is one procedure (2.3)",
                ),
                // A module docstring.
                StmtP::Expression(_) if at == 0 && is_string_expression(stmt) => {}
                _ => {
                    self.error(
                        StarCode::ToplevelStatementNotAllowed,
                        stmt.span,
                        "the top level holds only constant assignments and `def`s (2.1)",
                    );
                    // What it holds is checked too, so that every violation is listed (5).
                    let mut scope = Scope::enter(
                        &Scope::default(),
                        0,
                        &[],
                        Some(stmt),
                        &mut self.next_function,
                    );
                    self.stmt(stmt, &mut scope);
                }
            }
        }
        if !has_main {
            let span = module.statement().span;
            self.error(
                StarCode::NoMain,
                Span::new(span.begin(), span.begin()),
                "the procedure needs `def main():` without parameters (2.1)",
            );
        }
    }

    /// A top-level constant's value: an expression without calls to `diag` or to the file's
    /// functions, which run only from `main` (2.1).
    fn constant(&mut self, expr: &AstExpr) {
        let mut found = Vec::new();
        self.collect_runtime_calls(expr, &mut found);
        for span in found {
            self.error(
                StarCode::ToplevelStatementNotAllowed,
                span,
                "a top-level constant cannot call `diag` or a function of this file (2.1)",
            );
        }
        self.expr(expr, &mut Scope::default(), false);
    }

    /// The calls in a top-level constant that can only run from `main`: to `diag` and to the
    /// file's functions.
    fn collect_runtime_calls(&self, expr: &AstExpr, found: &mut Vec<Span>) {
        if let ExprP::Call(function, _) = &expr.node {
            let runtime = match &function.node {
                ExprP::Identifier(name) => self.defs.contains(&name.node.ident),
                ExprP::Dot(object, _) => self.is_diag(object, &Scope::default()),
                _ => false,
            };
            if runtime {
                found.push(function.span);
            }
        }
        expr.visit_expr(|inner| self.collect_runtime_calls(inner, found));
    }

    /// A `def`'s parameters and body. `name` is the top-level function it belongs to.
    fn function(&mut self, outer: &Scope, function: usize, def: &DefP<AstNoPayload>) {
        self.params(&def.params, &mut outer.clone());
        if let Some(ty) = &def.return_type {
            self.error(
                StarCode::UnsupportedSyntax,
                ty.span,
                "type annotations are not supported (2.3)",
            );
        }
        let mut scope = Scope::enter(
            outer,
            function,
            &def.params,
            Some(&def.body),
            &mut self.next_function,
        );
        for (index, inner) in Self::statements(&def.body).into_iter().enumerate() {
            // A docstring opens the body.
            if index == 0 && is_string_expression(inner) {
                continue;
            }
            self.stmt(inner, &mut scope);
        }
    }

    /// Positional parameters only, without defaults or types (2.3).
    /// A default value is checked too, in `defining`, the scope the function is defined in.
    fn params(&mut self, params: &[AstParameter], defining: &mut Scope) {
        for param in params {
            match &param.node {
                ParameterP::Normal(_, ty, default) => {
                    if let Some(ty) = ty {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            ty.span,
                            "type annotations are not supported (2.3)",
                        );
                    }
                    if let Some(default) = default {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            param.span,
                            "default parameter values are not supported (2.3)",
                        );
                        self.expr(default, defining, false);
                    }
                }
                ParameterP::Args(_, ty) | ParameterP::KwArgs(_, ty) => {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        param.span,
                        "`*args` and `**kwargs` are not supported (2.3)",
                    );
                    if let Some(ty) = ty {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            ty.span,
                            "type annotations are not supported (2.3)",
                        );
                    }
                }
                ParameterP::NoArgs | ParameterP::Slash => self.error(
                    StarCode::UnsupportedSyntax,
                    param.span,
                    "keyword-only and positional-only markers are not supported (2.3)",
                ),
            }
        }
    }

    fn stmt(&mut self, stmt: &AstStmt, scope: &mut Scope) {
        match &stmt.node {
            StmtP::Pass => {}
            // A jump carries the bindings to the loop's exit or head; what follows it in the
            // block is not reached.
            StmtP::Break | StmtP::Continue => {
                if !scope.dead
                    && let Some(jumps) = self.loops.last_mut()
                {
                    let kinds = scope.kinds.clone();
                    if matches!(stmt.node, StmtP::Break) {
                        join_jump(&mut jumps.breaks, kinds);
                    } else {
                        join_jump(&mut jumps.continues, kinds);
                    }
                }
                scope.dead = true;
            }
            StmtP::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, scope, false);
                }
                scope.dead = true;
            }
            StmtP::Expression(expr) => self.expr(expr, scope, false),
            StmtP::Assign(assign) => {
                if let Some(ty) = &assign.ty {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        ty.span,
                        "type annotations are not supported (2.3)",
                    );
                }
                self.expr(&assign.rhs, scope, false);
                let kinds = self.kinds(&assign.rhs, scope);
                if let Some(name) = self.target(&assign.lhs, scope) {
                    if kinds.is_dict() {
                        // A binding that is already this leaves the shared state as it is.
                        if scope.kinds.get(&name) != Some(&kinds) {
                            Rc::make_mut(&mut scope.kinds).insert(name, kinds);
                        }
                    } else {
                        unbind(&mut scope.kinds, &name);
                    }
                }
            }
            StmtP::AssignModify(target, _, value) => {
                self.target(target, scope);
                self.expr(value, scope, false);
            }
            StmtP::Statements(list) => {
                for inner in list {
                    self.stmt(inner, scope);
                }
            }
            // Each branch starts from the bindings before the `if`, and what follows sees the
            // bindings of every path, the skipped one included.
            StmtP::If(condition, then) => {
                self.expr(condition, scope, false);
                let (entry, dead) = (scope.kinds.clone(), scope.dead);
                self.stmt(then, scope);
                scope.join(entry, dead);
            }
            StmtP::IfElse(condition, branches) => {
                self.expr(condition, scope, false);
                let (entry, dead) = (scope.kinds.clone(), scope.dead);
                self.stmt(&branches.0, scope);
                let then = std::mem::replace(&mut scope.kinds, entry);
                let then_dead = std::mem::replace(&mut scope.dead, dead);
                self.stmt(&branches.1, scope);
                if scope.dead {
                    scope.kinds = then;
                    scope.dead = then_dead;
                } else {
                    scope.join(then, then_dead);
                }
            }
            StmtP::For(for_) => {
                // The iterable is evaluated once, with the bindings before the loop.
                self.expr(&for_.over, scope, false);
                if scope.dead {
                    // A loop that is never reached is checked for its syntax only.
                    if self.quiet == 0 {
                        let entry = scope.kinds.clone();
                        self.loops.push(Jumps::default());
                        self.loop_body(&for_.var, &for_.body, scope);
                        self.loops.pop();
                        scope.kinds = entry;
                        scope.dead = true;
                    }
                    return;
                }
                let state = self.loop_state(stmt.span.begin().get(), &for_.var, &for_.body, scope);
                if self.quiet == 0 {
                    // The body is checked once, with every binding a pass can start from.
                    scope.kinds = state.head;
                    scope.dead = false;
                    self.loops.push(Jumps::default());
                    self.loop_body(&for_.var, &for_.body, scope);
                    self.loops.pop();
                    // After an outermost loop no fixpoint can come back to it or to the loops
                    // in it, so their states are dropped rather than kept to the end of the
                    // file.
                    if self.loops.is_empty() {
                        self.loop_states.clear();
                    }
                }
                scope.kinds = state.exit;
                scope.dead = false;
            }
            StmtP::Def(def) => {
                // A nested `def` is a scope of its own and changes no binding around it, so a
                // quiet pass skips it; the reporting pass checks it once.
                if self.quiet > 0 {
                    return;
                }
                self.error(
                    StarCode::ClosureNotAllowed,
                    def.name.span,
                    "a nested `def` is not allowed; write it as a top-level `def` (2.3)",
                );
                // The number `Scope::enter` gave it, which its calls use too.
                let id = match scope.resolve(&def.name.node.ident) {
                    Lookup::Closure(id) => id,
                    _ => {
                        self.next_function += 1;
                        self.next_function - 1
                    }
                };
                self.function(scope, id, def);
            }
            StmtP::Load(_) => self.error(
                StarCode::LoadNotAllowed,
                stmt.span,
                "`load` is not allowed (2.3)",
            ),
        }
    }

    /// Walks a loop's body quietly until the bindings at the start of a pass stop changing. A
    /// pass starts from the bindings before the loop, at the end of a pass or at a `continue`;
    /// the loop ends with those or at a `break`.
    fn loop_state(
        &mut self,
        at: u32,
        var: &AstAssignTarget,
        body: &AstStmt,
        scope: &mut Scope,
    ) -> LoopState {
        let mut state = self.loop_states.get(&at).cloned().unwrap_or_default();
        if self.loop_states.contains_key(&at) && covers(&state.entry, &scope.kinds) {
            return state;
        }
        merge(&mut state.entry, scope.kinds.clone());
        let mut head = state.head;
        merge(&mut head, scope.kinds.clone());
        let mut breaks = state.breaks;
        loop {
            scope.kinds = head.clone();
            scope.dead = false;
            self.quiet += 1;
            self.loops.push(Jumps::default());
            self.loop_body(var, body, scope);
            let jumps = self.loops.pop().unwrap_or_default();
            self.quiet -= 1;
            let mut next = head.clone();
            if !scope.dead {
                merge(&mut next, std::mem::take(&mut scope.kinds));
            }
            if let Some(continues) = jumps.continues {
                merge(&mut next, continues);
            }
            if let Some(found) = jumps.breaks {
                join_jump(&mut breaks, found);
            }
            if Rc::ptr_eq(&next, &head) || next == head {
                break;
            }
            head = next;
        }
        // The loop ends when its iterable runs out, at the head, or at a `break`.
        let mut exit = head.clone();
        if let Some(breaks) = &breaks {
            merge(&mut exit, breaks.clone());
        }
        let state = LoopState {
            entry: state.entry,
            head,
            breaks,
            exit,
        };
        self.loop_states.insert(at, state.clone());
        state
    }

    /// One pass of a `for` body: the target is bound to an element, which is not known to be a
    /// dict.
    fn loop_body(&mut self, var: &AstAssignTarget, body: &AstStmt, scope: &mut Scope) {
        if let Some(name) = self.target(var, scope) {
            unbind(&mut scope.kinds, &name);
        }
        self.stmt(body, scope);
    }

    /// An assignment target: a single name. Returns the name.
    fn target(&mut self, target: &AstAssignTarget, scope: &mut Scope) -> Option<String> {
        match &target.node {
            AssignTargetP::Identifier(name) => Some(name.node.ident.clone()),
            AssignTargetP::Tuple(items) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    target.span,
                    "unpacking assignment is not supported; assign each name (2.2)",
                );
                for item in items {
                    if let Some(name) = self.target(item, scope) {
                        unbind(&mut scope.kinds, &name);
                    }
                }
                None
            }
            AssignTargetP::Index(_) | AssignTargetP::Dot(..) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    target.span,
                    "assigning to an element or attribute is not supported: values are not \
                     mutated (2.5)",
                );
                self.target_parts(target, scope);
                None
            }
        }
    }

    /// The expressions inside an assignment target, which is reported already.
    fn target_parts(&mut self, target: &AstAssignTarget, scope: &mut Scope) {
        match &target.node {
            AssignTargetP::Identifier(_) => {}
            AssignTargetP::Tuple(items) => {
                for item in items {
                    self.target_parts(item, scope);
                }
            }
            AssignTargetP::Index(pair) => {
                self.dict_key(&pair.0, &pair.1, scope);
                self.expr(&pair.0, scope, false);
                self.expr(&pair.1, scope, false);
            }
            AssignTargetP::Dot(object, _) => self.expr(object, scope, false),
        }
    }

    /// A dict known as one is indexed with a string literal only (2.3, 4.1).
    fn dict_key(&mut self, object: &AstExpr, key: &AstExpr, scope: &Scope) {
        if !matches!(&key.node, ExprP::Literal(AstLiteral::String(_)))
            && self.kinds(object, scope).is_dict()
        {
            self.error(
                StarCode::DynamicKeyNotAllowed,
                key.span,
                "a dict is indexed with a string literal only (2.3, 4.1)",
            );
        }
    }

    /// Whether `expr` is the `diag` module, not a name that hides it.
    fn is_diag(&self, expr: &AstExpr, scope: &Scope) -> bool {
        is_name(expr, "diag")
            && matches!(scope.resolve("diag"), Lookup::Outside)
            && !self.globals.contains("diag")
            && !self.defs.contains("diag")
    }

    /// What `expr` is known to be (see the module documentation).
    fn kinds(&self, expr: &AstExpr, scope: &Scope) -> Kinds {
        match &expr.node {
            ExprP::Dict(_) => Kinds::DICT,
            ExprP::Identifier(name) => {
                let name = &name.node.ident;
                let known = if scope.is_local(name) {
                    scope.kinds.get(name)
                } else {
                    self.global_kinds.get(name)
                };
                known.copied().unwrap_or_default()
            }
            ExprP::Call(callee, _) => match &callee.node {
                ExprP::Dot(module, function)
                    if self.is_diag(module, scope)
                        && DIAG_RESPONSES.contains(&function.node.as_str()) =>
                {
                    Kinds::RESPONSE
                }
                _ => Kinds::default(),
            },
            // Only a response's `"fields"` is a dict; the values it holds are not (3.4).
            ExprP::Index(pair)
                if matches!(&pair.1.node, ExprP::Literal(AstLiteral::String(key)) if key.node == "fields")
                    && self.kinds(&pair.0, scope).response =>
            {
                Kinds::DICT
            }
            ExprP::If(triple) => self
                .kinds(&triple.1, scope)
                .union(self.kinds(&triple.2, scope)),
            // `and` and `or` give one of their operands.
            ExprP::Op(left, BinOp::And | BinOp::Or, right) => {
                self.kinds(left, scope).union(self.kinds(right, scope))
            }
            _ => Kinds::default(),
        }
    }

    /// A comprehension, after it has been reported: its parts, with its loop variables bound.
    fn comprehension(
        &mut self,
        first: &ForClauseP<AstNoPayload>,
        clauses: &[ClauseP<AstNoPayload>],
        elements: &[&AstExpr],
        scope: &mut Scope,
    ) {
        // The first iterable is evaluated outside the comprehension.
        self.expr(&first.over, scope, false);
        let mut inner = Scope {
            frame: Rc::new(Frame {
                parent: Some(scope.frame.clone()),
                ..Frame::default()
            }),
            ..scope.clone()
        };
        self.bind(&first.var, &mut inner);
        for clause in clauses {
            match clause {
                ClauseP::For(for_) => {
                    self.expr(&for_.over, &mut inner, false);
                    self.bind(&for_.var, &mut inner);
                }
                ClauseP::If(condition) => self.expr(condition, &mut inner, false),
            }
        }
        for element in elements {
            self.expr(element, &mut inner, false);
        }
    }

    /// Binds the names of a comprehension's loop target in `scope`.
    fn bind(&mut self, target: &AstAssignTarget, scope: &mut Scope) {
        let mut names = HashSet::new();
        target_names(target, &mut names);
        for name in &names {
            unbind(&mut scope.kinds, name);
        }
        // The comprehension's own frame, which nothing else holds, so this copies nothing.
        Rc::make_mut(&mut scope.frame).locals.extend(names);
        self.target(target, scope);
    }

    /// Checks an expression; `callee` is true when it is the function a call names.
    fn expr(&mut self, expr: &AstExpr, scope: &mut Scope, callee: bool) {
        match &expr.node {
            ExprP::Literal(AstLiteral::Ellipsis) => self.error(
                StarCode::UnsupportedSyntax,
                expr.span,
                "`...` is not supported (2.2)",
            ),
            ExprP::Literal(_) => {}
            ExprP::Identifier(name) => self.identifier(&name.node.ident, expr.span, callee, scope),
            ExprP::Tuple(items) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    expr.span,
                    "tuples are not supported (2.2, 2.5)",
                );
                for item in items {
                    self.expr(item, scope, false);
                }
            }
            ExprP::Dot(object, attribute) => {
                if self.is_diag(object, scope) {
                    if !callee {
                        self.error(
                            StarCode::FunctionValueNotAllowed,
                            expr.span,
                            format!(
                                "`diag.{}` can only be called, not used as a value (2.3)",
                                attribute.node
                            ),
                        );
                    }
                } else {
                    if !callee {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            expr.span,
                            "attribute access is not supported; only methods are called (2.4)",
                        );
                    }
                    self.expr(object, scope, false);
                }
            }
            ExprP::Call(function, args) => self.call(function, &args.args, scope),
            ExprP::Index(pair) => {
                // The key first: a literal key needs no classification of the receiver, which
                // would walk a chain of indices again at every level.
                if !matches!(&pair.1.node, ExprP::Literal(AstLiteral::String(_)))
                    && self.kinds(&pair.0, scope).is_dict()
                {
                    self.error(
                        StarCode::DynamicKeyNotAllowed,
                        pair.1.span,
                        "a dict is indexed with a string literal only (2.3, 4.1)",
                    );
                }
                self.expr(&pair.0, scope, false);
                self.expr(&pair.1, scope, false);
            }
            ExprP::Index2(triple) => {
                if self.kinds(&triple.0, scope).is_dict() {
                    self.error(
                        StarCode::DynamicKeyNotAllowed,
                        expr.span,
                        "a dict is indexed with a string literal only (2.3, 4.1)",
                    );
                }
                self.error(
                    StarCode::UnsupportedSyntax,
                    expr.span,
                    "indexing with two keys is not supported (2.2)",
                );
                for item in [&triple.0, &triple.1, &triple.2] {
                    self.expr(item, scope, false);
                }
            }
            ExprP::Slice(object, start, stop, step) => {
                if self.kinds(object, scope).is_dict() {
                    self.error(
                        StarCode::DynamicKeyNotAllowed,
                        expr.span,
                        "a dict is indexed with a string literal only, never sliced (2.3, 4.1)",
                    );
                }
                self.expr(object, scope, false);
                for bound in [start, stop, step].into_iter().flatten() {
                    self.expr(bound, scope, false);
                }
            }
            ExprP::Lambda(lambda) => {
                self.error(
                    StarCode::ClosureNotAllowed,
                    expr.span,
                    "`lambda` is not allowed; write a top-level `def` (2.3)",
                );
                self.params(&lambda.params, scope);
                let mut inner = Scope::enter(
                    scope,
                    scope.function,
                    &lambda.params,
                    None,
                    &mut self.next_function,
                );
                self.expr(&lambda.body, &mut inner, false);
            }
            ExprP::Not(inner) | ExprP::Minus(inner) | ExprP::Plus(inner) | ExprP::BitNot(inner) => {
                self.expr(inner, scope, false)
            }
            ExprP::Op(left, _, right) => {
                self.expr(left, scope, false);
                self.expr(right, scope, false);
            }
            ExprP::If(triple) => {
                for item in [&triple.0, &triple.1, &triple.2] {
                    self.expr(item, scope, false);
                }
            }
            ExprP::List(items) => {
                for item in items {
                    self.expr(item, scope, false);
                }
            }
            ExprP::Dict(entries) => {
                for (key, value) in entries {
                    if !matches!(&key.node, ExprP::Literal(AstLiteral::String(_))) {
                        self.error(
                            StarCode::DynamicKeyNotAllowed,
                            key.span,
                            "a dict literal's keys are string literals only (4.1)",
                        );
                    }
                    self.expr(key, scope, false);
                    self.expr(value, scope, false);
                }
            }
            ExprP::ListComprehension(element, first, clauses) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    expr.span,
                    "comprehensions are not supported; use a `for` loop (2.3)",
                );
                self.comprehension(first, clauses, &[element], scope);
            }
            ExprP::DictComprehension(entry, first, clauses) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    expr.span,
                    "comprehensions are not supported; use a `for` loop (2.3)",
                );
                self.comprehension(first, clauses, &[&entry.0, &entry.1], scope);
            }
            ExprP::FString(fstring) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    expr.span,
                    "f-strings are not supported (2.2)",
                );
                for inner in &fstring.node.expressions {
                    self.expr(inner, scope, false);
                }
            }
        }
    }

    /// A name: a function is only called, never used as a value (2.3), and `None` has no value
    /// at run time (2.5). A local hides the functions and built-ins of the same name.
    fn identifier(&mut self, name: &str, span: Span, callee: bool, scope: &Scope) {
        if name == "None" {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`None` is not supported (2.5)",
            );
            return;
        }
        let lookup = scope.resolve(name);
        if callee || matches!(lookup, Lookup::Local) {
            return;
        }
        let function = self.defs.contains(name)
            || matches!(lookup, Lookup::Closure(_))
            || (!self.globals.contains(name)
                && (BUILTINS.contains(&name)
                    || FORBIDDEN_BUILTINS.contains(&name)
                    || EXCLUDED_BUILTINS.contains(&name)
                    || name == "fail"
                    || name == "diag"));
        if function {
            self.error(
                StarCode::FunctionValueNotAllowed,
                span,
                format!("`{name}` can only be called, not used as a value (2.3)"),
            );
        }
    }

    fn call(&mut self, function: &AstExpr, args: &[AstArgument], scope: &mut Scope) {
        let mut user_function = false;
        match &function.node {
            ExprP::Identifier(name) => {
                let name = name.node.ident.as_str();
                let lookup = scope.resolve(name);
                let local = matches!(lookup, Lookup::Local);
                if let Lookup::Closure(id) = lookup {
                    // The nested `def` is reported where it is defined; its calls still count
                    // for recursion.
                    if self.quiet == 0 {
                        self.calls.push(Call {
                            caller: scope.function,
                            callee: id,
                            written: name.to_owned(),
                            span: function.span,
                        });
                    }
                } else if local || (self.globals.contains(name) && !self.defs.contains(name)) {
                    self.error(
                        StarCode::FunctionValueNotAllowed,
                        function.span,
                        format!("`{name}` is a value, not a function: it cannot be called (2.3)"),
                    );
                } else if self.defs.contains(name) {
                    user_function = true;
                    if self.quiet == 0 {
                        self.calls.push(Call {
                            caller: scope.function,
                            callee: self.def_ids[name],
                            written: name.to_owned(),
                            span: function.span,
                        });
                    }
                } else if name == "fail" {
                    self.error(
                        StarCode::FailNotAllowed,
                        function.span,
                        "`fail` is not allowed; use `diag.fail(code, detail)` (2.3, 4.3)",
                    );
                } else if name == "set" {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        function.span,
                        "`set` is not supported (2.3)",
                    );
                } else if FORBIDDEN_BUILTINS.contains(&name) {
                    self.error(
                        StarCode::BuiltinNotAllowed,
                        function.span,
                        format!("`{name}` is not allowed; output goes through `diag.log` (2.3)"),
                    );
                } else if !BUILTINS.contains(&name) && name != "None" {
                    self.error(
                        StarCode::BuiltinNotAllowed,
                        function.span,
                        format!(
                            "`{name}` is neither a built-in of 2.4 nor a function of this file"
                        ),
                    );
                }
                self.identifier(name, function.span, true, scope);
            }
            ExprP::Dot(object, method) => {
                let method = method.node.as_str();
                if self.is_diag(object, scope) {
                    if !DIAG_FUNCTIONS.contains(&method) {
                        self.error(
                            StarCode::BuiltinNotAllowed,
                            function.span,
                            format!("`diag.{method}` is not a function of the `diag` module (3.1)"),
                        );
                    }
                } else if !STRING_METHODS.contains(&method) {
                    self.error(
                        StarCode::BuiltinNotAllowed,
                        function.span,
                        format!("the method `{method}` is not supported (2.4)"),
                    );
                }
                self.expr(function, scope, true);
            }
            _ => {
                self.error(
                    StarCode::FunctionValueNotAllowed,
                    function.span,
                    "only a named function can be called (2.3)",
                );
                self.expr(function, scope, false);
            }
        }
        for arg in args {
            match &arg.node {
                ArgumentP::Positional(value) => self.expr(value, scope, false),
                ArgumentP::Named(_, value) => {
                    if user_function {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            arg.span,
                            "keyword arguments to a function of this file are not supported (2.3)",
                        );
                    }
                    self.expr(value, scope, false);
                }
                ArgumentP::Args(value) | ArgumentP::KwArgs(value) => {
                    self.error(
                        StarCode::UnsupportedSyntax,
                        arg.span,
                        "`*args` and `**kwargs` are not supported (2.3)",
                    );
                    self.expr(value, scope, false);
                }
            }
        }
    }

    /// Recursion, direct or through other functions, is reported at each call that closes a
    /// cycle in the call graph (4.1).
    fn recursion(&mut self) {
        let edges: Vec<(usize, usize)> = self
            .calls
            .iter()
            .map(|call| (call.caller, call.callee))
            .collect();
        let component = components(self.next_function, &edges);
        let recursive: Vec<(Span, String)> = self
            .calls
            .iter()
            .filter(|call| component[call.caller] == component[call.callee])
            .map(|call| (call.span, call.written.clone()))
            .collect();
        self.components = component;
        for (span, written) in recursive {
            self.error(
                StarCode::UnboundedNotAllowed,
                span,
                format!("the call to `{written}` is recursive; execution must stay finite (4.1)"),
            );
        }
    }

    /// Reads and checks the comment annotations (4.2, 4.5, 4.6). An annotation is attached by
    /// its line to the statement that follows it, which must be in the same function.
    fn annotations(&mut self) -> Vec<Annotation> {
        let module = self.module;
        let codemap = module.file_span(module.statement().span).file;
        // Only lines are needed here, which a binary search over the line starts gives.
        let line_of = |span: Span| to_u32(codemap.find_line(span.begin()) + 1);
        let indent_of = |line: u32| {
            to_u32(
                codemap
                    .source_line(line as usize - 1)
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .count(),
            )
        };
        let mut statements = Vec::new();
        collect_statements(module.statement(), &mut statements);
        // Each statement's line, whether it is a `for`, and the byte offset it begins at, which
        // tells apart the statements that share a line (`a; b`).
        let statement_lines: Vec<(u32, bool, u32)> = statements
            .iter()
            .map(|stmt| {
                (
                    line_of(stmt.span),
                    matches!(stmt.node, StmtP::For(_)),
                    stmt.span.begin().get(),
                )
            })
            .collect();
        // A top-level `def` holds the lines after its own up to the next top-level statement, and
        // of those the comments indented further than `def`: a comment after the last statement
        // of the body still belongs to it.
        let top = Self::statements(module.statement());
        let defs: Vec<((u32, u32), u32)> = top
            .iter()
            .enumerate()
            .filter(|(_, stmt)| matches!(stmt.node, StmtP::Def(_)))
            .map(|(at, stmt)| {
                let begin = line_of(stmt.span);
                let end = top
                    .get(at + 1)
                    .map_or(u32::MAX, |next| line_of(next.span) - 1);
                ((begin, end), indent_of(begin))
            })
            .collect();
        // Both lists are in source order, so a lookup is a binary search.
        let enclosing = |line: u32, indent: u32| {
            let at = defs.partition_point(|((begin, _), _)| *begin < line);
            at.checked_sub(1)
                .map(|at| defs[at])
                .filter(|((_, end), def_indent)| line <= *end && indent > *def_indent)
                .map(|(range, _)| range)
        };
        // The statement after `line`, if it is in the function `def`.
        let next_statement = |def: (u32, u32), line: u32| {
            let at = statement_lines.partition_point(|(begin, ..)| *begin <= line);
            statement_lines
                .get(at)
                .copied()
                .filter(|(at, ..)| def.0 < *at && *at <= def.1)
        };
        // The top-level `def`s by line, with their names.
        let top_defs: Vec<(u32, String)> = top
            .iter()
            .filter_map(|stmt| match &stmt.node {
                StmtP::Def(def) => Some((line_of(stmt.span), def.name.node.ident.clone())),
                _ => None,
            })
            .collect();
        // For each 1-based line, the first line from it on that holds code: neither blank nor a
        // comment alone. A line inside a string that starts with `#` counts as a comment here,
        // but such a string began on an earlier line of code, so it never stands between a
        // comment and the `def` after it without code doing so too.
        let source_lines: Vec<&str> = codemap.source().lines().collect();
        let mut next_code = vec![u32::MAX; source_lines.len() + 2];
        for (at, text) in source_lines.iter().enumerate().rev() {
            let trimmed = text.trim_start();
            next_code[at + 1] = if trimmed.is_empty() || trimmed.starts_with('#') {
                next_code[at + 2]
            } else {
                to_u32(at + 1)
            };
        }
        let mut annotations = Vec::new();
        // The section functions, by the line of their `def`.
        let mut sectioned: BTreeMap<u32, String> = BTreeMap::new();
        // The statements that have an `@idempotency`, by where they begin.
        let mut idempotent: HashSet<u32> = HashSet::new();
        // Whether `main` has its `@preconditions`.
        let mut preconditioned = false;
        'comments: for &span in module.comments() {
            let file_span = module.file_span(span);
            // One comment marker: after `##` the text starts with `#`, an ordinary comment.
            let source = file_span.source_span();
            let text = source.strip_prefix('#').unwrap_or(source).trim();
            let Some(annotation) = text.strip_prefix('@') else {
                continue;
            };
            if annotation.starts_with(char::is_whitespace) || annotation.is_empty() {
                self.error(
                    StarCode::BadAnnotation,
                    span,
                    "an annotation keyword follows `@` directly, as in `@section` (4.2, 4.5, 4.6, 4.9)",
                );
                continue;
            }
            let line = line_of(span);
            // The indentation of the comment's line: for a comment after a statement, that
            // statement's.
            let column = indent_of(line);
            let mut words = annotation.split_whitespace();
            let keyword = words.next().unwrap_or_default();
            let args: Vec<&str> = words.collect();
            let bad = |checker: &mut Self, hint: String| {
                checker.error(StarCode::BadAnnotation, span, hint);
            };
            match keyword {
                "maxIterations" => {
                    let [value] = args.as_slice() else {
                        bad(self, "`@maxIterations` takes one integer (4.2)".to_owned());
                        continue;
                    };
                    let Some(limit) = parse_limit(self, span, value, 1) else {
                        continue;
                    };
                    let target = enclosing(line, column).and_then(|def| next_statement(def, line));
                    let Some((for_line, true, _)) = target else {
                        bad(
                            self,
                            "`@maxIterations` must precede a `for` statement in the same function \
                             (4.2)"
                                .to_owned(),
                        );
                        continue;
                    };
                    annotations.push(Annotation::MaxIterations {
                        line,
                        for_line,
                        limit,
                    });
                }
                "idempotency" => {
                    let value = match args.as_slice() {
                        ["safe"] => Idempotency::Safe,
                        ["checkState"] => Idempotency::CheckState,
                        ["unsafe"] => Idempotency::Unsafe,
                        _ => {
                            bad(
                                self,
                                "`@idempotency` takes `safe`, `checkState` or `unsafe` (4.6)"
                                    .to_owned(),
                            );
                            continue;
                        }
                    };
                    // A comment after code would take that statement's line and so annotate a
                    // later one.
                    if !source_lines
                        .get(line as usize - 1)
                        .is_some_and(|text| text.trim_start().starts_with('#'))
                    {
                        bad(
                            self,
                            "`@idempotency` stands on a line of its own, directly before the \
                             statement it annotates (4.6)"
                                .to_owned(),
                        );
                        continue;
                    }
                    let target = enclosing(line, column).and_then(|def| next_statement(def, line));
                    let Some((_, _, statement_at)) = target else {
                        bad(
                            self,
                            "`@idempotency` must precede a statement in the same function (4.6)"
                                .to_owned(),
                        );
                        continue;
                    };
                    if !idempotent.insert(statement_at) {
                        bad(
                            self,
                            "this statement already has an `@idempotency`; a statement takes one \
                             (4.6)"
                                .to_owned(),
                        );
                        continue;
                    }
                    annotations.push(Annotation::Idempotency {
                        line,
                        statement_at,
                        value,
                    });
                }
                "section" => {
                    let kind = match args.first() {
                        Some(&"interruptible") => SectionKind::Interruptible,
                        Some(&"uninterruptible") => SectionKind::Uninterruptible,
                        Some(&"recoveryRequired") => SectionKind::RecoveryRequired,
                        _ => {
                            bad(
                                self,
                                "`@section` takes `interruptible`, `uninterruptible` or \
                                 `recoveryRequired` (4.5)"
                                    .to_owned(),
                            );
                            continue;
                        }
                    };
                    let expected_millis = match &args[1..] {
                        [] => None,
                        [expected] => match expected.strip_prefix("expected=") {
                            Some(value) => match parse_limit(self, span, value, 0) {
                                Some(millis) => Some(millis),
                                None => continue,
                            },
                            None => {
                                bad(
                                    self,
                                    format!("unknown `@section` attribute `{expected}` (4.5)"),
                                );
                                continue;
                            }
                        },
                        _ => {
                            bad(
                                self,
                                "`@section` takes one attribute, `expected=` (4.5)".to_owned(),
                            );
                            continue;
                        }
                    };
                    // The comment stands alone at the start of its line, and the next line of
                    // code after it is a top-level `def`: only blank lines and other comments
                    // come between, so it is outside every function body and every statement.
                    let standalone = source_lines
                        .get(line as usize - 1)
                        .is_some_and(|text| text.starts_with('#'));
                    let target = next_code
                        .get(line as usize + 1)
                        .filter(|_| standalone)
                        .and_then(|next| {
                            top_defs
                                .binary_search_by_key(next, |(def_line, _)| *def_line)
                                .ok()
                        })
                        .map(|at| &top_defs[at]);
                    let Some((def, name)) = target.cloned() else {
                        bad(
                            self,
                            "`@section` must directly precede a top-level `def`, which it makes a \
                             section; a nested `def` or a lambda cannot be one (4.5)"
                                .to_owned(),
                        );
                        continue;
                    };
                    if sectioned.contains_key(&def) {
                        bad(
                            self,
                            format!("`{name}` already has a `@section`; a `def` takes one (4.5)"),
                        );
                        continue;
                    }
                    sectioned.insert(def, name);
                    annotations.push(Annotation::Section {
                        line,
                        def,
                        kind,
                        expected_millis,
                    });
                }
                "preconditions" => {
                    let mut kinds = Vec::new();
                    for name in &args {
                        let kind = match precondition_kind(name) {
                            Some(kind) => kind,
                            None => {
                                bad(
                                    self,
                                    format!(
                                        "`{name}` is not a precondition; `@preconditions` takes \
                                         `voltage`, `externalSupply`, `ignition`, `engine` and \
                                         `vehicleSpeed` (4.9)"
                                    ),
                                );
                                continue 'comments;
                            }
                        };
                        if kinds.contains(&kind) {
                            bad(self, format!("`{name}` is named twice (4.9)"));
                            continue 'comments;
                        }
                        kinds.push(kind);
                    }
                    if kinds.is_empty() {
                        bad(
                            self,
                            "`@preconditions` names at least one precondition (4.9)".to_owned(),
                        );
                        continue;
                    }
                    // Alone at the start of its line, with only comments and blank lines
                    // between it and `def main()`.
                    let standalone = source_lines
                        .get(line as usize - 1)
                        .is_some_and(|text| text.starts_with('#'));
                    let before_main = next_code
                        .get(line as usize + 1)
                        .filter(|_| standalone)
                        .and_then(|next| {
                            top_defs
                                .binary_search_by_key(next, |(def_line, _)| *def_line)
                                .ok()
                        })
                        .is_some_and(|at| top_defs[at].1 == "main");
                    if !before_main {
                        bad(
                            self,
                            "`@preconditions` must directly precede `def main()` (4.9)".to_owned(),
                        );
                        continue;
                    }
                    if preconditioned {
                        bad(
                            self,
                            "`main` already has a `@preconditions`; it takes one (4.9)".to_owned(),
                        );
                        continue;
                    }
                    preconditioned = true;
                    annotations.push(Annotation::Preconditions { line, span, kinds });
                }
                "endsection" => bad(
                    self,
                    "`@endsection` does not exist: a section is a whole top-level `def`, marked \
                     by a `@section` directly before it (4.5)"
                        .to_owned(),
                ),
                other => bad(
                    self,
                    format!("unknown annotation `@{other}`; see 4.2, 4.5, 4.6 and 4.9"),
                ),
            }
        }
        self.section_nesting(sectioned.values());
        annotations
    }

    /// Sections do not nest: no function that runs inside a section, the section function itself
    /// or anything it calls, directly or not, may call another section function (4.5,
    /// ADR-301). A call that closes a cycle is reported as recursion only.
    fn section_nesting<'n>(&mut self, sections: impl Iterator<Item = &'n String>) {
        let ids: HashSet<usize> = sections.map(|name| self.def_ids[name]).collect();
        let mut successors: Vec<Vec<usize>> = vec![Vec::new(); self.next_function];
        for call in &self.calls {
            successors[call.caller].push(call.callee);
        }
        // Every function reachable from a section function, by one pass over the graph.
        let mut inside = vec![false; self.next_function];
        let mut stack: Vec<usize> = ids.iter().copied().collect();
        for &id in &stack {
            inside[id] = true;
        }
        while let Some(function) = stack.pop() {
            for &next in &successors[function] {
                if !inside[next] {
                    inside[next] = true;
                    stack.push(next);
                }
            }
        }
        let nested: Vec<(Span, String)> = self
            .calls
            .iter()
            .filter(|call| {
                inside[call.caller]
                    && ids.contains(&call.callee)
                    && self.components[call.caller] != self.components[call.callee]
            })
            .map(|call| (call.span, call.written.clone()))
            .collect();
        for (span, written) in nested {
            self.error(
                StarCode::BadAnnotation,
                span,
                format!(
                    "`{written}` is a section and this call runs inside another section; \
                     sections do not nest (4.5)"
                ),
            );
        }
    }
}

/// An annotation's integer, from `min` to 2^32 - 1. A value that is not a decimal integer, signed
/// or not, is a bad annotation; one outside the range is out of range.
fn parse_limit(checker: &mut Checker<'_>, span: Span, value: &str, min: i128) -> Option<u32> {
    let digits = value.strip_prefix(['-', '+']).unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        checker.error(
            StarCode::BadAnnotation,
            span,
            format!("`{value}` is not a decimal integer"),
        );
        return None;
    }
    // Too many digits for an i128 is out of range as well.
    match value.parse::<i128>() {
        Ok(n) if n >= min && n <= i128::from(u32::MAX) => u32::try_from(n).ok(),
        _ => {
            checker.error(
                StarCode::ArgumentOutOfRange,
                span,
                format!("`{value}` is outside {min} to 4294967295"),
            );
            None
        }
    }
}

/// The strongly connected component of each of the `count` functions in the call graph `edges`
/// (Tarjan's
/// algorithm, iterative so that a long call chain cannot exhaust the stack). Two functions share
/// a component exactly when each reaches the other; time and memory are linear in the graph.
fn components(count: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); count];
    for &(from, to) in edges {
        successors[from].push(to);
    }
    let mut index = vec![usize::MAX; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut component = vec![usize::MAX; count];
    let mut next_index = 0;
    let mut next_component = 0;
    for root in 0..count {
        if index[root] != usize::MAX {
            continue;
        }
        // Each frame is a node and the position of the next successor to visit.
        let mut frames = vec![(root, 0)];
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut at)) = frames.last_mut() {
            if let Some(&next) = successors[node].get(*at) {
                *at += 1;
                if index[next] == usize::MAX {
                    index[next] = next_index;
                    low[next] = next_index;
                    next_index += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    frames.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = next_component;
                    if member == node {
                        break;
                    }
                }
                next_component += 1;
            }
        }
    }
    component
}

/// Every statement but the `Statements` groupings, in source order.
fn collect_statements<'a>(stmt: &'a AstStmt, out: &mut Vec<&'a AstStmt>) {
    match &stmt.node {
        StmtP::Statements(list) => list.iter().for_each(|inner| collect_statements(inner, out)),
        StmtP::If(_, then) => {
            out.push(stmt);
            collect_statements(then, out);
        }
        StmtP::IfElse(_, branches) => {
            out.push(stmt);
            collect_statements(&branches.0, out);
            collect_statements(&branches.1, out);
        }
        StmtP::For(for_) => {
            out.push(stmt);
            collect_statements(&for_.body, out);
        }
        StmtP::Def(def) => {
            out.push(stmt);
            collect_statements(&def.body, out);
        }
        _ => out.push(stmt),
    }
}

fn is_name(expr: &AstExpr, name: &str) -> bool {
    matches!(&expr.node, ExprP::Identifier(ident) if ident.node.ident == name)
}

/// The names a function body binds, without descending into nested `def`s: the assigned names
/// into `locals`, the nested `def`s into `closures`.
fn collect_locals(stmt: &AstStmt, locals: &mut HashSet<String>, closures: &mut HashSet<String>) {
    match &stmt.node {
        StmtP::Statements(list) => list
            .iter()
            .for_each(|inner| collect_locals(inner, locals, closures)),
        StmtP::Assign(assign) => target_names(&assign.lhs, locals),
        StmtP::AssignModify(target, ..) => target_names(target, locals),
        StmtP::If(_, then) => collect_locals(then, locals, closures),
        StmtP::IfElse(_, branches) => {
            collect_locals(&branches.0, locals, closures);
            collect_locals(&branches.1, locals, closures);
        }
        StmtP::For(for_) => {
            target_names(&for_.var, locals);
            collect_locals(&for_.body, locals, closures);
        }
        StmtP::Def(def) => {
            closures.insert(def.name.node.ident.clone());
        }
        _ => {}
    }
}

/// The names an assignment target binds.
fn target_names(target: &AstAssignTarget, out: &mut HashSet<String>) {
    match &target.node {
        AssignTargetP::Identifier(name) => {
            out.insert(name.node.ident.clone());
        }
        AssignTargetP::Tuple(items) => items.iter().for_each(|item| target_names(item, out)),
        AssignTargetP::Index(_) | AssignTargetP::Dot(..) => {}
    }
}

fn is_string_expression(stmt: &AstStmt) -> bool {
    matches!(&stmt.node, StmtP::Expression(expr) if matches!(expr.node, ExprP::Literal(AstLiteral::String(_))))
}

#[cfg(test)]
mod tests;
