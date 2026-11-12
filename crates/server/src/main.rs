//! `ngr-server --config <file>`: the server, `ngr-server registration-code ...`: the command
//! that issues an agent registration code, and `ngr-server user add|set-password|disable ...`:
//! the commands for local users (`crates/server/docs/ngr-server.md`).
//! Exit codes: 0 after a graceful stop, an issued code or a finished user command, 1 for a start,
//! run, issuance or user-command failure (a refused password, an unusable database, a user that
//! exists or does not), 2 for a usage error, which includes a terminal on standard input where a
//! command wants a password.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use tokio::sync::oneshot;

use server::local_user::{self, NewUser};
use server::registration::{self, IssueRequest};
use server::{Config, Server, db};

const USAGE: &str = "usage: ngr-server --config <file>\n       \
ngr-server registration-code --config <file> --owner <subject> --name <display name> \\\n         \
--agent-key <64 hex digits> [--ttl-minutes <1-60>]\n       \
ngr-server user add --config <file> --username <name> --name <display name> [--role <role>]...\n       \
ngr-server user set-password --config <file> --username <name>\n       \
ngr-server user disable --config <file> --username <name>\n\
the password of `user add` and `user set-password` is read from standard input, which must not be \
a terminal\n";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Serve with the configuration file.
    Serve { config: PathBuf },
    /// Issue a registration code (ADR-463, ADR-465).
    RegistrationCode {
        config: PathBuf,
        request: IssueRequest,
    },
    /// A command for local users (ADR-469).
    User(UserCommand),
}

/// What `ngr-server user` is asked to do.
#[derive(Debug, PartialEq, Eq)]
enum UserCommand {
    Add { config: PathBuf, user: NewUser },
    SetPassword { config: PathBuf, username: String },
    Disable { config: PathBuf, username: String },
}

/// The subcommand word is one only in first position: `ngr-server --config registration-code`
/// still serves, with a configuration file of that name.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    if args
        .peek()
        .is_some_and(|first| first == "registration-code")
    {
        args.next();
        return parse_registration_code(args);
    }
    if args.peek().is_some_and(|first| first == "user") {
        args.next();
        return parse_user(args).map(Command::User);
    }
    let mut config = None;
    while let Some(arg) = args.next() {
        if arg == "--config" {
            let value = args.next().ok_or("--config needs a file")?;
            if config.replace(PathBuf::from(value)).is_some() {
                return Err("--config given twice".into());
            }
        } else {
            return Err(format!("unknown argument {}", arg.to_string_lossy()));
        }
    }
    config
        .map(|config| Command::Serve { config })
        .ok_or_else(|| "--config is required".into())
}

/// The flags of `registration-code`: any order, each at most once, `--flag value` only.
fn parse_registration_code(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut config = None;
    let mut owner = None;
    let mut name = None;
    let mut agent_key = None;
    let mut ttl = None;
    while let Some(arg) = args.next() {
        let flag = arg.to_str().unwrap_or_default();
        let slot = match flag {
            "--config" => &mut config,
            "--owner" => &mut owner,
            "--name" => &mut name,
            "--agent-key" => &mut agent_key,
            "--ttl-minutes" => &mut ttl,
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        };
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        if slot.replace(value).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let config = config.ok_or("--config is required")?;
    let text = |value: Option<OsString>, flag: &str| -> Result<Option<String>, String> {
        value
            .map(|v| {
                v.into_string()
                    .map_err(|_| format!("{flag} must be valid UTF-8"))
            })
            .transpose()
    };
    let owner = text(owner, "--owner")?.ok_or("--owner is required")?;
    let name = text(name, "--name")?.ok_or("--name is required")?;
    let agent_key = text(agent_key, "--agent-key")?.ok_or("--agent-key is required")?;
    let ttl = text(ttl, "--ttl-minutes")?;
    let request =
        IssueRequest::new(&owner, &name, &agent_key, ttl.as_deref()).map_err(|e| e.to_string())?;
    Ok(Command::RegistrationCode {
        config: PathBuf::from(config),
        request,
    })
}

/// The flags of `user <subcommand>`: any order, each at most once but `--role`, `--flag value`
/// only. Every check here runs before standard input or the database is touched.
fn parse_user(mut args: impl Iterator<Item = OsString>) -> Result<UserCommand, String> {
    let sub = args
        .next()
        .ok_or("user needs a subcommand: add, set-password or disable")?;
    let sub = sub.to_str().unwrap_or_default().to_owned();
    let allowed: &[&str] = match sub.as_str() {
        "add" => &["--config", "--username", "--name", "--role"],
        "set-password" | "disable" => &["--config", "--username"],
        _ => return Err("user needs a subcommand: add, set-password or disable".into()),
    };
    let mut config = None;
    let mut username = None;
    let mut name = None;
    let mut roles: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        let flag = arg.to_str().unwrap_or_default();
        if !allowed.contains(&flag) {
            return Err(format!("unknown argument {}", arg.to_string_lossy()));
        }
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        let text = || {
            value
                .clone()
                .into_string()
                .map_err(|_| format!("{flag} must be valid UTF-8"))
        };
        let slot = match flag {
            "--config" => &mut config,
            "--username" => &mut username,
            "--name" => &mut name,
            _ => {
                roles.push(text()?);
                continue;
            }
        };
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let config = PathBuf::from(config.ok_or("--config is required")?);
    let username = username
        .ok_or("--username is required")?
        .into_string()
        .map_err(|_| "--username must be valid UTF-8".to_owned())?;
    match sub.as_str() {
        "add" => {
            let name = name
                .ok_or("--name is required")?
                .into_string()
                .map_err(|_| "--name must be valid UTF-8".to_owned())?;
            let user = NewUser::new(&username, &name, &roles).map_err(|e| e.to_string())?;
            Ok(UserCommand::Add { config, user })
        }
        _ => {
            if !local_user::is_command_username(&username) {
                return Err(local_user::RequestError::Username.to_string());
            }
            Ok(if sub == "disable" {
                UserCommand::Disable { config, username }
            } else {
                UserCommand::SetPassword { config, username }
            })
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("ngr-server: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match command {
        Command::Serve { config } => match serve(&config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                tracing::error!(%message, "ngr-server failed");
                ExitCode::from(1)
            }
        },
        Command::User(command) => match run_user(command).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(Failed::Usage(message)) => {
                eprintln!("ngr-server: {message}\n{USAGE}");
                ExitCode::from(2)
            }
            Err(Failed::Run(message)) => {
                let _ = writeln!(std::io::stderr(), "ngr-server: {message}");
                ExitCode::from(1)
            }
        },
        Command::RegistrationCode { config, request } => {
            match issue_code(&config, &request).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    let _ = writeln!(std::io::stderr(), "ngr-server: {message}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Issues a code: the configuration, the database, the read-only migration check, the row, then
/// one JSON line on standard output. It never applies a migration and never touches the
/// instruction key file (ADR-465). The code is printed after the commit, so a failed write to
/// standard output leaves a row that expires unused.
async fn issue_code(path: &std::path::Path, request: &IssueRequest) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| e.to_string())?;
    let pool = db::connect(&config.database)
        .await
        .map_err(|e| e.to_string())?;
    let issued = async {
        db::require_current(&pool)
            .await
            .map_err(|e| e.to_string())?;
        registration::issue(&pool, request)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    db::close(&pool).await;
    let issued = issued?;
    let line = serde_json::json!({
        "code": issued.code.as_str(),
        "expires_at": issued.expires_at,
        "server_key_id": issued.server_key_id,
    });
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| format!("cannot write the code to standard output: {}", e.kind()))
}

/// Why a user command ended without success: the command line (exit 2) or the run (exit 1).
enum Failed {
    Usage(String),
    Run(String),
}

impl Failed {
    fn run(error: impl ToString) -> Failed {
        Failed::Run(error.to_string())
    }
}

/// Writes one JSON line to standard output. A failed write is a failure of the command, whose
/// change is committed already.
fn emit(line: &serde_json::Value, what: &str) -> Result<(), Failed> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| {
            Failed::Run(format!(
                "cannot write {what} to standard output: {}",
                e.kind()
            ))
        })
}

/// Reads and checks a password from standard input, refusing a terminal (there is no prompt;
/// a password typed there would be echoed).
fn password_from_stdin() -> Result<String, Failed> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(Failed::Usage(
            "standard input is a terminal; pipe or redirect the password into the command \
             (there is no prompt)"
                .into(),
        ));
    }
    local_user::read_password(stdin.lock()).map_err(Failed::run)
}

/// The user commands: the password (for `add` and `set-password`), its hash, the configuration,
/// the database, the read-only migration check, the change, then one JSON line on standard
/// output. Like `registration-code`, they never apply a migration (ADR-465, ADR-469).
async fn run_user(command: UserCommand) -> Result<(), Failed> {
    let (config, hash) = match &command {
        UserCommand::Add { config, .. } | UserCommand::SetPassword { config, .. } => {
            let password = password_from_stdin()?;
            let hash = local_user::Hasher::new()
                .hash(password)
                .await
                .map_err(Failed::run)?;
            (config, Some(hash))
        }
        UserCommand::Disable { config, .. } => (config, None),
    };
    let config = Config::load(config).map_err(Failed::run)?;
    let pool = db::connect(&config.database).await.map_err(Failed::run)?;
    let line = async {
        db::require_current(&pool).await.map_err(Failed::run)?;
        let hash = hash.as_deref().unwrap_or_default();
        Ok::<_, Failed>(match &command {
            UserCommand::Add { user, .. } => {
                let added = local_user::add(&pool, user, hash)
                    .await
                    .map_err(Failed::run)?;
                serde_json::json!({
                    "operator_id": added.operator_id,
                    "username": user.username(),
                    "roles": user.roles(),
                })
            }
            UserCommand::SetPassword { username, .. } => {
                let operator_id = local_user::set_password(&pool, username, hash)
                    .await
                    .map_err(Failed::run)?;
                serde_json::json!({ "operator_id": operator_id, "username": username })
            }
            UserCommand::Disable { username, .. } => {
                let done = local_user::disable(&pool, username)
                    .await
                    .map_err(Failed::run)?;
                serde_json::json!({
                    "operator_id": done.operator_id,
                    "username": username,
                    "disabled_at": done.disabled_at,
                    "already_disabled": done.already,
                })
            }
        })
    }
    .await;
    db::close(&pool).await;
    emit(&line?, "the result")
}

async fn serve(path: &std::path::Path) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| e.to_string())?;
    let server = Server::start(&config).await.map_err(|e| e.to_string())?;
    // The handlers are in place before anyone is told the server listens, so a stop signal sent
    // on that cue is never lost.
    let (first, second) = stop_signals()?;
    tracing::info!(origin = %server.pinned_origin(), "listening");
    server
        .run(
            async {
                let _ = first.await;
            },
            async {
                let _ = second.await;
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("stopped");
    Ok(())
}

/// Installs the stop-signal handlers (SIGTERM and SIGINT on Unix, Ctrl-C and Ctrl-Break on
/// Windows) and returns two receivers: the first stop signal, which begins the graceful stop, and
/// a second one, which ends the wait for requests in flight at once.
fn stop_signals() -> Result<(oneshot::Receiver<()>, oneshot::Receiver<()>), String> {
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    let mut signals = Signals::install().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        signals.next().await;
        let _ = first_tx.send(());
        signals.next().await;
        let _ = second_tx.send(());
    });
    Ok((first_rx, second_rx))
}

#[cfg(unix)]
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn install() -> std::io::Result<Signals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    async fn next(&mut self) {
        tokio::select! { _ = self.term.recv() => {}, _ = self.int.recv() => {} }
    }
}

#[cfg(windows)]
struct Signals {
    c: tokio::signal::windows::CtrlC,
    b: tokio::signal::windows::CtrlBreak,
}

#[cfg(windows)]
impl Signals {
    fn install() -> std::io::Result<Signals> {
        Ok(Signals {
            c: tokio::signal::windows::ctrl_c()?,
            b: tokio::signal::windows::ctrl_break()?,
        })
    }

    async fn next(&mut self) {
        tokio::select! { _ = self.c.recv() => {}, _ = self.b.recv() => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    fn issue_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "registration-code",
            "--config",
            "c.toml",
            "--owner",
            "alice",
            "--name",
            "Bench PC",
            "--agent-key",
            KEY,
        ];
        args.extend_from_slice(extra);
        args
    }

    #[test]
    fn the_serving_form_is_unchanged() {
        assert_eq!(
            parse(&["--config", "a.toml"]),
            Ok(Command::Serve {
                config: "a.toml".into()
            })
        );
        assert!(parse(&[]).is_err());
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--config", "a", "--config", "b"]).is_err());
        assert!(parse(&["--config", "a", "--owner", "x"]).is_err());
    }

    #[test]
    fn the_subcommand_word_counts_only_in_first_position() {
        assert_eq!(
            parse(&["--config", "registration-code"]),
            Ok(Command::Serve {
                config: "registration-code".into()
            })
        );
        assert!(parse(&["--config", "a.toml", "registration-code"]).is_err());
        assert!(parse(&["registration-code"]).is_err());
    }

    #[test]
    fn the_subcommand_takes_its_flags_in_any_order() {
        let Ok(Command::RegistrationCode { config, request }) = parse(&issue_args(&[])) else {
            panic!("a complete command line must parse");
        };
        assert_eq!(config, PathBuf::from("c.toml"));
        assert_eq!(request.ttl_minutes(), 10);
        let reordered = [
            "registration-code",
            "--ttl-minutes",
            "5",
            "--agent-key",
            KEY,
            "--name",
            "Bench PC",
            "--owner",
            "alice",
            "--config",
            "c.toml",
        ];
        let Ok(Command::RegistrationCode { request, .. }) = parse(&reordered) else {
            panic!("the order of flags is free");
        };
        assert_eq!(request.ttl_minutes(), 5);
    }

    #[test]
    fn the_subcommand_refuses_bad_command_lines() {
        for extra in [
            &["--ttl-minutes", "0"][..],
            &["--ttl-minutes", "61"],
            &["--ttl-minutes", "-1"],
            &["--ttl-minutes", "1.5"],
            &["--ttl-minutes", "1", "--ttl-minutes", "2"],
            &["--owner", "bob"],
            &["--port", "1"],
            &["--ttl-minutes"],
            &["extra"],
        ] {
            assert!(parse(&issue_args(extra)).is_err(), "{extra:?}");
        }
        // A missing flag, one at a time.
        for skip in ["--config", "--owner", "--name", "--agent-key"] {
            let full = issue_args(&[]);
            let at = full.iter().position(|a| *a == skip).unwrap();
            let mut args = full.clone();
            args.drain(at..at + 2);
            assert!(parse(&args).is_err(), "without {skip}");
        }
        // Values: an empty owner or name, a short key, an over-long or control-character name.
        for (flag, bad) in [
            ("--owner", ""),
            ("--name", ""),
            ("--name", "a\nb"),
            ("--agent-key", "abcd"),
        ] {
            let mut args = issue_args(&[]);
            let at = args.iter().position(|a| *a == flag).unwrap();
            args[at + 1] = bad;
            assert!(parse(&args).is_err(), "{flag} {bad:?}");
        }
        let long = "x".repeat(129);
        let mut args = issue_args(&[]);
        let at = args.iter().position(|a| *a == "--name").unwrap();
        args[at + 1] = &long;
        assert!(parse(&args).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_value_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let mut args: Vec<OsString> = issue_args(&[]).into_iter().map(OsString::from).collect();
        let at = args.iter().position(|a| a == "--owner").unwrap();
        args[at + 1] = OsString::from_vec(vec![0xff, 0xfe]);
        assert!(parse_args(args).is_err());
    }

    // ----- `user`

    fn user_args<'a>(sub: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec!["user", sub, "--config", "c.toml", "--username", "alice"];
        if sub == "add" {
            args.extend(["--name", "Alice A"]);
        }
        args.extend_from_slice(extra);
        args
    }

    #[test]
    fn the_user_subcommands_parse_in_any_order() {
        let Ok(Command::User(UserCommand::Add { config, user })) =
            parse(&user_args("add", &["--role", "viewer", "--role", "admin"]))
        else {
            panic!("a complete add must parse");
        };
        assert_eq!(config, PathBuf::from("c.toml"));
        assert_eq!(user.username(), "alice");
        assert_eq!(user.roles(), ["viewer".to_owned(), "admin".to_owned()]);
        // Zero roles are fine and the order of flags is free.
        let Ok(Command::User(UserCommand::Add { user, .. })) = parse(&[
            "user",
            "add",
            "--name",
            "A",
            "--username",
            "a",
            "--config",
            "c",
        ]) else {
            panic!("the order of flags is free");
        };
        assert!(user.roles().is_empty());
        assert_eq!(
            parse(&user_args("set-password", &[])),
            Ok(Command::User(UserCommand::SetPassword {
                config: "c.toml".into(),
                username: "alice".into()
            }))
        );
        assert_eq!(
            parse(&user_args("disable", &[])),
            Ok(Command::User(UserCommand::Disable {
                config: "c.toml".into(),
                username: "alice".into()
            }))
        );
    }

    #[test]
    fn the_user_word_counts_only_in_first_position() {
        assert_eq!(
            parse(&["--config", "user"]),
            Ok(Command::Serve {
                config: "user".into()
            })
        );
        assert!(parse(&["--config", "a.toml", "user"]).is_err());
    }

    #[test]
    fn the_user_subcommands_refuse_bad_command_lines() {
        let cases: Vec<Vec<&str>> = vec![
            vec!["user"],
            vec!["user", "frobnicate", "--config", "c", "--username", "a"],
            user_args("add", &["--surprise", "1"]),
            user_args("add", &["--role"]),
            user_args("add", &["--role", "root"]),
            user_args("add", &["--role", "admin", "--role", "admin"]),
            user_args("add", &["--username", "bob"]),
            user_args("add", &["--name", "B"]),
            user_args("add", &["--config", "d.toml"]),
            // A flag the subcommand does not take.
            user_args("set-password", &["--name", "B"]),
            user_args("disable", &["--role", "admin"]),
            user_args("set-password", &["--username", "bob"]),
            // Names.
            vec![
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "Alice",
                "--name",
                "A",
            ],
            vec![
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "",
                "--name",
                "A",
            ],
            vec![
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "a b",
                "--name",
                "A",
            ],
            vec![
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "a",
                "--name",
                "",
            ],
            vec![
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "a",
                "--name",
                "a\nb",
            ],
            vec!["user", "disable", "--config", "c", "--username", "A"],
            vec!["user", "set-password", "--config", "c"],
            vec!["user", "disable", "--username", "a"],
        ];
        for args in cases {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let long = "x".repeat(129);
        assert!(
            parse(&[
                "user",
                "add",
                "--config",
                "c",
                "--username",
                "a",
                "--name",
                &long
            ])
            .is_err()
        );
        let long = "x".repeat(65);
        assert!(parse(&["user", "disable", "--config", "c", "--username", &long]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_user_value_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        for flag in ["--username", "--name", "--role"] {
            let mut args: Vec<OsString> = user_args("add", &["--role", "viewer"])
                .into_iter()
                .map(OsString::from)
                .collect();
            let at = args.iter().rposition(|a| a == flag).unwrap();
            args[at + 1] = OsString::from_vec(vec![0xff, 0xfe]);
            assert!(parse_args(args).is_err(), "{flag}");
        }
    }
}
