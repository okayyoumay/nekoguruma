//! The record format of RFC 4180 section 2, with LF line ends allowed (`docs/csv-tables.md`
//! section 2).

/// One record and the line on which it starts (the first line is 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) line: u32,
    pub(crate) cells: Vec<String>,
}

impl Record {
    /// An empty line, or a record whose cells are all empty: skipped before anything else.
    pub(crate) fn is_blank(&self) -> bool {
        self.cells.iter().all(String::is_empty)
    }
}

/// Why a file is not a sequence of records, and the line where it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    pub(crate) line: u32,
    pub(crate) hint: &'static str,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Splits a file into records. A byte order mark at the start is skipped; any other invalid
/// UTF-8, a quote inside an unquoted cell, text after a closing quote, a carriage return that
/// does not end a line, and an unterminated quoted cell are syntax errors.
pub(crate) fn parse(bytes: &[u8]) -> Result<Vec<Record>, SyntaxError> {
    let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|e| SyntaxError {
        line: line_of(&bytes[..e.valid_up_to()]),
        hint: "the file is not valid UTF-8",
    })?;
    let mut records = Vec::new();
    let mut chars = text.chars().peekable();
    let mut line: u32 = 1;
    while chars.peek().is_some() {
        let start = line;
        let mut cells = Vec::new();
        let mut cell = String::new();
        loop {
            // At the start of a cell.
            if chars.peek() == Some(&'"') {
                chars.next();
                loop {
                    match chars.next() {
                        None => {
                            return Err(SyntaxError {
                                line: start,
                                hint: "a quoted cell is not closed",
                            });
                        }
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            cell.push('"');
                        }
                        Some('"') => break,
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            cell.push(c);
                        }
                    }
                }
                match chars.peek() {
                    None | Some(',' | '\n' | '\r') => {}
                    Some(_) => {
                        return Err(SyntaxError {
                            line,
                            hint: "text follows a closing quote",
                        });
                    }
                }
            } else {
                while let Some(&c) = chars.peek() {
                    match c {
                        ',' | '\n' | '\r' => break,
                        '"' => {
                            return Err(SyntaxError {
                                line,
                                hint: "a quote inside a cell that is not quoted",
                            });
                        }
                        _ => {
                            cell.push(c);
                            chars.next();
                        }
                    }
                }
            }
            cells.push(std::mem::take(&mut cell));
            match chars.next() {
                Some(',') => continue,
                Some('\r') => {
                    if chars.next() != Some('\n') {
                        return Err(SyntaxError {
                            line,
                            hint: "a carriage return that does not end a line",
                        });
                    }
                    line += 1;
                    break;
                }
                Some('\n') => {
                    line += 1;
                    break;
                }
                // The last record need not end with a line break.
                None => break,
                Some(_) => unreachable!("a cell ends only at a comma, a line end or the end"),
            }
        }
        records.push(Record { line: start, cells });
    }
    Ok(records)
}

/// The line on which the byte after `prefix` lies.
fn line_of(prefix: &[u8]) -> u32 {
    let breaks = prefix.iter().filter(|&&b| b == b'\n').count();
    u32::try_from(breaks).map_or(u32::MAX, |n| n.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str) -> Vec<(u32, Vec<String>)> {
        parse(text.as_bytes())
            .expect("parses")
            .into_iter()
            .map(|r| (r.line, r.cells))
            .collect()
    }

    fn row(line: u32, cells: &[&str]) -> (u32, Vec<String>) {
        (line, cells.iter().map(|c| (*c).to_owned()).collect())
    }

    #[test]
    fn records_end_with_crlf_lf_or_the_end_of_the_file() {
        assert_eq!(
            cells("a,b\r\nc,d\ne,f"),
            vec![
                row(1, &["a", "b"]),
                row(2, &["c", "d"]),
                row(3, &["e", "f"])
            ]
        );
        assert_eq!(cells("a\n"), vec![row(1, &["a"])]);
        assert_eq!(cells(""), vec![]);
    }

    #[test]
    fn quoted_cells_hold_commas_quotes_and_line_breaks() {
        assert_eq!(
            cells("\"a,b\",\"say \"\"hi\"\"\",\"two\nlines\"\nnext,\"\"\n"),
            vec![
                row(1, &["a,b", "say \"hi\"", "two\nlines"]),
                row(3, &["next", ""]),
            ]
        );
    }

    #[test]
    fn spaces_and_empty_cells_are_kept() {
        assert_eq!(
            cells(" a ,,\n\n"),
            vec![row(1, &[" a ", "", ""]), row(2, &[""])]
        );
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(
            cells("\u{FEFF}id\n1\n"),
            vec![row(1, &["id"]), row(2, &["1"])]
        );
    }

    #[test]
    fn syntax_errors_name_their_line() {
        let error = |bytes: &[u8]| parse(bytes).expect_err("rejected");
        assert_eq!(error(b"a\n\"open\nstill").line, 2);
        assert_eq!(error(b"a\nb\"c\n").line, 2);
        assert_eq!(error(b"\"a\"b\n").line, 1);
        assert_eq!(error(b"a\rb\n").line, 1);
        assert_eq!(error(b"a\n\"x\ny\"z\n").line, 3);
        // Invalid UTF-8 on line 3.
        assert_eq!(error(b"a\nb\nc\xFF\n").line, 3);
        // A second byte order mark is not skipped; it is just a character.
        assert!(parse(b"\xEF\xBB\xBF\xEF\xBB\xBFa").is_ok());
    }
}
