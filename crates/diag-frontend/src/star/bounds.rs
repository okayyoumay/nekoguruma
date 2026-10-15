//! The size and nesting bounds a procedure is held to before it is parsed
//! (`docs/starlark-subset.md` 2.8, ADR-303).
//!
//! `starlark_syntax`'s parser, the validation it runs after parsing, the checker's walk and the
//! AST's drop all recurse once per level of the AST, so a deeply nested file would overflow the
//! stack and abort the process. [`prescan`] reads the file's tokens with the parser's own lexer
//! and bounds the depth of the AST the parser would build, without building it: every inner node
//! of an expression has a token of its own (an operator, a keyword, a `.`, a bracket), so the
//! counts below are never less than the depth they stand for. [`on_parser_stack`] then runs the
//! parse on a thread whose stack holds the deepest file the bounds let through.

use starlark_syntax::codemap::CodeMap;
use starlark_syntax::lexer::{Lexer, Token};
use starlark_syntax::syntax::Dialect;

/// The largest source accepted, in bytes.
pub const MAX_SOURCE_BYTES: usize = 1 << 20;

/// The deepest nesting of brackets: `(`, `[`, `{` and an f-string's `{`.
pub const MAX_BRACKET_DEPTH: usize = 64;

/// The deepest nesting of indented blocks.
pub const MAX_BLOCK_DEPTH: usize = 32;

/// The most `elif` clauses the `if` statements enclosing a point have together. Each `elif`
/// nests the rest of its statement one level deeper in the AST.
pub const MAX_ELIF_CHAIN: usize = 256;

/// The deepest an expression may nest: the most operators, keywords, postfix accesses (`.`, an
/// index, a call) and brackets on one path from the statement down to an operand. A chain such as
/// `a + b + c` or `x["a"]["b"]` nests one level deeper per link, and whatever wraps a bracketed
/// operand adds to the depth inside it.
pub const MAX_EXPRESSION_DEPTH: usize = 512;

/// The stack of the thread that parses and checks a file. A file at every bound at once, in the
/// costliest shapes, needs about 5 MiB in a debug build and under 1 MiB in a release build
/// (ADR-303).
pub const PARSER_STACK_BYTES: usize = 16 << 20;

/// A bound the source exceeds, at the byte where it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Exceeded {
    pub at: usize,
    pub hint: String,
}

/// Checks `source` against the nesting bounds. A token the lexer refuses ends the scan: the parser
/// stops at the same token, so it builds nothing past what was scanned.
pub(super) fn prescan(source: &str, dialect: &Dialect) -> Result<(), Exceeded> {
    let codemap = CodeMap::new(String::new(), source.to_owned());
    let mut scan = Scan::default();
    for lexeme in Lexer::new(source, dialect, codemap) {
        let Ok((at, token, _)) = lexeme else {
            break;
        };
        scan.token(at, &token)?;
    }
    Ok(())
}

/// One open bracket level, or the statement's own level, in the scan.
///
/// An expression at this level is a sequence of elements separated by commas, which are siblings
/// in the AST. Within the current element every link (an operator, a keyword, a postfix access)
/// may wrap every operand, the bracketed ones included, so the element nests at most `links`
/// plus the deepest bracketed operand in it.
#[derive(Default)]
struct Level {
    /// The links of the current element.
    links: usize,
    /// The depth of the deepest closed bracket in the current element.
    bracketed: usize,
    /// The depth of the deepest element already ended by a comma.
    ended: usize,
    /// The `lambda`s whose parameters are still being read: until its `:`, a comma separates
    /// parameters of one `lambda`, whose body nests under everything before it.
    lambdas: usize,
}

impl Level {
    fn element(&self) -> usize {
        self.links + self.bracketed
    }
}

/// The scan's state at one token.
struct Scan {
    /// The open bracket levels; the first is the statement's own level.
    levels: Vec<Level>,
    /// The `elif` count of the `if` statement in progress at each open block; the first is the
    /// top level.
    elifs: Vec<usize>,
    /// The sum of `elifs`.
    elif: usize,
    /// Whether the token is the first of a statement on its own line.
    statement_start: bool,
    /// Whether the previous token ended an operand, so that a `(` or `[` is a call or an index.
    after_operand: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            levels: vec![Level::default()],
            elifs: vec![0],
            elif: 0,
            statement_start: true,
            after_operand: false,
        }
    }
}

impl Scan {
    fn token(&mut self, at: usize, token: &Token) -> Result<(), Exceeded> {
        let statement_start = std::mem::replace(&mut self.statement_start, false);
        let after_operand = std::mem::replace(&mut self.after_operand, false);
        match token {
            Token::Comment(_) => {
                self.statement_start = statement_start;
                self.after_operand = after_operand;
                return Ok(());
            }
            // A `;` ends a small statement on the same line, inside the same block: it ends the
            // expression but not an `if` statement whose inline body it is in.
            Token::Newline | Token::Semicolon => {
                if self.levels.len() == 1 {
                    self.levels[0] = Level::default();
                }
                self.statement_start = matches!(token, Token::Newline);
                return Ok(());
            }
            Token::Indent => {
                self.elifs.push(0);
                self.statement_start = true;
                if self.elifs.len() - 1 > MAX_BLOCK_DEPTH {
                    return Err(exceeded(
                        at,
                        format!("blocks nest deeper than {MAX_BLOCK_DEPTH} levels"),
                    ));
                }
                return Ok(());
            }
            Token::Dedent => {
                if self.elifs.len() > 1 {
                    self.elif -= self.elifs.pop().unwrap_or_default();
                }
                self.statement_start = true;
                return Ok(());
            }
            _ => {}
        }
        if statement_start {
            let current = self
                .elifs
                .last_mut()
                .expect("the top level is never popped");
            if matches!(token, Token::Elif) {
                *current += 1;
                self.elif += 1;
                if self.elif > MAX_ELIF_CHAIN {
                    return Err(exceeded(
                        at,
                        format!("more than {MAX_ELIF_CHAIN} `elif` clauses enclose this point"),
                    ));
                }
            } else if !matches!(token, Token::Else) {
                // Any other statement ends the `if` statement in progress at this level.
                self.elif -= std::mem::take(current);
            }
        }
        match token {
            Token::OpeningRound
            | Token::OpeningSquare
            | Token::OpeningCurly
            | Token::FStringExprStart => {
                if after_operand && matches!(token, Token::OpeningRound | Token::OpeningSquare) {
                    self.level().links += 1;
                }
                self.levels.push(Level::default());
                if self.levels.len() - 1 > MAX_BRACKET_DEPTH {
                    return Err(exceeded(
                        at,
                        format!("brackets nest deeper than {MAX_BRACKET_DEPTH} levels"),
                    ));
                }
                return self.check_depth(at);
            }
            Token::ClosingRound
            | Token::ClosingSquare
            | Token::ClosingCurly
            | Token::FStringExprEnd => {
                // An unbalanced bracket is the parser's syntax error.
                if self.levels.len() > 1 {
                    let closed = self.levels.pop().expect("more than one level is open");
                    let depth = 1 + closed.ended.max(closed.element());
                    let level = self.level();
                    level.bracketed = level.bracketed.max(depth);
                }
                self.after_operand = true;
                return Ok(());
            }
            Token::Comma => {
                let level = self.level();
                if level.lambdas == 0 {
                    level.ended = level.ended.max(level.element());
                    level.links = 0;
                    level.bracketed = 0;
                }
                return Ok(());
            }
            Token::Colon => {
                let level = self.level();
                level.lambdas = level.lambdas.saturating_sub(1);
                return Ok(());
            }
            Token::Identifier(_)
            | Token::Int(_)
            | Token::Float(_)
            | Token::String(_)
            | Token::Bytes(_)
            | Token::FStringEnd => {
                self.after_operand = true;
                return Ok(());
            }
            Token::FStringStart(_)
            | Token::FStringText(_)
            | Token::FStringBang
            | Token::Pass
            | Token::Break
            | Token::Continue => return Ok(()),
            Token::Lambda => self.level().lambdas += 1,
            _ => {}
        }
        // Every other token (operators, keywords, `.`) is a link; a statement keyword counts one
        // more than it nests, which only makes the bound stricter.
        self.level().links += 1;
        self.check_depth(at)
    }

    fn level(&mut self) -> &mut Level {
        self.levels
            .last_mut()
            .expect("the statement level is never popped")
    }

    /// Fails when the statement nests deeper than [`MAX_EXPRESSION_DEPTH`] along the path to this
    /// token: each open bracket counts as closed here, wrapped by its level's links.
    fn check_depth(&self, at: usize) -> Result<(), Exceeded> {
        let mut levels = self.levels.iter().rev();
        let innermost = levels.next().expect("the statement level is never popped");
        let depth = levels.fold(innermost.element(), |inner, level| {
            level.links + level.bracketed.max(1 + inner)
        });
        if depth > MAX_EXPRESSION_DEPTH {
            return Err(exceeded(
                at,
                format!("the expression here nests deeper than {MAX_EXPRESSION_DEPTH} levels"),
            ));
        }
        Ok(())
    }
}

fn exceeded(at: usize, hint: String) -> Exceeded {
    Exceeded {
        at,
        hint: format!("{hint}; split it up (2.8)"),
    }
}

/// Runs `f` on a thread with a [`PARSER_STACK_BYTES`] stack and returns its result. A file within
/// the bounds is parsed, checked and walked by anything recursive (the lowering included) inside
/// it. A panic in `f` resumes on the caller.
///
/// # Panics
///
/// When the thread cannot be started.
pub fn on_parser_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("star-parser".to_owned())
            .stack_size(PARSER_STACK_BYTES)
            .spawn_scoped(scope, f)
            .expect("the Starlark parser thread starts")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
