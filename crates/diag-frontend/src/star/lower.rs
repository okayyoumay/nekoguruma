//! Lowering a checked Starlark procedure to a `diag_ir::Program` (subset 2.5, 4.1, 4.4;
//! ADR-254, ADR-304).
//!
//! The core of the lowering: top-level constants (folded at ingestion), top-level `def`s with
//! positional parameters, calls between them, locals, the operators the IR's instructions
//! cover, `and`, `or`, `not`, conditional expressions, `if`/`elif`/`else` and `diag.fail`.
//! Every value that reaches run time has one type, inferred at ingestion (ADR-304); an
//! operation the IR cannot express for its operands is `STAR_NOT_LOWERABLE`.
//!
//! Not lowered yet, each its own backlog item: `for` loops, the other `diag` functions,
//! built-ins and string methods on values known only at run time, and section and idempotency
//! annotations. Each is rejected with `STAR_NOT_LOWERABLE` (or `STAR_API_NOT_AVAILABLE` for a
//! `diag` function without an instruction), so nothing is lowered silently wrong.

use std::collections::{HashMap, HashSet};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program, SourceSpan};
use starlark_syntax::codemap::Span;
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignOp, AssignTargetP, AstArgument, AstExpr, AstLiteral, AstNoPayload, AstStmt,
    BinOp, DefP, ExprP, ParameterP, StmtP,
};

use super::{
    BUILTINS, Checked, Pending, STRING_METHODS, StarCode, StarError, collect_locals,
    is_string_expression, resolve, to_u32,
};

/// The longest constant string, bytes or list a fold may build, so a constant expression cannot
/// make ingestion allocate without bound.
const MAX_CONSTANT_LEN: usize = 1 << 20;

/// How many passes type inference may take before it gives up (ADR-304). Each pass can only
/// widen a type, so real procedures settle in a few; a file that needs more is rejected rather
/// than analysed without bound.
const MAX_TYPE_PASSES: usize = 256;

/// The longest `diag.fail` code (subset 3.1).
const MAX_FAIL_CODE_LEN: usize = 64;

/// Lowers `checked`, the procedure in `file`, to a program. Every reason the procedure cannot
/// be lowered is reported, in source order.
pub fn lower(file: &str, checked: &Checked) -> Result<Program, Vec<StarError>> {
    let mut lowerer = Lowerer::new(checked);
    lowerer.constants();
    lowerer.functions();
    if lowerer.errors.is_empty() {
        lowerer.infer();
    }
    if lowerer.errors.is_empty() {
        lowerer.emit_all();
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
    Ok(program)
}

// ---------------------------------------------------------------- Types and constants

/// A run-time type (subset 2.5): `str` exists at run time as UTF-8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    Float,
    Bool,
    Bytes,
    Str,
}

impl Ty {
    fn bit(self) -> u8 {
        match self {
            Ty::Int => 1,
            Ty::Float => 2,
            Ty::Bool => 4,
            Ty::Bytes => 8,
            Ty::Str => 16,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::Bytes => "bytes",
            Ty::Str => "str",
        }
    }
}

/// The types a value may have at run time, inferred at ingestion. Empty until something is
/// known about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tys(u8);

impl Tys {
    fn of(ty: Ty) -> Self {
        Tys(ty.bit())
    }

    fn union(self, other: Tys) -> Tys {
        Tys(self.0 | other.0)
    }

    /// The one type, when there is exactly one.
    fn one(self) -> Option<Ty> {
        [Ty::Int, Ty::Float, Ty::Bool, Ty::Bytes, Ty::Str]
            .into_iter()
            .find(|ty| self.0 == ty.bit())
    }

    fn describe(self) -> String {
        let names: Vec<_> = [Ty::Int, Ty::Float, Ty::Bool, Ty::Bytes, Ty::Str]
            .into_iter()
            .filter(|ty| self.0 & ty.bit() != 0)
            .map(Ty::name)
            .collect();
        if names.is_empty() {
            "no known type".to_owned()
        } else {
            names.join(" or ")
        }
    }
}

/// A value known at ingestion.
#[derive(Debug, Clone, PartialEq)]
enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Const>),
    /// String keys in source order.
    Dict(Vec<(String, Const)>),
}

impl Const {
    fn ty(&self) -> Option<Ty> {
        match self {
            Const::Int(_) => Some(Ty::Int),
            Const::Float(_) => Some(Ty::Float),
            Const::Bool(_) => Some(Ty::Bool),
            Const::Str(_) => Some(Ty::Str),
            Const::Bytes(_) => Some(Ty::Bytes),
            Const::List(_) | Const::Dict(_) => None,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Const::List(_) => "list",
            Const::Dict(_) => "dict",
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
            Const::List(l) => !l.is_empty(),
            Const::Dict(d) => !d.is_empty(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Const::Str(s) => s.len(),
            Const::Bytes(b) => b.len(),
            Const::List(l) => l.len(),
            Const::Dict(d) => d.len(),
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
}

impl<'a> Lowerer<'a> {
    fn new(checked: &'a Checked) -> Self {
        Lowerer {
            checked,
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
        }
    }

    fn error(&mut self, code: StarCode, span: Span, hint: impl Into<String>) {
        if self.quiet == 0 {
            self.errors.push(Pending {
                code,
                span,
                hint: hint.into(),
            });
        }
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
            let mut names: Vec<String> = params.clone();
            let mut others: Vec<String> = assigned
                .into_iter()
                .filter(|name| !params.contains(name))
                .collect();
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
            if !self.changed {
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
                let name = name.node.ident.clone();
                let current = self.load(id, &name, target.span);
                let rhs_value = self.expr(Some(id), rhs);
                let value = self.binary(Some(id), assign_op(*op), current, rhs_value, stmt.span);
                self.store(id, &name, value, stmt.span, code);
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
            StmtP::For(_) | StmtP::Break | StmtP::Continue => {
                self.not_lowerable(stmt.span, "`for` loops are not lowered yet (4.2)")
            }
            StmtP::Def(_) | StmtP::Load(_) => self.not_lowerable(
                stmt.span,
                "a nested `def` or `load` cannot be lowered (2.3)",
            ),
        }
    }

    /// Stores `value` in the local `name`.
    fn store(&mut self, id: usize, name: &str, value: Val, span: Span, code: &mut Code) {
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
        match &expr.node {
            ExprP::Literal(literal) => match literal {
                AstLiteral::Int(int) => match &int.node {
                    TokenInt::I32(n) => Val::Const(Const::Int(i64::from(*n))),
                    TokenInt::BigInt(big) => match big.to_string().parse::<i64>() {
                        Ok(n) => Val::Const(Const::Int(n)),
                        Err(_) => {
                            self.not_lowerable(span, "an integer outside the 64-bit range (4.4)");
                            Val::Const(Const::Int(0))
                        }
                    },
                },
                AstLiteral::Float(f) => Val::Const(Const::Float(f.node)),
                AstLiteral::String(s) => Val::Const(Const::Str(s.node.clone())),
                AstLiteral::Bytes(b) => Val::Const(Const::Bytes(b.node.clone())),
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
                            return Val::Const(Const::List(Vec::new()));
                        }
                    }
                }
                Val::Const(Const::List(values))
            }
            ExprP::Dict(pairs) => {
                let mut entries = Vec::new();
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
                    match self.expr(id, value) {
                        Val::Const(value) => entries.push((key, value)),
                        _ => self.not_lowerable(
                            value.span,
                            "a dict exists only at ingestion, so its values must be constants (2.5)",
                        ),
                    }
                }
                Val::Const(Const::Dict(entries))
            }
            ExprP::Index(parts) => {
                let (object, index) = &**parts;
                let a = self.expr(id, object);
                let b = self.expr(id, index);
                self.index(a, b, span)
            }
            ExprP::Slice(object, start, stop, step) => {
                let object = self.expr(id, object);
                let bound = |this: &mut Self, part: &Option<Box<AstExpr>>| match part {
                    None => Some(None),
                    Some(expr) => match this.expr(id, expr) {
                        Val::Const(Const::Int(n)) => Some(Some(n)),
                        _ => None,
                    },
                };
                let (start, stop) = (bound(self, start), bound(self, stop));
                let step_given = step.is_some();
                match (object, start, stop) {
                    (Val::Const(object), Some(start), Some(stop)) if !step_given => {
                        self.slice(object, start, stop, span)
                    }
                    _ => {
                        self.not_lowerable(
                            span,
                            "a slice is lowered only when it is a constant without a step; a slice of \
                             a value known at run time needs a slice instruction (2.5)",
                        );
                        Val::Run(Tys::default(), Vec::new())
                    }
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
            Const::Str(s) => vec![Op::PushBytes(self.pool(s.as_bytes()))],
            Const::Bytes(b) => vec![Op::PushBytes(self.pool(b))],
            Const::List(_) | Const::Dict(_) => Vec::new(),
        };
        ops.into_iter().map(|op| (Ins::Op(op), span)).collect()
    }

    fn pool(&mut self, bytes: &[u8]) -> u32 {
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
                    "the truth of bytes or a string known at run time needs a length instruction (2.5)",
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

    fn out_of_range(&mut self, span: Span) -> Val {
        self.not_lowerable(span, "the result is outside the 64-bit integer range (4.4)");
        Val::Const(Const::Int(0))
    }

    fn binary(&mut self, id: Option<usize>, op: BinOp, a: Val, b: Val, span: Span) -> Val {
        if let (Val::Const(x), Val::Const(y)) = (&a, &b) {
            return match fold(op, x, y) {
                Ok(value) => {
                    if value.len() > MAX_CONSTANT_LEN {
                        self.not_lowerable(span, "the constant is larger than 1 MiB");
                    }
                    Val::Const(value)
                }
                Err(reason) => {
                    self.not_lowerable(span, reason);
                    Val::Const(Const::Int(0))
                }
            };
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
            (BinOp::Add | BinOp::Subtract | BinOp::Multiply, Ty::Int, Ty::Int)
            | (
                BinOp::Add | BinOp::Subtract | BinOp::Multiply | BinOp::Divide,
                Ty::Float,
                Ty::Float,
            ) => {
                code.push(at(match op {
                    BinOp::Add => Op::Add,
                    BinOp::Subtract => Op::Sub,
                    BinOp::Multiply => Op::Mul,
                    _ => Op::Div,
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
                    "comparing an int with a float known at run time needs a conversion instruction (4.4)",
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
            (BinOp::Less | BinOp::Greater, Ty::Int, Ty::Int)
            | (BinOp::Less | BinOp::Greater, Ty::Float, Ty::Float) => {
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
            (BinOp::LessOrEqual | BinOp::GreaterOrEqual, Ty::Float, Ty::Float) => {
                let Some(id) = id else {
                    return Val::Run(Tys::default(), Vec::new());
                };
                // NaN is neither less, greater nor equal, so a <= b is (a < b) or (a == b).
                code.extend(self.float_or_equal(id, op == BinOp::LessOrEqual, span));
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
            self.not_lowerable(span, "a function may have at most 65,535 locals");
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
        let done = self.label();
        let mut ops = vec![
            Ins::Op(Op::StoreLocal(b)),
            Ins::Op(Op::StoreLocal(a)),
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
        ];
        if percent {
            ops.extend([
                Ins::Op(Op::LoadLocal(r)),
                Ins::Op(Op::LoadLocal(b)),
                Ins::Op(Op::Add),
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

    /// `a <= b` (`less` true) or `a >= b` on floats: `(a < b) or (a == b)`.
    fn float_or_equal(&mut self, id: usize, less: bool, span: Span) -> Code {
        // The temporaries hold floats.
        let (a, b) = (self.temp(id, span), self.temp(id, span));
        for slot in [a, b] {
            if let Some(tys) = self.funcs[id].slot_tys.get_mut(usize::from(slot)) {
                *tys = Tys::of(Ty::Float);
            }
        }
        let end = self.label();
        let ops = vec![
            Ins::Op(Op::StoreLocal(b)),
            Ins::Op(Op::StoreLocal(a)),
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(if less { Op::CmpLt } else { Op::CmpGt }),
            Ins::Op(Op::Dup),
            Ins::Op(Op::Not),
            // When the strict comparison holds, it stays as the result.
            Ins::JumpIfFalse(end),
            Ins::Op(Op::Pop),
            Ins::Op(Op::LoadLocal(a)),
            Ins::Op(Op::LoadLocal(b)),
            Ins::Op(Op::CmpEq),
            Ins::Mark(end),
        ];
        ops.into_iter().map(|ins| (ins, span)).collect()
    }

    fn index(&mut self, object: Val, index: Val, span: Span) -> Val {
        match (object, index) {
            (Val::Const(object), Val::Const(index)) => match fold_index(&object, &index) {
                Ok(value) => Val::Const(value),
                Err(reason) => {
                    self.not_lowerable(span, reason);
                    Val::Const(Const::Int(0))
                }
            },
            (_, Val::Const(Const::Int(n))) if n < 0 => {
                self.not_lowerable(
                    span,
                    "a negative index into bytes known at run time needs a length instruction (2.5)",
                );
                Val::Run(Tys::of(Ty::Int), Vec::new())
            }
            (object, index) => {
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
        }
    }

    fn slice(&mut self, object: Const, start: Option<i64>, stop: Option<i64>, span: Span) -> Val {
        let len = object.len() as i64;
        let clamp = |n: Option<i64>, default: i64| -> usize {
            let n = n.unwrap_or(default);
            let n = if n < 0 { (n + len).max(0) } else { n.min(len) };
            n as usize
        };
        let (from, to) = (clamp(start, 0), clamp(stop, len));
        let to = to.max(from);
        match object {
            Const::Bytes(b) => Val::Const(Const::Bytes(b[from..to].to_vec())),
            Const::List(l) => Val::Const(Const::List(l[from..to].to_vec())),
            // Indexes count bytes here, which are characters only in ASCII.
            Const::Str(s) if s.is_ascii() => Val::Const(Const::Str(s[from..to].to_owned())),
            other => {
                self.not_lowerable(
                    span,
                    format!("a {} cannot be sliced here", other.type_name()),
                );
                Val::Const(Const::Int(0))
            }
        }
    }

    // ------------------------------------------------------------ Calls

    fn call(
        &mut self,
        id: Option<usize>,
        function: &AstExpr,
        args: &[AstArgument],
        span: Span,
    ) -> Val {
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
                if let Some(&callee) = self.by_name.get(name) {
                    return self.call_function(id, callee, &positional, span);
                }
                let values: Vec<Val> = positional.iter().map(|arg| self.expr(id, arg)).collect();
                if BUILTINS.contains(&name) {
                    let consts: Option<Vec<Const>> = values
                        .into_iter()
                        .map(|v| match v {
                            Val::Const(c) => Some(c),
                            _ => None,
                        })
                        .collect();
                    return match consts.map(|args| fold_builtin(name, &args)) {
                        Some(Ok(value)) => Val::Const(value),
                        Some(Err(reason)) => {
                            self.not_lowerable(span, reason);
                            Val::Run(Tys::default(), Vec::new())
                        }
                        None => {
                            self.not_lowerable(
                                span,
                                format!("`{name}` on a value known only at run time is not lowered yet (2.4)"),
                            );
                            Val::Run(Tys::default(), Vec::new())
                        }
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
                        match fold_method(&s, &method.node, &args) {
                            Ok(value) => Val::Const(value),
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
                    Some((Ty::Str | Ty::Bytes, code)) => Some(code),
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
        BinOp::Add if a == b && matches!(a, Ty::Str | Ty::Bytes) => {
            "a concatenation instruction (2.5)"
        }
        BinOp::Percent if a == Ty::Str => "string formatting instructions (2.5)",
        BinOp::Divide | BinOp::FloorDivide | BinOp::Percent
            if matches!(a, Ty::Int | Ty::Float) && matches!(b, Ty::Int | Ty::Float) =>
        {
            "float conversion, floor or remainder instructions (4.4)"
        }
        _ if a != b && matches!(a, Ty::Int | Ty::Float) && matches!(b, Ty::Int | Ty::Float) => {
            "a conversion between int and float (4.4)"
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
fn always_returns(stmt: &AstStmt) -> bool {
    match &stmt.node {
        StmtP::Return(_) => true,
        StmtP::Statements(list) => list.iter().any(always_returns),
        StmtP::IfElse(_, branches) => always_returns(&branches.0) && always_returns(&branches.1),
        _ => false,
    }
}

// ---------------------------------------------------------------- Folding

fn number(value: &Const) -> Option<f64> {
    match value {
        Const::Int(n) => Some(*n as f64),
        Const::Float(f) => Some(*f),
        _ => None,
    }
}

/// Starlark's floor division and remainder on integers.
fn floor_div_mod(a: i64, b: i64) -> Result<(i64, i64), String> {
    if b == 0 {
        return Err("division by zero".to_owned());
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
                (Str(x), Str(y)) => Some(x.cmp(y)),
                (Bytes(x), Bytes(y)) => Some(x.cmp(y)),
                _ => match (a, b) {
                    (Int(x), Int(y)) => Some(x.cmp(y)),
                    _ => match (number(a), number(b)) {
                        (Some(x), Some(y)) => x.partial_cmp(&y),
                        _ => return Err(mismatch()),
                    },
                },
            };
            use std::cmp::Ordering::*;
            Ok(Bool(match (op, ordering) {
                (_, None) => false,
                (BinOp::Less, Some(o)) => o == Less,
                (BinOp::Greater, Some(o)) => o == Greater,
                (BinOp::LessOrEqual, Some(o)) => o != Greater,
                (_, Some(o)) => o != Less,
            }))
        }
        BinOp::In | BinOp::NotIn => {
            let found = match (a, b) {
                (_, List(items)) => items.iter().any(|item| equal(a, item)),
                (Str(key), Dict(entries)) => entries.iter().any(|(k, _)| k == key),
                (Str(needle), Str(haystack)) => haystack.contains(needle.as_str()),
                (Bytes(needle), Bytes(haystack)) => {
                    needle.is_empty()
                        || haystack
                            .windows(needle.len())
                            .any(|w| w == needle.as_slice())
                }
                (Int(byte), Bytes(haystack)) => {
                    u8::try_from(*byte).is_ok_and(|byte| haystack.contains(&byte))
                }
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
            (Str(x), Str(y)) if op == BinOp::Add => Ok(Str(format!("{x}{y}"))),
            (Bytes(x), Bytes(y)) if op == BinOp::Add => Ok(Bytes([x.as_slice(), y].concat())),
            (List(x), List(y)) if op == BinOp::Add => Ok(List([x.as_slice(), y].concat())),
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
                let (q, r) = floor_div_mod(*x, *y)?;
                Ok(Int(if op == BinOp::FloorDivide { q } else { r }))
            }
            _ => match (number(a), number(b)) {
                (Some(_), Some(0.0)) => Err("division by zero".to_owned()),
                (Some(x), Some(y)) => {
                    let q = (x / y).floor();
                    Ok(Float(if op == BinOp::FloorDivide {
                        q
                    } else {
                        x - q * y
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
            (Int(_), Int(n)) if *n < 0 => Err("a negative shift count".to_owned()),
            (Int(x), Int(n)) if op == BinOp::RightShift => Ok(Int(if *n >= 64 {
                if *x < 0 { -1 } else { 0 }
            } else {
                x >> n
            })),
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

/// Starlark equality: numbers compare by value across `int` and `float`; other values of
/// different types are never equal.
fn equal(a: &Const, b: &Const) -> bool {
    match (a, b) {
        (Const::Int(_) | Const::Float(_), Const::Int(_) | Const::Float(_)) => match (a, b) {
            (Const::Int(x), Const::Int(y)) => x == y,
            _ => number(a) == number(b),
        },
        (Const::List(x), Const::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q))
        }
        (Const::Dict(x), Const::Dict(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && equal(v, v2)))
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
        (Const::List(items), Const::Int(n)) => Ok(items[position(*n, items.len())?].clone()),
        (Const::Bytes(bytes), Const::Int(n)) => {
            Ok(Const::Int(i64::from(bytes[position(*n, bytes.len())?])))
        }
        (Const::Dict(entries), Const::Str(key)) => entries
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
        ("len", [Const::Bytes(_) | Const::List(_) | Const::Dict(_)]) => {
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
        ("int", [Const::Float(f)]) if f.is_finite() && f.trunc().abs() < 9.2e18 => {
            Ok(Const::Int(f.trunc() as i64))
        }
        ("float", [Const::Int(n)]) => Ok(Const::Float(*n as f64)),
        ("float", [Const::Float(f)]) => Ok(Const::Float(*f)),
        ("min" | "max", [_, _, ..]) => {
            let mut best = args[0].clone();
            for arg in &args[1..] {
                let less = match fold(BinOp::Less, arg, &best)? {
                    Const::Bool(b) => b,
                    _ => false,
                };
                if (name == "min") == less {
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
        ("upper", []) => Ok(Const::Str(s.to_uppercase())),
        ("lower", []) => Ok(Const::Str(s.to_lowercase())),
        ("strip", []) => Ok(Const::Str(s.trim().to_owned())),
        ("startswith", [Const::Str(p)]) => Ok(Const::Bool(s.starts_with(p.as_str()))),
        ("endswith", [Const::Str(p)]) => Ok(Const::Bool(s.ends_with(p.as_str()))),
        _ => Err(format!(
            "`.{method}` with these arguments is not lowered yet (2.4)"
        )),
    }
}

#[cfg(test)]
#[path = "lower_tests.rs"]
mod tests;
