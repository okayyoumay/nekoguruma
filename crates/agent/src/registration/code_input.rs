//! Reading the registration code for `ngr-agent register` (design 6.4, ADR-463, ADR-470).
//!
//! The code comes from standard input or from the file `--code-file` names, never from an
//! argument. At most [`MAX_INPUT`] bytes are read, so a stream that never stops is not read to its
//! end. What was read is parsed by [`RegistrationCode::parse`] (which trims outer whitespace and
//! takes the typed variants), and the canonical text is what is sent. The input is never echoed:
//! no message holds a byte of it or the file's path, only the kind of I/O error.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use shared_crypto::Zeroizing;
use shared_crypto::registration::RegistrationCode;

use super::{Exit, RegisterError};

/// The most bytes of the input that are read, a code with room for padding.
pub const MAX_INPUT: usize = 256;

/// Where the code is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeSource {
    Stdin,
    File(PathBuf),
}

/// Reads and parses the code. A stream that never ends makes this wait (it holds no lock).
pub fn read(source: &CodeSource) -> Result<RegistrationCode, RegisterError> {
    match source {
        CodeSource::Stdin => read_from(io::stdin().lock()).map_err(|error| match error {
            InputError::Io(kind) => cannot(format!("cannot read standard input: {kind:?}")),
            InputError::Empty => cannot("no registration code on standard input".to_owned()),
            other => cannot(other.to_string()),
        }),
        CodeSource::File(path) => {
            let file = File::open(path).map_err(|e| file_error(e.kind()))?;
            read_from(file).map_err(|error| match error {
                InputError::Io(kind) => file_error(kind),
                InputError::Empty => {
                    cannot("no registration code in the file given with --code-file".to_owned())
                }
                other => cannot(other.to_string()),
            })
        }
    }
}

fn file_error(kind: io::ErrorKind) -> RegisterError {
    cannot(format!(
        "cannot read the file given with --code-file: {kind:?}"
    ))
}

fn cannot(message: String) -> RegisterError {
    RegisterError::new(Exit::CannotRegister, message)
}

/// Why the input is not a code. The text never holds the input.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum InputError {
    #[error("cannot read the input: {0:?}")]
    Io(io::ErrorKind),
    #[error("the input is empty or only whitespace")]
    Empty,
    #[error("the input holding the registration code is longer than {MAX_INPUT} bytes")]
    TooLong,
    #[error("the input holding the registration code is not UTF-8")]
    NotUtf8,
    #[error("the input holding the registration code starts with a byte order mark")]
    Bom,
    #[error("{0}")]
    Code(shared_crypto::registration::CodeError),
}

fn read_from(reader: impl Read) -> Result<RegistrationCode, InputError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_INPUT + 1));
    reader
        .take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| InputError::Io(e.kind()))?;
    if bytes.len() > MAX_INPUT {
        return Err(InputError::TooLong);
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(InputError::Bom);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| InputError::NotUtf8)?;
    if text
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return Err(InputError::Empty);
    }
    RegistrationCode::parse(text).map_err(InputError::Code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> RegistrationCode {
        RegistrationCode::generate().unwrap()
    }

    fn parse(bytes: &[u8]) -> Result<RegistrationCode, InputError> {
        read_from(bytes)
    }

    /// C1.
    #[test]
    fn a_code_is_taken_with_its_line_ending_spaces_case_and_variants() {
        let code = code();
        let text = code.as_str().to_owned();
        for input in [
            text.clone(),
            format!("{text}\n"),
            format!("{text}\r\n"),
            format!("  {text} \n"),
            format!("\t{text}\r\n\r\n"),
            text.to_lowercase(),
            text.to_uppercase(),
        ] {
            assert_eq!(parse(input.as_bytes()).unwrap(), code, "{input:?}");
        }
        // The `I`, `L` and `O` variants of a digit.
        let (prefix, digits) = text.split_at(5);
        let variant = format!("{prefix}{}", digits.replace('1', "I").replace('0', "o"));
        assert_eq!(parse(variant.as_bytes()).unwrap(), code);
    }

    /// C3 and C4.
    #[test]
    fn nothing_and_whitespace_are_empty() {
        for input in [&b""[..], b"\n", b"  \r\n\t "] {
            assert_eq!(parse(input).unwrap_err(), InputError::Empty);
        }
    }

    /// C5.
    #[test]
    fn something_that_is_not_a_code_gets_the_codes_error() {
        assert!(matches!(parse(b"hello"), Err(InputError::Code(_))));
        let mut wrong_prefix = code().as_str().replace("ngr1-", "ngr2-");
        assert!(matches!(
            parse(wrong_prefix.as_bytes()),
            Err(InputError::Code(
                shared_crypto::registration::CodeError::Prefix
            ))
        ));
        wrong_prefix.push('A');
        assert!(matches!(
            parse(wrong_prefix.as_bytes()),
            Err(InputError::Code(_))
        ));
        // A separator inside the code.
        let text = code().as_str().to_owned();
        let split = format!("{}-{}", &text[..9], &text[9..]);
        assert!(matches!(parse(split.as_bytes()), Err(InputError::Code(_))));
    }

    /// C6.
    #[test]
    fn too_long_not_utf8_and_a_bom_are_refused() {
        let code = code();
        let long = format!("{}{}", code.as_str(), " ".repeat(MAX_INPUT));
        assert_eq!(parse(long.as_bytes()).unwrap_err(), InputError::TooLong);
        // Exactly the limit passes.
        let exact = format!(
            "{}{}",
            code.as_str(),
            " ".repeat(MAX_INPUT - code.as_str().len())
        );
        assert_eq!(exact.len(), MAX_INPUT);
        assert_eq!(parse(exact.as_bytes()).unwrap(), code);
        assert_eq!(parse(&[0xFF, 0xFE, b'a']).unwrap_err(), InputError::NotUtf8);
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(code.as_str().as_bytes());
        assert_eq!(parse(&bom).unwrap_err(), InputError::Bom);
    }

    /// At most 257 bytes are read, so an endless stream ends.
    #[test]
    fn an_endless_stream_is_not_read_to_its_end() {
        let endless = io::repeat(b'a');
        assert_eq!(read_from(endless).unwrap_err(), InputError::TooLong);
    }

    /// C7, and that no message holds the path.
    #[test]
    fn a_file_that_cannot_be_read_names_only_the_io_kind() {
        let dir = tempfile::tempdir().unwrap();
        let secret_name = dir.path().join("secret-file-name");
        for path in [secret_name.clone(), dir.path().to_owned()] {
            let error = read(&CodeSource::File(path)).unwrap_err();
            assert_eq!(error.exit_code(), 1);
            let text = error.to_string();
            assert!(
                text.starts_with("cannot read the file given with --code-file: "),
                "{text}"
            );
            assert!(!text.contains("secret-file-name"), "{text}");
            assert!(!text.contains(dir.path().to_str().unwrap()), "{text}");
        }
    }

    #[test]
    fn a_code_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let code = code();
        let path = dir.path().join("code.txt");
        std::fs::write(&path, format!("{}\n", code.as_str())).unwrap();
        assert_eq!(read(&CodeSource::File(path)).unwrap(), code);
    }
}
