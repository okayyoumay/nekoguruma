//! `ngr-agent register`: the agent's initial registration (design 6.4, 6.7, 14.4; ADR-463,
//! ADR-470). The binary parses the command line and maps the outcome to an exit status; the flow,
//! the record and the HTTP call are here so that the control channel session library and the
//! `connect` command can use them.
//!
//! The first step that fails decides the outcome, and the registration code is spent last:
//!
//! 1. the agent key, through [`crate::identity::agent_key`] (ADR-460): a key that this call had
//!    to create stops the command, since the operator must issue a code for its fingerprint;
//! 2. the code ([`code_input`]);
//! 3. the exclusive lock `<state>/registration/registration.lock`, tried once and held to the
//!    end;
//! 4. the existing record, looked at with `symlink_metadata` and never parsed (a record that
//!    exists stops the command unless `--replace` was given);
//! 5. the pre-flight: a stale temporary file removed and a new one created, so that a failure
//!    here is known before anything is sent;
//! 6. one POST to `127.0.0.1` ([`crate::server_http`]), whose answer is judged by status and
//!    then by [`answer::check`];
//! 7. the record written to the temporary file, synced, renamed over the final name, and the
//!    directory synced ([`record::PendingRecord`]).
//!
//! The exit statuses are [`Exit`]. A refused or unknown outcome leaves the local record
//! unchanged. A code that was sent may have been spent: where the outcome is unknown the text
//! says to issue a new one if the same code is then refused.
//!
//! Nothing here prints or logs the code, an argument, the bytes read, or text the server sent
//! other than the status number and `Retry-After`.

pub mod answer;
pub mod code_input;
pub mod origin;
pub mod record;

use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::registration::{
    Mode, RegistrationCode, RegistrationTranscript, sign_registration,
};
use worker_host::service::OperatingMode;

use crate::identity::{AgentKey, IdentityError, agent_key};
use crate::journal::{JournalError, WriterLock};
use crate::server_http::{self, PostError, PostRequest, Reply, ReplyBody, Timeouts};
use code_input::CodeSource;
use origin::ServerOrigin;
use record::{DIR_NAME, Durability, LOCK_FILE, PendingRecord, RECORD_FILE, Record};

/// The route's path (`api/openapi.yaml`).
pub const PATH: &str = "/api/v1/agents/register";

/// The text of every outcome that is unknown (H3).
const UNKNOWN: &str = "outcome unknown: if the same code is then refused, issue a new one";
/// The text of a refusal by the server (T2).
const REFUSED: &str = "refused: the code is wrong, expired, used or issued for another key, or \
                       --server-key is not this server's";

/// The exit status of the command (the usage text lists them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 1: cannot register as asked.
    CannotRegister,
    /// 3: unavailable, or the outcome is unknown; the local record is unchanged.
    Unavailable,
    /// 4: the key is present and unusable.
    KeyUnusable,
    /// 5: a record exists and `--replace` was not given.
    AlreadyRegistered,
    /// 6: the answer is not authentic.
    NotAuthentic,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::CannotRegister => 1,
            Exit::Unavailable => 3,
            Exit::KeyUnusable => 4,
            Exit::AlreadyRegistered => 5,
            Exit::NotAuthentic => 6,
        }
    }
}

/// Why the command did not register. The text is fixed wording, never the code, an argument or
/// text from the server.
#[derive(Debug)]
pub struct RegisterError {
    exit: Exit,
    message: String,
}

impl RegisterError {
    pub(crate) fn new(exit: Exit, message: String) -> Self {
        Self { exit, message }
    }

    pub fn exit(&self) -> Exit {
        self.exit
    }

    pub fn exit_code(&self) -> u8 {
        self.exit.code()
    }
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RegisterError {}

fn fail(exit: Exit, message: impl Into<String>) -> RegisterError {
    RegisterError::new(exit, message.into())
}

/// What `register` was asked for, already checked by the command line's parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub mode: OperatingMode,
    pub server: ServerOrigin,
    /// `--server-key`, as bytes.
    pub server_key: [u8; 32],
    pub replace: bool,
}

/// A registration that is stored.
#[derive(Debug)]
pub struct Registered {
    pub record: Record,
    /// Whether an earlier record was replaced.
    pub replaced: bool,
    /// Whether the directory sync after the rename failed (the record is in place).
    pub durability: Durability,
}

impl Registered {
    /// The one JSON line the command prints. It holds no PEM and no code.
    pub fn output(&self) -> serde_json::Value {
        let record = &self.record;
        serde_json::json!({
            "agent_id": record.agent_id,
            "owner_id": record.owner_id,
            "preset": record.preset,
            "allow_remote": record.allow_remote,
            "server_origin": record.server_origin,
            "instruction_key_id": record.instruction_key_id,
            "agent_key_fingerprint": record.agent_key_fingerprint,
            "replaced": self.replaced,
        })
    }
}

fn registration_mode(mode: OperatingMode) -> Mode {
    match mode {
        OperatingMode::Device => Mode::Device,
        OperatingMode::User => Mode::User,
    }
}

/// Registers this agent: the whole flow of the module's documentation, with the key, the state
/// directory and the code taken from where the build and the environment say.
pub fn register(request: &Request, source: &CodeSource) -> Result<Registered, RegisterError> {
    let key = agent_key(request.mode).map_err(identity_error)?;
    if key.created() {
        return Err(fail(
            Exit::CannotRegister,
            format!(
                "the agent key was just created; issue a code for fingerprint {}",
                key.public_key().fingerprint_hex()
            ),
        ));
    }
    let code = code_input::read(source)?;
    let dir = crate::state::private_subdir(request.mode, DIR_NAME)
        .map_err(|error| fail(Exit::Unavailable, error.to_string()))?;
    register_in(request, &key, &code, &dir, Timeouts::default())
}

fn identity_error(error: IdentityError) -> RegisterError {
    let exit = match error {
        IdentityError::Unavailable(_) => Exit::Unavailable,
        IdentityError::KeyUnusable(_) => Exit::KeyUnusable,
        IdentityError::NoStore(_) | IdentityError::NotProvisioned | IdentityError::Random(_) => {
            Exit::CannotRegister
        }
    };
    fail(exit, error.to_string())
}

/// Steps 3 to 7 in the registration directory `dir`, which exists and is private.
pub fn register_in(
    request: &Request,
    key: &AgentKey,
    code: &RegistrationCode,
    dir: &Path,
    timeouts: Timeouts,
) -> Result<Registered, RegisterError> {
    // The lock is held until this function returns, and its file is never deleted.
    let _lock = match WriterLock::at(&dir.join(LOCK_FILE)) {
        Ok(lock) => lock,
        Err(JournalError::InUse) => {
            return Err(fail(Exit::Unavailable, "another registration is running"));
        }
        Err(error) => {
            return Err(fail(
                Exit::Unavailable,
                format!("cannot take the registration lock: {error}"),
            ));
        }
    };
    let replaced = existing_record(dir, request.replace)?;
    let pending = PendingRecord::prepare(dir).map_err(|error| {
        fail(
            Exit::Unavailable,
            format!(
                "cannot prepare the registration record's temporary file: {:?}; nothing was sent",
                error.kind()
            ),
        )
    })?;

    let agent_public = key.public_key();
    let transcript = RegistrationTranscript::new(
        code,
        &agent_public,
        registration_mode(request.mode),
        request.server_key,
    );
    let proof = sign_registration(&*key.signer(), &transcript).map_err(|_| {
        fail(
            Exit::Unavailable,
            "cannot sign the registration proof; nothing was sent",
        )
    })?;
    let body = serde_json::json!({
        "code": code.as_str(),
        "publicKey": agent_public.to_spki_pem(),
        "mode": registration_mode(request.mode).as_str(),
        "proof": STANDARD.encode(proof.to_bytes()),
    })
    .to_string()
    .into_bytes();

    let host = request.server.host_header();
    let post = PostRequest {
        port: request.server.port(),
        host: &host,
        path: PATH,
        body,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| {
            fail(
                Exit::Unavailable,
                "cannot start the runtime; nothing was sent",
            )
        })?;
    let reply = runtime
        .block_on(server_http::post(&post, 201, timeouts))
        .map_err(transport_error)?;

    let verified = judge(&reply, code, &agent_public, request.server_key)?;
    let record = Record {
        agent_id: verified.agent_id,
        owner_id: verified.owner_id,
        preset: verified.preset.as_str().to_owned(),
        allow_remote: verified.allow_remote,
        server_origin: request.server.as_str().to_owned(),
        instruction_key_pem: verified.instruction_key_pem,
        instruction_key_id: verified.instruction_key_id,
        agent_key_fingerprint: agent_public.fingerprint_hex(),
    };
    let durability = pending.commit(&record).map_err(|error| {
        fail(
            Exit::Unavailable,
            format!(
                "registered at the server but not stored ({:?}); a new code is needed",
                error.kind()
            ),
        )
    })?;
    Ok(Registered {
        record,
        replaced,
        durability,
    })
}

/// Step 4: whether a record exists, looked at without parsing it. A regular file stops the
/// command unless `replace`; any other kind of entry is an error whatever `replace` says.
fn existing_record(dir: &Path, replace: bool) -> Result<bool, RegisterError> {
    match std::fs::symlink_metadata(dir.join(RECORD_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(fail(
            Exit::Unavailable,
            format!("cannot look at the registration record: {:?}", error.kind()),
        )),
        Ok(meta) if meta.is_file() => {
            if replace {
                Ok(true)
            } else {
                Err(fail(
                    Exit::AlreadyRegistered,
                    "this agent already has a registration record; give --replace to register \
                     again",
                ))
            }
        }
        Ok(_) => Err(fail(
            Exit::Unavailable,
            "the registration record's place is not a regular file",
        )),
    }
}

fn transport_error(error: PostError) -> RegisterError {
    match error {
        PostError::NotSent(kind) => fail(
            Exit::Unavailable,
            format!("cannot connect to the server ({kind:?}); nothing was sent"),
        ),
        PostError::ConnectTimeout => fail(
            Exit::Unavailable,
            "the connection to the server timed out; nothing was sent",
        ),
        PostError::Unknown => fail(Exit::Unavailable, UNKNOWN),
    }
}

/// Step 6: the status first (T), then, for a 201, the body (V).
fn judge(
    reply: &Reply,
    code: &RegistrationCode,
    agent_key: &shared_crypto::PublicKey,
    server_key_id: [u8; 32],
) -> Result<answer::Verified, RegisterError> {
    let status = reply.status;
    match status {
        201 => {
            let body = match &reply.body {
                Some(ReplyBody::Bytes(bytes)) => bytes,
                Some(ReplyBody::TooLarge) | None => {
                    return Err(fail(Exit::CannotRegister, "not a registration answer"));
                }
            };
            let expected = answer::Expected {
                code,
                agent_key,
                server_key_id,
            };
            answer::check(reply.json_content_type, body, &expected)
                .map_err(|error| fail(error.exit_kind(), error.to_string()))
        }
        403 => Err(fail(Exit::CannotRegister, REFUSED)),
        429 => {
            let wait = reply
                .retry_after
                .as_deref()
                .filter(|text| text.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|text| text.parse::<u32>().ok())
                .filter(|secs| (1..=60).contains(secs));
            Err(fail(
                Exit::CannotRegister,
                match wait {
                    Some(secs) => format!("{REFUSED}; wait {secs} s"),
                    None => REFUSED.to_owned(),
                },
            ))
        }
        408 => Err(fail(Exit::Unavailable, UNKNOWN)),
        421 => Err(fail(
            Exit::CannotRegister,
            format!("unexpected answer (status {status}): --server is not the server's own origin"),
        )),
        300..=399 => Err(fail(Exit::CannotRegister, "redirect, not followed")),
        400..=499 => Err(fail(
            Exit::CannotRegister,
            format!("unexpected answer (status {status})"),
        )),
        500..=599 => Err(fail(Exit::Unavailable, UNKNOWN)),
        _ => Err(fail(
            Exit::CannotRegister,
            format!("unexpected answer (status {status})"),
        )),
    }
}

#[cfg(test)]
mod tests;
