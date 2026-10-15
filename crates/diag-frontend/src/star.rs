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

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use starlark_syntax::codemap::Span;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignTargetP, AstArgument, AstAssignTarget, AstExpr, AstLiteral, AstParameter,
    AstStmt, ExprP, ParameterP, StmtP,
};
use starlark_syntax::syntax::{AstModule, Dialect, DialectTypes};

/// The error codes of `docs/starlark-subset.md` that the checker reports.
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
    /// A malformed, unknown or unpaired annotation (4.2, 4.5, 4.6).
    #[serde(rename = "STAR_BAD_ANNOTATION")]
    BadAnnotation,
    /// An annotation value outside its range (4.2, 4.5).
    #[serde(rename = "STAR_ARGUMENT_OUT_OF_RANGE")]
    ArgumentOutOfRange,
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
            Self::ArgumentOutOfRange => "STAR_ARGUMENT_OUT_OF_RANGE",
        }
    }
}

impl std::fmt::Display for StarCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One violation, in the form of section 5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StarError {
    pub code: StarCode,
    /// The file's name.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
    /// The source line, without its line end.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Annotation {
    /// `@maxIterations n`, attached to the `for` statement that follows (4.2).
    MaxIterations { line: u32, limit: u32 },
    /// `@section kind [expected=millis]` (4.5).
    Section {
        line: u32,
        kind: SectionKind,
        expected_millis: Option<u32>,
    },
    /// `@endsection` (4.5).
    EndSection { line: u32 },
    /// `@idempotency value`, attached to the statement that follows (4.6).
    Idempotency { line: u32, value: Idempotency },
}

/// A procedure that passed the checks: its AST and annotations, for the lowering.
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
        enable_f_strings: false,
        ..Dialect::Standard
    }
}

/// Parses `source`, the contents of the procedure file `file`, and checks it against the subset.
pub fn check(file: &str, source: &str) -> Result<Checked, Vec<StarError>> {
    let module = match AstModule::parse(file, source.to_owned(), &dialect()) {
        Ok(module) => module,
        Err(error) => return Err(vec![syntax_error(file, source, &error)]),
    };
    let mut checker = Checker {
        file,
        module: &module,
        errors: Vec::new(),
        defs: HashMap::new(),
        globals: HashSet::new(),
        global_dicts: HashSet::new(),
        calls: Vec::new(),
    };
    checker.top_level();
    checker.recursion();
    let annotations = checker.annotations();
    let mut errors = checker.errors;
    if errors.is_empty() {
        return Ok(Checked {
            module,
            annotations,
        });
    }
    errors.sort_by_key(|e| (e.line, e.column));
    Err(errors)
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
            snippet: source.lines().next().unwrap_or_default().to_owned(),
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
        snippet: span.file.source_line(resolved.begin.line).to_owned(),
        hint,
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A top-level `def`: its line range, for the annotations, and whether it takes parameters.
struct Def {
    span: Span,
}

/// A call from one top-level `def` to another, for the recursion check (4.1).
struct Call {
    caller: String,
    callee: String,
    span: Span,
}

struct Checker<'a> {
    file: &'a str,
    module: &'a AstModule,
    errors: Vec<StarError>,
    /// The top-level `def`s by name.
    defs: HashMap<String, Def>,
    /// The top-level constants.
    globals: HashSet<String>,
    /// The top-level constants bound to a dict.
    global_dicts: HashSet<String>,
    calls: Vec<Call>,
}

/// The names of the function being checked: its locals bound to a dict.
struct Scope {
    name: String,
    dicts: HashSet<String>,
}

impl Checker<'_> {
    fn error(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        let file_span = self.module.file_span(span);
        let resolved = file_span.resolve_span();
        self.errors.push(StarError {
            code,
            file: self.file.to_owned(),
            line: to_u32(resolved.begin.line + 1),
            column: to_u32(resolved.begin.column + 1),
            snippet: file_span.file.source_line(resolved.begin.line).to_owned(),
            hint: hint.into(),
        });
    }

    fn statements(stmt: &AstStmt) -> Vec<&AstStmt> {
        match &stmt.node {
            StmtP::Statements(list) => list.iter().collect(),
            _ => vec![stmt],
        }
    }

    /// The top level holds constant assignments and `def`s only (2.1, 2.3); `main` must exist.
    fn top_level(&mut self) {
        let module = self.module;
        let top = Self::statements(module.statement());
        // The names first, so that a call to a function defined further down resolves.
        for stmt in &top {
            match &stmt.node {
                StmtP::Def(def) => {
                    self.defs
                        .insert(def.name.node.ident.clone(), Def { span: stmt.span });
                }
                StmtP::Assign(assign) => {
                    if let AssignTargetP::Identifier(name) = &assign.lhs.node {
                        self.globals.insert(name.node.ident.clone());
                        if is_dict_literal(&assign.rhs) {
                            self.global_dicts.insert(name.node.ident.clone());
                        }
                    }
                }
                _ => {}
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
                    self.params(&def.params);
                    if let Some(ty) = &def.return_type {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            ty.span,
                            "type annotations are not supported (2.3)",
                        );
                    }
                    let mut scope = Scope {
                        name,
                        dicts: HashSet::new(),
                    };
                    for (index, inner) in Self::statements(&def.body).into_iter().enumerate() {
                        // A docstring opens the body.
                        if index == 0 && is_string_expression(inner) {
                            continue;
                        }
                        self.stmt(inner, &mut scope);
                    }
                }
                StmtP::Assign(assign) => {
                    let AssignTargetP::Identifier(_) = &assign.lhs.node else {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            assign.lhs.span,
                            "a top-level constant is assigned to a single name (2.1)",
                        );
                        continue;
                    };
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
                _ => self.error(
                    StarCode::ToplevelStatementNotAllowed,
                    stmt.span,
                    "the top level holds only constant assignments and `def`s (2.1)",
                ),
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
        collect_runtime_calls(expr, &self.defs, &mut found);
        for span in found {
            self.error(
                StarCode::ToplevelStatementNotAllowed,
                span,
                "a top-level constant cannot call `diag` or a function of this file (2.1)",
            );
        }
        let mut scope = Scope {
            name: String::new(),
            dicts: HashSet::new(),
        };
        self.expr(expr, &mut scope, false);
    }

    /// Positional parameters only, without defaults or types (2.3).
    fn params(&mut self, params: &[AstParameter]) {
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
                    if default.is_some() {
                        self.error(
                            StarCode::UnsupportedSyntax,
                            param.span,
                            "default parameter values are not supported (2.3)",
                        );
                    }
                }
                ParameterP::Args(..) | ParameterP::KwArgs(..) => self.error(
                    StarCode::UnsupportedSyntax,
                    param.span,
                    "`*args` and `**kwargs` are not supported (2.3)",
                ),
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
            StmtP::Break | StmtP::Continue | StmtP::Pass => {}
            StmtP::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, scope, false);
                }
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
                let dict = self.is_dict(&assign.rhs, scope);
                if let Some(name) = self.target(&assign.lhs) {
                    if dict {
                        scope.dicts.insert(name);
                    } else {
                        scope.dicts.remove(&name);
                    }
                }
            }
            StmtP::AssignModify(target, _, value) => {
                self.target(target);
                self.expr(value, scope, false);
            }
            StmtP::Statements(list) => {
                for inner in list {
                    self.stmt(inner, scope);
                }
            }
            StmtP::If(condition, then) => {
                self.expr(condition, scope, false);
                self.stmt(then, scope);
            }
            StmtP::IfElse(condition, branches) => {
                self.expr(condition, scope, false);
                self.stmt(&branches.0, scope);
                self.stmt(&branches.1, scope);
            }
            StmtP::For(for_) => {
                if let Some(name) = self.target(&for_.var) {
                    scope.dicts.remove(&name);
                }
                self.expr(&for_.over, scope, false);
                self.stmt(&for_.body, scope);
            }
            StmtP::Def(def) => self.error(
                StarCode::ClosureNotAllowed,
                def.name.span,
                "a nested `def` is not allowed; write it as a top-level `def` (2.3)",
            ),
            StmtP::Load(_) => self.error(
                StarCode::LoadNotAllowed,
                stmt.span,
                "`load` is not allowed (2.3)",
            ),
        }
    }

    /// An assignment target: a single name. Returns the name.
    fn target(&mut self, target: &AstAssignTarget) -> Option<String> {
        match &target.node {
            AssignTargetP::Identifier(name) => Some(name.node.ident.clone()),
            AssignTargetP::Tuple(_) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    target.span,
                    "unpacking assignment is not supported; assign each name (2.2)",
                );
                None
            }
            AssignTargetP::Index(_) | AssignTargetP::Dot(..) => {
                self.error(
                    StarCode::UnsupportedSyntax,
                    target.span,
                    "assigning to an element or attribute is not supported: values are not \
                     mutated (2.5)",
                );
                None
            }
        }
    }

    /// Whether `expr` is known to be a dict (see the module documentation).
    fn is_dict(&self, expr: &AstExpr, scope: &Scope) -> bool {
        match &expr.node {
            ExprP::Dict(_) => true,
            ExprP::Identifier(name) => {
                scope.dicts.contains(&name.node.ident)
                    || self.global_dicts.contains(&name.node.ident)
            }
            ExprP::Call(callee, _) => match &callee.node {
                ExprP::Dot(module, function) => {
                    is_name(module, "diag") && DIAG_RESPONSES.contains(&function.node.as_str())
                }
                _ => false,
            },
            ExprP::Index(pair) => {
                matches!(&pair.1.node, ExprP::Literal(AstLiteral::String(key)) if key.node == "fields")
                    && self.is_dict(&pair.0, scope)
            }
            _ => false,
        }
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
            ExprP::Identifier(name) => self.identifier(&name.node.ident, expr.span, callee),
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
                if is_name(object, "diag") {
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
                if self.is_dict(&pair.0, scope)
                    && !matches!(&pair.1.node, ExprP::Literal(AstLiteral::String(_)))
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
                self.expr(object, scope, false);
                for bound in [start, stop, step].into_iter().flatten() {
                    self.expr(bound, scope, false);
                }
            }
            ExprP::Lambda(_) => self.error(
                StarCode::ClosureNotAllowed,
                expr.span,
                "`lambda` is not allowed; write a top-level `def` (2.3)",
            ),
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
                    self.expr(key, scope, false);
                    self.expr(value, scope, false);
                }
            }
            ExprP::ListComprehension(..) | ExprP::DictComprehension(..) => self.error(
                StarCode::UnsupportedSyntax,
                expr.span,
                "comprehensions are not supported; use a `for` loop (2.3)",
            ),
            ExprP::FString(_) => self.error(
                StarCode::UnsupportedSyntax,
                expr.span,
                "f-strings are not supported (2.2)",
            ),
        }
    }

    /// A name: a function is only called, never used as a value (2.3), and `None` has no value
    /// at run time (2.5).
    fn identifier(&mut self, name: &str, span: Span, callee: bool) {
        if name == "None" {
            self.error(
                StarCode::UnsupportedSyntax,
                span,
                "`None` is not supported (2.5)",
            );
            return;
        }
        if callee {
            return;
        }
        let function = self.defs.contains_key(name)
            || (!self.globals.contains(name)
                && (BUILTINS.contains(&name)
                    || FORBIDDEN_BUILTINS.contains(&name)
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
                if self.defs.contains_key(name) {
                    user_function = true;
                    self.calls.push(Call {
                        caller: scope.name.clone(),
                        callee: name.to_owned(),
                        span: function.span,
                    });
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
                self.identifier(name, function.span, true);
            }
            ExprP::Dot(object, method) => {
                let method = method.node.as_str();
                if is_name(object, "diag") {
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
        let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for call in &self.calls {
            edges
                .entry(call.caller.as_str())
                .or_default()
                .push(call.callee.as_str());
        }
        let reaches = |from: &str, to: &str| -> bool {
            let mut seen = HashSet::new();
            let mut pending = vec![from];
            while let Some(name) = pending.pop() {
                if name == to {
                    return true;
                }
                if seen.insert(name) {
                    pending.extend(edges.get(name).into_iter().flatten().copied());
                }
            }
            false
        };
        let recursive: Vec<(Span, String)> = self
            .calls
            .iter()
            .filter(|call| reaches(&call.callee, &call.caller))
            .map(|call| (call.span, call.callee.clone()))
            .collect();
        for (span, callee) in recursive {
            self.error(
                StarCode::UnboundedNotAllowed,
                span,
                format!("the call to `{callee}` is recursive; execution must stay finite (4.1)"),
            );
        }
    }

    /// Reads and checks the comment annotations (4.2, 4.5, 4.6). An annotation is attached by
    /// its line to the statement that follows it.
    fn annotations(&mut self) -> Vec<Annotation> {
        let module = self.module;
        let mut statements = Vec::new();
        collect_statements(module.statement(), &mut statements);
        let statement_lines: Vec<(u32, bool)> = statements
            .iter()
            .map(|stmt| {
                let line = to_u32(module.file_span(stmt.span).resolve_span().begin.line + 1);
                (line, matches!(stmt.node, StmtP::For(_)))
            })
            .collect();
        let defs: Vec<(u32, u32)> = self
            .defs
            .values()
            .map(|def| {
                let span = module.file_span(def.span).resolve_span();
                (to_u32(span.begin.line + 1), to_u32(span.end.line + 1))
            })
            .collect();
        let enclosing = |line: u32| {
            defs.iter()
                .copied()
                .find(|(begin, end)| *begin < line && line <= *end)
        };
        let next_statement = |line: u32| {
            statement_lines
                .iter()
                .filter(|(at, _)| *at > line)
                .min_by_key(|(at, _)| *at)
                .copied()
        };
        let mut annotations = Vec::new();
        // The open `@section` of the function being read: its def, column and comment.
        let mut open: Option<((u32, u32), u32, Span)> = None;
        for &span in module.comments() {
            let file_span = module.file_span(span);
            let text = file_span.source_span().trim_start_matches('#').trim();
            let Some(annotation) = text.strip_prefix('@') else {
                continue;
            };
            let resolved = file_span.resolve_span();
            let line = to_u32(resolved.begin.line + 1);
            let column = to_u32(resolved.begin.column);
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
                    if !matches!(next_statement(line), Some((_, true))) {
                        bad(
                            self,
                            "`@maxIterations` must precede a `for` statement (4.2)".to_owned(),
                        );
                        continue;
                    }
                    annotations.push(Annotation::MaxIterations { line, limit });
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
                    if enclosing(line).is_none() || next_statement(line).is_none() {
                        bad(
                            self,
                            "`@idempotency` must precede a statement in a function (4.6)"
                                .to_owned(),
                        );
                        continue;
                    }
                    annotations.push(Annotation::Idempotency { line, value });
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
                    let Some(def) = enclosing(line) else {
                        bad(
                            self,
                            "`@section` must be inside a function (4.5)".to_owned(),
                        );
                        continue;
                    };
                    if let Some((_, _, open_span)) = open {
                        self.error(
                            StarCode::BadAnnotation,
                            open_span,
                            "this `@section` has no `@endsection` before the next one (4.5)",
                        );
                    }
                    open = Some((def, column, span));
                    annotations.push(Annotation::Section {
                        line,
                        kind,
                        expected_millis,
                    });
                }
                "endsection" => {
                    if !args.is_empty() {
                        bad(self, "`@endsection` takes no arguments (4.5)".to_owned());
                        continue;
                    }
                    match open.take() {
                        Some((def, open_column, _))
                            if enclosing(line) == Some(def) && open_column == column =>
                        {
                            annotations.push(Annotation::EndSection { line });
                        }
                        Some((_, _, open_span)) => {
                            bad(
                                self,
                                "`@endsection` must be in the same function and at the same \
                                 indentation as its `@section` (4.5)"
                                    .to_owned(),
                            );
                            self.error(
                                StarCode::BadAnnotation,
                                open_span,
                                "this `@section` has no matching `@endsection` (4.5)",
                            );
                        }
                        None => bad(self, "`@endsection` without a `@section` (4.5)".to_owned()),
                    }
                }
                other => bad(
                    self,
                    format!("unknown annotation `@{other}`; see 4.2, 4.5 and 4.6"),
                ),
            }
        }
        if let Some((_, _, open_span)) = open {
            self.error(
                StarCode::BadAnnotation,
                open_span,
                "this `@section` has no `@endsection` (4.5)",
            );
        }
        annotations
    }
}

/// An annotation's integer, from `min` to 2^32 - 1. A value that is not an integer is a bad
/// annotation; one outside the range is out of range.
fn parse_limit(checker: &mut Checker<'_>, span: Span, value: &str, min: u64) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        checker.error(
            StarCode::BadAnnotation,
            span,
            format!("`{value}` is not a decimal integer"),
        );
        return None;
    }
    match value.parse::<u64>() {
        Ok(n) if n >= min && n <= u64::from(u32::MAX) => Some(n as u32),
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

/// The calls in a top-level constant that can only run from `main`: to `diag` and to the
/// file's functions.
fn collect_runtime_calls(expr: &AstExpr, defs: &HashMap<String, Def>, found: &mut Vec<Span>) {
    if let ExprP::Call(function, _) = &expr.node {
        let runtime = match &function.node {
            ExprP::Identifier(name) => defs.contains_key(&name.node.ident),
            ExprP::Dot(object, _) => is_name(object, "diag"),
            _ => false,
        };
        if runtime {
            found.push(function.span);
        }
    }
    expr.visit_expr(|inner| collect_runtime_calls(inner, defs, found));
}

fn is_name(expr: &AstExpr, name: &str) -> bool {
    matches!(&expr.node, ExprP::Identifier(ident) if ident.node.ident == name)
}

fn is_dict_literal(expr: &AstExpr) -> bool {
    matches!(expr.node, ExprP::Dict(_))
}

fn is_string_expression(stmt: &AstStmt) -> bool {
    matches!(&stmt.node, StmtP::Expression(expr) if matches!(expr.node, ExprP::Literal(AstLiteral::String(_))))
}

#[cfg(test)]
mod tests;
