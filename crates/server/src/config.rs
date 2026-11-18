//! The server's configuration file (design 13, ADR-456): one TOML file named by `--config`.
//! There is no default path, no search and no environment variable for the server's own
//! settings; the one exception is `PGOPTIONS`, which sqlx reads for the database connection
//! (`Database::connect_options`).

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use sqlx::postgres::{PgConnectOptions, PgSslMode};

/// The port the local deployment serves on unless the file names another (`api/openapi.yaml`).
pub const DEFAULT_PORT: u16 = 8080;
/// How long a graceful stop waits for requests in flight.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;
/// The longest grace period the file may ask for; a longer one is refused.
pub const MAX_SHUTDOWN_GRACE_SECS: u64 = 300;
/// How long the server waits for a database connection at start unless the file says otherwise.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// The longest connect timeout the file may ask for; a longer one is refused.
pub const MAX_CONNECT_TIMEOUT_SECS: u64 = 300;

/// The largest object the local store accepts unless the file says otherwise: 4 GiB.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

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

/// The database URL. It holds the password, so `Debug` shows nothing of it and it is never
/// logged (ADR-458).
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct DatabaseUrl(String);

impl DatabaseUrl {
    pub fn new(url: impl Into<String>) -> DatabaseUrl {
        DatabaseUrl(url.into())
    }

    /// The URL as written. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DatabaseUrl(..)")
    }
}

/// The `[database]` table (design 12, ADR-458).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `postgres://user:password@host:port/database`, with no query parameters. The file must be
    /// readable by the service account only.
    pub url: DatabaseUrl,
    /// The longest the server waits for a database connection at start, in seconds.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
}

fn default_connect_timeout_secs() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_SECS
}

impl Database {
    pub fn new(url: impl Into<String>) -> Database {
        Database {
            url: DatabaseUrl::new(url),
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
        }
    }

    /// The connection options from the URL. The error text is fixed: sqlx's own message may
    /// repeat a part of the URL, which holds the password.
    ///
    /// The URL must be `postgres://user:password@host[:port]/database` with nothing after the
    /// database name: a query string is refused before sqlx sees it, because sqlx logs the
    /// value of a parameter it does not know (a mistyped `?pasword=...` would reach the log),
    /// and the listed parts keep sqlx from filling them in from `PGHOST`, `PGUSER`,
    /// `PGPASSWORD`, `PGDATABASE`, `PGPORT` or `~/.pgpass`. TLS is off (ADR-458). One limit
    /// remains: sqlx reads `PGOPTIONS` and offers no way to unset it, so that variable, if set
    /// in the server's environment, still reaches the connection.
    pub(crate) fn connect_options(&self) -> Result<PgConnectOptions, ConfigError> {
        let text = self.url.expose();
        let url = url::Url::parse(text).map_err(|_| ConfigError::DatabaseUrl)?;
        let complete = matches!(url.scheme(), "postgres" | "postgresql")
            && url.host_str().is_some_and(|h| !h.is_empty())
            && !url.username().is_empty()
            && url.password().is_some_and(|p| !p.is_empty())
            && url.path().len() > 1
            && url.query().is_none()
            && url.fragment().is_none();
        if !complete {
            return Err(ConfigError::DatabaseUrl);
        }
        let options = PgConnectOptions::from_str(text).map_err(|_| ConfigError::DatabaseUrl)?;
        Ok(options
            .port(url.port().unwrap_or(5432))
            .ssl_mode(PgSslMode::Disable)
            .application_name("ngr-server"))
    }
}

/// The `[instruction_key]` table (design 11.1, ADR-462): where the private key that signs job
/// instructions is kept. There is no default path, no search and no environment variable
/// (ADR-456 item 7). The table holds a path and never key material.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionKeyConfig {
    /// An absolute path to a PKCS#8 file. The server creates it, readable by its account only,
    /// when the file is absent and the database records no active key; the directory must exist.
    pub path: PathBuf,
}

/// The `[storage]` table (design 13, 14.3; ADR-474): the local profile's object store, a
/// directory of files. There is no default path (ADR-456 item 7).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// An absolute path. The server creates the directory (mode 0700 on Unix) when it is absent
    /// and its parent exists.
    pub dir: PathBuf,
    /// The most bytes one object may hold; an append past it is refused. Not 0.
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
}

fn default_max_object_bytes() -> u64 {
    DEFAULT_MAX_OBJECT_BYTES
}

impl StorageConfig {
    pub fn new(dir: impl Into<PathBuf>) -> StorageConfig {
        StorageConfig {
            dir: dir.into(),
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
        }
    }
}

/// The `[object_server]` table (design 13, 13.3; ADR-491): the S3-compatible server the server
/// starts as its child and stops with itself. Every path is absolute and there is no search of
/// `PATH` (ADR-456 item 7). There is no bind-address key, no access-key key and no secret-key
/// key: the listener is `127.0.0.1` in code, and the credentials live in files in
/// `credentials_dir`, which the server creates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectServerConfig {
    /// The pinned RustFS executable (`scripts/fetch-rustfs.sh` fetches and checks it). The
    /// server does not hash it; on Unix it refuses one, or its directory, that group or others
    /// can write or that belongs to neither its account nor root.
    pub binary: PathBuf,
    /// The directory the object server keeps its data in. The server creates it (mode 0700 on
    /// Unix) when it is absent and its parent exists, locks it for its life and hands the
    /// `volume` directory inside it to the object server.
    pub data_dir: PathBuf,
    /// The directory of the two credential files the server creates (access key, secret key). It
    /// must exist; the server never creates it.
    pub credentials_dir: PathBuf,
    /// The loopback port the object server listens on; the default is `http.port` plus one.
    #[serde(default)]
    pub port: Option<u16>,
}

impl ObjectServerConfig {
    /// A table with the default port.
    pub fn new(
        binary: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        credentials_dir: impl Into<PathBuf>,
    ) -> ObjectServerConfig {
        ObjectServerConfig {
            binary: binary.into(),
            data_dir: data_dir.into(),
            credentials_dir: credentials_dir.into(),
            port: None,
        }
    }

    /// The port the object server listens on, given the server's own: `port`, else
    /// `http_port + 1`. Refused: 0, the server's own port, and an overflow of the default. A
    /// server port of 0 (a `Config` built in code that lets the system pick) is not compared,
    /// and needs an explicit `port`.
    pub fn resolved_port(&self, http_port: u16) -> Result<u16, ConfigError> {
        let port = match self.port {
            Some(port) => port,
            None => http_port
                .checked_add(1)
                .filter(|_| http_port != 0)
                .ok_or(ConfigError::ObjectServerPort)?,
        };
        if port == 0 || (http_port != 0 && port == http_port) {
            return Err(ConfigError::ObjectServerPort);
        }
        Ok(port)
    }
}

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub profile: Profile,
    #[serde(default)]
    pub http: Http,
    /// Required: the server does not start without its database.
    pub database: Database,
    /// Required: the server does not start without its instruction key.
    pub instruction_key: InstructionKeyConfig,
    /// Required: the server does not start without its object store.
    pub storage: StorageConfig,
    /// Required in a file (`Config::load` refuses a file without it). `None` only in a `Config`
    /// built in code, which starts no object server (tests).
    #[serde(default)]
    pub object_server: Option<ObjectServerConfig>,
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
    /// hold a secret (the database URL's password).
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
    #[error(
        "invalid configuration: database.connect_timeout_secs must be from 1 to \
         {MAX_CONNECT_TIMEOUT_SECS}"
    )]
    ConnectTimeout,
    /// The URL is not shown: it holds the password.
    #[error(
        "invalid configuration: database.url is not a postgres://user:password@host[:port]/database URL without parameters"
    )]
    DatabaseUrl,
    #[error("invalid configuration: instruction_key.path must be an absolute path")]
    InstructionKeyPath,
    #[error("invalid configuration: storage.dir must be an absolute path")]
    StorageDir,
    #[error("invalid configuration: storage.max_object_bytes must not be 0")]
    StorageMaxObjectBytes,
    #[error("invalid configuration: the [object_server] table is required")]
    ObjectServerMissing,
    #[error(
        "invalid configuration: object_server.binary, object_server.data_dir and \
         object_server.credentials_dir must be absolute paths"
    )]
    ObjectServerPath,
    #[error(
        "invalid configuration: object_server.port must be from 1 to 65535 and differ from \
         http.port (the default is http.port plus one, which must fit)"
    )]
    ObjectServerPort,
    #[error(
        "invalid configuration: object_server.data_dir must not be storage.dir, and \
         object_server.credentials_dir must not be the directory of instruction_key.path"
    )]
    ObjectServerShared,
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
        if !(1..=MAX_CONNECT_TIMEOUT_SECS).contains(&config.database.connect_timeout_secs) {
            return Err(ConfigError::ConnectTimeout);
        }
        config.database.connect_options()?;
        if !config.instruction_key.path.is_absolute() {
            return Err(ConfigError::InstructionKeyPath);
        }
        if !config.storage.dir.is_absolute() {
            return Err(ConfigError::StorageDir);
        }
        if config.storage.max_object_bytes == 0 {
            return Err(ConfigError::StorageMaxObjectBytes);
        }
        config.check_object_server()?;
        Ok(config)
    }

    /// The checks of `[object_server]` that need no file system. Nesting of one directory in
    /// another is not detected.
    fn check_object_server(&self) -> Result<(), ConfigError> {
        let object = self
            .object_server
            .as_ref()
            .ok_or(ConfigError::ObjectServerMissing)?;
        if ![&object.binary, &object.data_dir, &object.credentials_dir]
            .iter()
            .all(|path| path.is_absolute())
        {
            return Err(ConfigError::ObjectServerPath);
        }
        object.resolved_port(self.http.port)?;
        if object.data_dir == self.storage.dir
            || self.instruction_key.path.parent() == Some(object.credentials_dir.as_path())
        {
            return Err(ConfigError::ObjectServerShared);
        }
        Ok(())
    }

    /// Parses the text without validating values.
    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest valid file. The key table comes first, so text appended to `DB` lands in
    /// `[database]`. The path is absolute on the platform the test runs on.
    #[cfg(unix)]
    const DB: &str = "[instruction_key]\npath = \"/srv/nekoguruma/instruction.key\"\n\
                      [object_server]\nbinary = \"/opt/nekoguruma/rustfs\"\n\
                      data_dir = \"/srv/nekoguruma/object-server\"\n\
                      credentials_dir = \"/etc/nekoguruma\"\n\
                      [storage]\ndir = \"/srv/nekoguruma/objects\"\n\
                      [database]\nurl = \"postgres://ngr:pw@127.0.0.1:5432/nekoguruma\"\n";
    #[cfg(windows)]
    const DB: &str = "[instruction_key]\npath = 'C:\\srv\\nekoguruma\\instruction.key'\n\
                      [object_server]\nbinary = 'C:\\opt\\nekoguruma\\rustfs.exe'\n\
                      data_dir = 'C:\\srv\\nekoguruma\\object-server'\n\
                      credentials_dir = 'C:\\etc\\nekoguruma'\n\
                      [storage]\ndir = 'C:\\srv\\nekoguruma\\objects'\n\
                      [database]\nurl = \"postgres://ngr:pw@127.0.0.1:5432/nekoguruma\"\n";

    /// The tables of `DB` before `[database]`.
    fn key_table() -> &'static str {
        &DB[..DB.find("[database]").unwrap()]
    }

    /// `DB` followed by `extra` in its own table or at the end.
    fn with_db(extra: &str) -> String {
        format!("{extra}\n{DB}")
    }

    #[test]
    fn a_file_with_only_the_database_gives_the_defaults() {
        let c = Config::parse(DB).unwrap();
        assert_eq!(c.profile, Profile::Local);
        assert_eq!(c.http.port, 8080);
        assert_eq!(c.http.shutdown_grace_secs, 30);
        assert_eq!(c.database.connect_timeout_secs, 10);
    }

    #[test]
    fn the_database_table_and_its_url_are_required() {
        assert!(Config::parse("").is_err());
        assert!(Config::parse("[http]\nport = 9001\n").is_err());
        assert!(Config::parse("[database]\nconnect_timeout_secs = 3\n").is_err());
    }

    #[test]
    fn the_instruction_key_table_and_its_path_are_required() {
        let db_only = format!(
            "{}[database]\nurl = \"postgres://ngr:pw@127.0.0.1:5432/nekoguruma\"\n",
            storage_table()
        );
        assert!(Config::parse(&db_only).is_err());
        assert!(Config::parse(&format!("[instruction_key]\n{db_only}")).is_err());
        assert!(
            Config::parse(&format!(
                "[instruction_key]\npath = \"/k\"\nmode = 1\n{db_only}"
            ))
            .is_err()
        );
        assert!(Config::parse(&format!("[instruction_key]\npath = \"/k\"\n{db_only}")).is_ok());
    }

    #[test]
    fn load_refuses_a_relative_instruction_key_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.toml");
        let text = "[instruction_key]\npath = \"keys/instruction.key\"\n\
                    [storage]\ndir = \"/srv/objects\"\n\
                    [database]\nurl = \"postgres://ngr:pw@127.0.0.1:5432/nekoguruma\"\n";
        std::fs::write(&path, text).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::InstructionKeyPath)
        ));
        std::fs::write(&path, DB).unwrap();
        assert!(Config::load(&path).is_ok());
    }

    // T15: the `[storage]` table.
    #[test]
    fn the_storage_table_and_its_dir_are_required() {
        let without = DB.replace(&storage_table(), "");
        assert!(Config::parse(&without).is_err());
        let empty = DB.replace(&storage_table(), "[storage]\n");
        assert!(Config::parse(&empty).is_err());
        let unknown = format!("{}extra = 1\n", storage_table());
        assert!(Config::parse(&DB.replace(&storage_table(), &unknown)).is_err());
    }

    /// The `[storage]` table of `DB`, with its key line.
    fn storage_table() -> String {
        let from = DB.find("[storage]").unwrap();
        let to = DB.find("[database]").unwrap();
        DB[from..to].to_owned()
    }

    #[test]
    fn the_object_size_limit_defaults_to_4_gib() {
        let c = Config::parse(DB).unwrap();
        assert_eq!(c.storage.max_object_bytes, 4 * 1024 * 1024 * 1024);
        let set = DB.replace(
            &storage_table(),
            &format!("{}max_object_bytes = 16\n", storage_table()),
        );
        assert_eq!(Config::parse(&set).unwrap().storage.max_object_bytes, 16);
    }

    #[test]
    fn load_refuses_a_relative_storage_dir_and_a_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage.toml");
        let relative = DB.replace(&storage_table(), "[storage]\ndir = \"objects\"\n");
        std::fs::write(&path, relative).unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::StorageDir)));
        let zero = DB.replace(
            &storage_table(),
            &format!("{}max_object_bytes = 0\n", storage_table()),
        );
        std::fs::write(&path, zero).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::StorageMaxObjectBytes)
        ));
        std::fs::write(&path, DB).unwrap();
        assert!(Config::load(&path).is_ok());
    }

    /// `DB` with its `[object_server]` table replaced by `table` (empty: removed).
    fn with_object_server(table: &str) -> String {
        let from = DB.find("[object_server]").unwrap();
        let to = DB.find("[storage]").unwrap();
        format!("{}{table}{}", &DB[..from], &DB[to..])
    }

    /// The `[object_server]` table of `DB`.
    fn object_server_table() -> String {
        let from = DB.find("[object_server]").unwrap();
        let to = DB.find("[storage]").unwrap();
        DB[from..to].to_owned()
    }

    /// Loads `text` from a file.
    fn load(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    // C1: the table is required in a file and optional in a `Config` built in code.
    #[test]
    fn load_requires_the_object_server_table_and_parse_does_not() {
        let without = with_object_server("");
        assert_eq!(Config::parse(&without).unwrap().object_server, None);
        assert!(matches!(
            load(&without),
            Err(ConfigError::ObjectServerMissing)
        ));
        let c = load(DB).unwrap();
        assert!(c.object_server.is_some());
        // The three paths are required keys of the table.
        for key in ["binary", "data_dir", "credentials_dir"] {
            let text: String = object_server_table()
                .lines()
                .filter(|line| !line.starts_with(key))
                .map(|line| format!("{line}\n"))
                .collect();
            assert!(Config::parse(&with_object_server(&text)).is_err(), "{key}");
        }
    }

    // C2: no key can move the listener or carry a credential.
    #[test]
    fn the_object_server_table_has_no_bind_address_or_secret_keys() {
        for key in [
            "bind = \"0.0.0.0\"",
            "address = \"0.0.0.0:9000\"",
            "host = \"0.0.0.0\"",
            "access_key = \"a\"",
            "secret_key = \"s3cret-value\"",
            "secret = \"s3cret-value\"",
        ] {
            let text = format!("{}{key}\n", object_server_table());
            assert!(Config::parse(&with_object_server(&text)).is_err(), "{key}");
            // `load` words the error without the source line.
            let error = load(&with_object_server(&text)).unwrap_err();
            let shown = format!("{error} / {error:?}");
            assert!(!shown.contains("s3cret-value"), "{key}: {shown}");
        }
    }

    // C3
    #[test]
    fn load_refuses_a_relative_object_server_path() {
        for key in ["binary", "data_dir", "credentials_dir"] {
            let text: String = object_server_table()
                .lines()
                .map(|line| {
                    if line.starts_with(key) {
                        format!("{key} = \"relative/path\"\n")
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect();
            assert!(
                matches!(
                    load(&with_object_server(&text)),
                    Err(ConfigError::ObjectServerPath)
                ),
                "{key}"
            );
        }
    }

    // C4
    #[test]
    fn the_object_server_port_defaults_to_the_servers_plus_one_and_refuses_the_rest() {
        let object = ObjectServerConfig::new("/b", "/d", "/c");
        assert_eq!(object.resolved_port(8080).unwrap(), 8081);
        assert_eq!(load(DB).unwrap().object_server.unwrap().port, None);
        // The default overflows at 65535; 0 and the server's own port are refused.
        assert!(object.resolved_port(65535).is_err());
        let with = |port: u16| ObjectServerConfig {
            port: Some(port),
            ..object.clone()
        };
        assert_eq!(with(9000).resolved_port(8080).unwrap(), 9000);
        assert_eq!(with(9000).resolved_port(65535).unwrap(), 9000);
        assert!(with(0).resolved_port(8080).is_err());
        assert!(with(8080).resolved_port(8080).is_err());
        // A server port of 0 (code-built) needs an explicit port, and is not compared.
        assert!(object.resolved_port(0).is_err());
        assert_eq!(with(9000).resolved_port(0).unwrap(), 9000);
        // The same rules apply when a file is loaded.
        for (http, port) in [
            ("[http]\nport = 65535\n", ""),
            ("", "port = 0\n"),
            ("[http]\nport = 9000\n", "port = 9000\n"),
        ] {
            let table = format!("{}{port}", object_server_table());
            let text = format!("{http}{}", with_object_server(&table));
            assert!(
                matches!(load(&text), Err(ConfigError::ObjectServerPort)),
                "{http}{port}"
            );
        }
        let ok = format!("{}port = 9100\n", object_server_table());
        let loaded = load(&with_object_server(&ok)).unwrap();
        assert_eq!(loaded.object_server.unwrap().port, Some(9100));
    }

    // C5
    #[test]
    fn load_refuses_the_data_dir_of_the_store_and_the_key_directory_as_credentials() {
        let same_data = DB.replace(
            &object_server_table(),
            &object_server_table().replace("object-server", "objects"),
        );
        assert!(matches!(
            load(&same_data),
            Err(ConfigError::ObjectServerShared)
        ));
        let key_dir = if cfg!(windows) {
            "C:\\srv\\nekoguruma"
        } else {
            "/srv/nekoguruma"
        };
        let table = object_server_table();
        let (head, _) = table.split_once("credentials_dir").unwrap();
        let shared = DB.replace(&table, &format!("{head}credentials_dir = '{key_dir}'\n"));
        assert!(matches!(
            load(&shared),
            Err(ConfigError::ObjectServerShared)
        ));
    }

    #[test]
    fn the_database_url_is_not_shown_by_debug() {
        let c = Config::parse(&with_db("")).unwrap();
        assert!(!format!("{c:?}").contains("pw"), "{c:?}");
    }

    #[test]
    fn values_are_read() {
        let c = Config::parse(&with_db(
            "profile = \"local\"\n[http]\nport = 9001\nshutdown_grace_secs = 5\n",
        ))
        .unwrap();
        assert_eq!(c.http.port, 9001);
        assert_eq!(c.http.shutdown_grace_secs, 5);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(Config::parse(&with_db("colour = 1")).is_err());
        assert!(Config::parse(&with_db("[http]\nport = 1\nextra = 2")).is_err());
        assert!(Config::parse(&format!("{DB}extra = 1\n")).is_err());
    }

    #[test]
    fn a_bind_address_key_does_not_exist() {
        assert!(Config::parse(&with_db("[http]\nbind = \"0.0.0.0\"")).is_err());
        assert!(Config::parse(&with_db("[http]\nhost = \"localhost\"")).is_err());
    }

    #[test]
    fn another_profile_is_refused() {
        assert!(Config::parse(&with_db("profile = \"standard\"")).is_err());
    }

    #[test]
    fn a_port_out_of_range_or_of_the_wrong_type_is_refused() {
        assert!(Config::parse(&with_db("[http]\nport = 70000")).is_err());
        assert!(Config::parse(&with_db("[http]\nport = \"80\"")).is_err());
    }

    #[test]
    fn load_refuses_port_zero_a_missing_file_and_bad_toml() {
        let dir = tempfile::tempdir().unwrap();
        let zero = dir.path().join("zero.toml");
        std::fs::write(&zero, with_db("[http]\nport = 0\n")).unwrap();
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
        std::fs::write(&long, with_db("[http]\nshutdown_grace_secs = 301\n")).unwrap();
        assert!(matches!(
            Config::load(&long),
            Err(ConfigError::GraceTooLong)
        ));
        std::fs::write(&long, with_db("[http]\nshutdown_grace_secs = 300\n")).unwrap();
        assert!(Config::load(&long).is_ok());
    }

    #[test]
    fn load_reads_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, with_db("[http]\nport = 8123\n")).unwrap();
        assert_eq!(Config::load(&ok).unwrap().http.port, 8123);
    }

    #[test]
    fn load_refuses_a_bad_connect_timeout_and_a_bad_database_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.toml");
        for secs in [0, 301] {
            let text = format!("{DB}connect_timeout_secs = {secs}\n");
            std::fs::write(&path, text).unwrap();
            assert!(matches!(
                Config::load(&path),
                Err(ConfigError::ConnectTimeout)
            ));
        }
        std::fs::write(&path, format!("{DB}connect_timeout_secs = 300\n")).unwrap();
        assert!(Config::load(&path).is_ok());

        for url in [
            "mysql://u:s3cret-value@h/db",
            "s3cret-value",
            "postgres://u:s3cret-value@h:notaport/db",
            "postgres://u:s3cret-value@h/db?sslmode=s3cret-value",
            "postgres://u:s3cret-value@h/db?sslpassword=s3cret-value",
            "postgres://u:s3cret-value@h/db#s3cret-value",
            "postgres://u:s3cret-value@h",
            "postgres://u@h/db",
            "postgres://:s3cret-value@h/db",
        ] {
            std::fs::write(
                &path,
                format!("{}[database]\nurl = \"{url}\"\n", key_table()),
            )
            .unwrap();
            let error = Config::load(&path).unwrap_err();
            assert!(matches!(error, ConfigError::DatabaseUrl), "{url}");
            let shown = format!("{error} / {error:?}");
            assert!(!shown.contains("s3cret-value"), "{shown}");
        }
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
            "[database]\nurl = \"postgres://u:s3cret-value@h/db\"\nport = 1\n",
            "[database]\nurl = \"postgres://u:s3cret-value@h/db\"\nconnect_timeout_secs = \"s3cret-value\"\n",
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
