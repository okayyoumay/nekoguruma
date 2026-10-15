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

/// The longest chain of operators, keywords and postfix accesses (`.`, an index, a call) the
/// expressions enclosing a point have together, counted from the last comma at each bracket
/// level. A chain such as `a + b + c` or `x["a"]["b"]` nests one level deeper per link.
pub const MAX_EXPRESSION_CHAIN: usize = 512;

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

/// The scan's state at one token.
struct Scan {
    /// The chain length of each open bracket level; the first is the statement's own level.
    chains: Vec<usize>,
    /// The sum of `chains`.
    chain: usize,
    /// The `elif` count of the `if` statement in progress at each open block; the first is the
    /// top level.
    elifs: Vec<usize>,
    /// The sum of `elifs`.
    elif: usize,
    /// Whether the token is the first of a statement.
    statement_start: bool,
    /// Whether the previous token ended an operand, so that a `(` or `[` is a call or an index.
    after_operand: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            chains: vec![0],
            chain: 0,
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
            Token::Newline | Token::Semicolon => {
                if self.chains.len() == 1 {
                    self.chain -= self.chains[0];
                    self.chains[0] = 0;
                }
                self.statement_start = true;
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
                    self.link(at)?;
                }
                self.chains.push(0);
                if self.chains.len() - 1 > MAX_BRACKET_DEPTH {
                    return Err(exceeded(
                        at,
                        format!("brackets nest deeper than {MAX_BRACKET_DEPTH} levels"),
                    ));
                }
            }
            Token::ClosingRound
            | Token::ClosingSquare
            | Token::ClosingCurly
            | Token::FStringExprEnd => {
                // An unbalanced bracket is the parser's syntax error.
                if self.chains.len() > 1 {
                    self.chain -= self.chains.pop().unwrap_or_default();
                }
                self.after_operand = true;
            }
            Token::Comma => {
                let current = self
                    .chains
                    .last_mut()
                    .expect("the statement level is never popped");
                self.chain -= std::mem::take(current);
            }
            Token::Identifier(_)
            | Token::Int(_)
            | Token::Float(_)
            | Token::String(_)
            | Token::Bytes(_)
            | Token::FStringEnd => self.after_operand = true,
            Token::FStringStart(_)
            | Token::FStringText(_)
            | Token::FStringBang
            | Token::Colon
            | Token::Pass
            | Token::Break
            | Token::Continue => {}
            // Every other token (operators, keywords, `.`) is a link; a statement keyword counts
            // one more than it nests, which only makes the bound stricter.
            _ => self.link(at)?,
        }
        Ok(())
    }

    /// Adds one link to the chain at the innermost bracket level.
    fn link(&mut self, at: usize) -> Result<(), Exceeded> {
        *self
            .chains
            .last_mut()
            .expect("the statement level is never popped") += 1;
        self.chain += 1;
        if self.chain > MAX_EXPRESSION_CHAIN {
            return Err(exceeded(
                at,
                format!(
                    "the expressions here chain more than {MAX_EXPRESSION_CHAIN} operators, \
                     keywords and accesses"
                ),
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
