//! The server's configuration file (design 13, ADR-456): one TOML file named by `--config`.
//! There is no default path, no search and no environment variable.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The port the local deployment serves on unless the file names another (`api/openapi.yaml`).
pub const DEFAULT_PORT: u16 = 8080;
/// How long a graceful stop waits for requests in flight.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;
/// The longest grace period the file may ask for; a longer one is refused.
pub const MAX_SHUTDOWN_GRACE_SECS: u64 = 300;

/// The deployment profile (design 13). This build serves the local profile only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Bundled-database, loopback-only deployment (design 13.3).
    #[default]
    Local,
}

/// The `[http]` table. There is no bind-address key: the listener is fixed to 127.0.0.1 in
/// code, so no configuration can make the local profile listen elsewhere (design 13.3).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    /// The listener's port. The pinned origin is `http://127.0.0.1:<port>`, so changing it
    /// changes the origin the browser keeps its per-origin data under (ADR-456).
    pub port: u16,
    /// The longest a graceful stop waits for requests in flight, in seconds.
    pub shutdown_grace_secs: u64,
}

impl Default for Http {
    fn default() -> Self {
        Http {
            port: DEFAULT_PORT,
            shutdown_grace_secs: DEFAULT_SHUTDOWN_GRACE_SECS,
        }
    }
}

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub profile: Profile,
    pub http: Http,
}

/// Why a configuration file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the configuration file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Holds the parser's message and the position, never the offending source line: a line may
    /// hold a secret (the database URL's password, once the server has one).
    #[error("invalid configuration file {}{}: {message}", path.display(), at(*line, *column))]
    Parse {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        column: Option<usize>,
    },
    #[error("invalid configuration: http.port must not be 0")]
    PortZero,
    #[error(
        "invalid configuration: http.shutdown_grace_secs must be at most {MAX_SHUTDOWN_GRACE_SECS}"
    )]
    GraceTooLong,
}

fn at(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(" at line {l}, column {c}"),
        _ => String::new(),
    }
}

/// Replaces what sits between double quotes in a parser message: serde quotes the offending
/// string value (`invalid type: string "..."`), and a value may be a secret.
fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut quoted = false;
    let mut escaped = false;
    for c in message.chars() {
        if !quoted {
            out.push(c);
            quoted = c == '"';
        } else if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            out.push_str("...\"");
            quoted = false;
        }
    }
    if quoted {
        out.push_str("...\"");
    }
    out
}

/// The 1-based line and column of byte offset `offset` in `text`.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let start = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    (line, before.len() - start + 1)
}

impl Config {
    /// Reads and validates the file. Port 0 is refused here; a `Config` built in code may carry
    /// it, and `Server::start` then takes the port the operating system picked.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config = Config::parse(&text).map_err(|error| {
            let (line, column) = match error.span() {
                Some(span) => {
                    let (l, c) = position(&text, span.start);
                    (Some(l), Some(c))
                }
                None => (None, None),
            };
            ConfigError::Parse {
                path: path.to_owned(),
                message: redact(error.message()),
                line,
                column,
            }
        })?;
        if config.http.port == 0 {
            return Err(ConfigError::PortZero);
        }
        if config.http.shutdown_grace_secs > MAX_SHUTDOWN_GRACE_SECS {
            return Err(ConfigError::GraceTooLong);
        }
        Ok(config)
    }

    /// Parses the text without validating values.
    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_the_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.profile, Profile::Local);
        assert_eq!(c.http.port, 8080);
        assert_eq!(c.http.shutdown_grace_secs, 30);
    }

    #[test]
    fn values_are_read() {
        let c =
            Config::parse("profile = \"local\"\n[http]\nport = 9001\nshutdown_grace_secs = 5\n")
                .unwrap();
        assert_eq!(c.http.port, 9001);
        assert_eq!(c.http.shutdown_grace_secs, 5);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(Config::parse("colour = 1").is_err());
        assert!(Config::parse("[http]\nport = 1\nextra = 2").is_err());
    }

    #[test]
    fn a_bind_address_key_does_not_exist() {
        assert!(Config::parse("[http]\nbind = \"0.0.0.0\"").is_err());
        assert!(Config::parse("[http]\nhost = \"localhost\"").is_err());
    }

    #[test]
    fn another_profile_is_refused() {
        assert!(Config::parse("profile = \"standard\"").is_err());
    }

    #[test]
    fn a_port_out_of_range_or_of_the_wrong_type_is_refused() {
        assert!(Config::parse("[http]\nport = 70000").is_err());
        assert!(Config::parse("[http]\nport = \"80\"").is_err());
    }

    #[test]
    fn load_refuses_port_zero_a_missing_file_and_bad_toml() {
        let dir = tempfile::tempdir().unwrap();
        let zero = dir.path().join("zero.toml");
        std::fs::write(&zero, "[http]\nport = 0\n").unwrap();
        assert!(matches!(Config::load(&zero), Err(ConfigError::PortZero)));

        let missing = dir.path().join("missing.toml");
        assert!(matches!(
            Config::load(&missing),
            Err(ConfigError::Read { .. })
        ));

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[http\n").unwrap();
        assert!(matches!(Config::load(&bad), Err(ConfigError::Parse { .. })));

        let long = dir.path().join("long.toml");
        std::fs::write(&long, "[http]\nshutdown_grace_secs = 301\n").unwrap();
        assert!(matches!(
            Config::load(&long),
            Err(ConfigError::GraceTooLong)
        ));
        std::fs::write(&long, "[http]\nshutdown_grace_secs = 300\n").unwrap();
        assert!(Config::load(&long).is_ok());
    }

    #[test]
    fn load_reads_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "[http]\nport = 8123\n").unwrap();
        assert_eq!(Config::load(&ok).unwrap().http.port, 8123);
    }

    #[test]
    fn a_parse_error_names_the_position_and_never_the_source_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.toml");
        for text in [
            "[http]\nport = 8080\npassw0rd = \"s3cret-value\"\n",
            "[http]\nport = 8080 s3cret-value\n",
            "# s3cret-value\n[http\n",
            "[http]\nport = \"s3cret-value\"\n",
        ] {
            std::fs::write(&path, text).unwrap();
            let error = Config::load(&path).unwrap_err();
            let shown = format!("{error} / {error:?}");
            assert!(!shown.contains("s3cret-value"), "{shown}");
            assert!(shown.contains("line "), "{shown}");
        }
    }

    #[test]
    fn redact_hides_quoted_values_only() {
        assert_eq!(
            redact("invalid type: string \"a\\\"b\", expected u16"),
            "invalid type: string \"...\", expected u16"
        );
        assert_eq!(redact("unknown field `x`"), "unknown field `x`");
    }

    #[test]
    fn positions_are_one_based() {
        assert_eq!(position("ab\ncd", 0), (1, 1));
        assert_eq!(position("ab\ncd", 4), (2, 2));
    }
}
