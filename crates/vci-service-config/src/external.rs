//! Text an operational log entry carries apart from its message (design 12.2, ADR-348 item 6).
//!
//! An entry's `external` field holds what NGR cannot check against the list of what an
//! operational log never holds: text NGR does not write itself (a worker's or a vendor
//! library's text, a file path) and names that may identify a person or a device (a VCI name, a
//! J2534 device name). [`External`] escapes it, so one entry stays one line and the text cannot
//! imitate another entry or field.

use std::fmt::{self, Write};

/// `T`'s text, escaped for an entry's `external` field: a backslash, a line break, a tab, every
/// other control character and the Unicode line and paragraph separators are written as escapes
/// (`\\`, `\n`, `\r`, `\t`, `\u{1b}`), so the text holds no character that ends or breaks a
/// line. Everything else is written as it is.
///
/// ```
/// use vci_service_config::External;
///
/// assert_eq!(External("a\nb\u{1b}").to_string(), r"a\nb\u{1b}");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct External<T>(pub T);

impl<T: fmt::Display> fmt::Display for External<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(Escaping(f), "{}", self.0)
    }
}

/// A writer that escapes what it passes on to `f`.
struct Escaping<'a, 'b>(&'a mut fmt::Formatter<'b>);

impl Write for Escaping<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for c in text.chars() {
            match c {
                '\\' => self.0.write_str(r"\\")?,
                '\n' => self.0.write_str(r"\n")?,
                '\r' => self.0.write_str(r"\r")?,
                '\t' => self.0.write_str(r"\t")?,
                c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                    write!(self.0, "\\u{{{:x}}}", c as u32)?
                }
                c => self.0.write_char(c)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::External;

    /// A line break and a control character are escaped, so the text stays on one line and
    /// cannot start an entry of its own.
    #[test]
    fn line_breaks_and_control_characters_are_escaped() {
        let text = "failed\n{\"level\":\"error\"}\r\x1b[2J\0";
        let escaped = External(text).to_string();
        assert_eq!(escaped, r#"failed\n{"level":"error"}\r\u{1b}[2J\u{0}"#);
        assert!(!escaped.chars().any(char::is_control), "{escaped:?}");
    }

    /// A backslash is escaped too, so an escape in the text cannot pass for one of ours.
    #[test]
    fn a_backslash_is_escaped() {
        assert_eq!(External(r"C:\n\x").to_string(), r"C:\\n\\x");
    }

    /// C1 controls, DEL and the Unicode line and paragraph separators are escaped; other text,
    /// tabs aside, is kept.
    #[test]
    fn other_line_ending_characters_are_escaped_and_other_text_is_kept() {
        assert_eq!(
            External("a\u{85}b\u{7f}c\u{2028}d\u{2029}e\tf").to_string(),
            r"a\u{85}b\u{7f}c\u{2028}d\u{2029}e\tf"
        );
        assert_eq!(
            External("ä 診断 /home/user").to_string(),
            "ä 診断 /home/user"
        );
    }

    /// Any `Display` value is escaped as it is written, a path's included.
    #[test]
    fn a_display_value_is_escaped() {
        let path = std::path::Path::new("dir\nname");
        assert_eq!(External(path.display()).to_string(), r"dir\nname");
    }
}
