//! Local users: the operators of the local deployment profile and their passwords (design 13,
//! 13.3; ADR-469). This module holds the policy (what a user name, a display name, a role and a
//! password may be), the password hashing, the reading of a password from the command line's
//! standard input, and the database operations of `ngr-server user add`, `set-password` and
//! `disable`. The login route and the session table are in [`crate::operator_session`].
//!
//! A local user is a row of `operators` in the one local tenant ([`LOCAL_TENANT_ID`]); its
//! `subject` is the user name. Its password is a row of `operator_credentials` holding an
//! Argon2id PHC string, so the parameters travel with the hash. Errors are fixed text and never
//! repeat a password, a user name or a hash.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use argon2::password_hash::{self, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use shared_crypto::{RandomError, random_32};
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db;

/// The ID of the local tenant, fixed in code: login, `set-password` and `disable` all look a user
/// up by this tenant and the user name. `user add` creates the tenant when it is absent.
pub const LOCAL_TENANT_ID: &str = "4e475200-0000-4000-8000-000000000001";
/// The name `user add` gives the local tenant when it creates it.
pub const LOCAL_TENANT_NAME: &str = "local";

/// The roles `operator_roles.role` accepts. A test pins this list to the migration's CHECK.
pub const ROLES: [&str; 6] = [
    "viewer",
    "worker",
    "reprogrammer",
    "remote",
    "approver",
    "admin",
];

/// The shortest password `user add` and `user set-password` accept, in Unicode scalar values.
pub const MIN_PASSWORD_CHARS: usize = 12;
/// The longest password, in bytes. Login accepts 1 to this many bytes.
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// The longest user name the command accepts, in bytes (the pattern is `[a-z0-9._@-]{1,64}`).
pub const MAX_COMMAND_USERNAME: usize = 64;
/// The longest user name login accepts, in bytes.
pub const MAX_LOGIN_USERNAME: usize = 256;
/// The longest display name, in characters.
pub const MAX_NAME_CHARS: usize = 128;

/// Argon2id memory cost, in KiB (ADR-469).
pub const ARGON_MEMORY_KIB: u32 = 19456;
/// Argon2id passes.
pub const ARGON_PASSES: u32 = 2;
/// Argon2id lanes.
pub const ARGON_LANES: u32 = 1;

fn argon2() -> Argon2<'static> {
    let params = Params::new(ARGON_MEMORY_KIB, ARGON_PASSES, ARGON_LANES, None)
        .expect("the policy's Argon2 parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

// ---------------------------------------------------------------------------------------------
// Policy

/// A command-line value that fails validation. The text names the flag, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("--username must be 1 to 64 characters of a-z, 0-9, '.', '_', '@' and '-'")]
    Username,
    #[error("--name must be 1 to 128 characters without control characters")]
    Name,
    #[error("--role must be one of viewer, worker, reprogrammer, remote, approver, admin")]
    Role,
    #[error("--role given twice with the same role")]
    DuplicateRole,
}

/// True for a user name the command accepts: 1 to 64 bytes of `a-z 0-9 . _ @ -`.
pub fn is_command_username(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_COMMAND_USERNAME
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._@-".contains(&b))
}

/// A validated `user add` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    username: String,
    name: String,
    roles: Vec<String>,
}

impl NewUser {
    /// Validates the command's values. The roles keep the order given.
    pub fn new(username: &str, name: &str, roles: &[String]) -> Result<NewUser, RequestError> {
        if !is_command_username(username) {
            return Err(RequestError::Username);
        }
        if name.is_empty()
            || name.chars().count() > MAX_NAME_CHARS
            || name.chars().any(char::is_control)
        {
            return Err(RequestError::Name);
        }
        for (i, role) in roles.iter().enumerate() {
            if !ROLES.contains(&role.as_str()) {
                return Err(RequestError::Role);
            }
            if roles[..i].contains(role) {
                return Err(RequestError::DuplicateRole);
            }
        }
        Ok(NewUser {
            username: username.to_owned(),
            name: name.to_owned(),
            roles: roles.to_vec(),
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn roles(&self) -> &[String] {
        &self.roles
    }
}

/// Why a password read from standard input was refused. Each is exit 1 with nothing written; the
/// text never shows the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    #[error("the password is empty")]
    Empty,
    #[error("the password is shorter than 12 characters")]
    TooShort,
    #[error("the password is longer than 1024 bytes")]
    TooLong,
    #[error("the password is not valid UTF-8")]
    NotUtf8,
    #[error("the password starts with a byte order mark")]
    ByteOrderMark,
    #[error("the input has more than one line")]
    SecondLine,
    #[error("the password contains a control character")]
    Control,
    #[error("cannot read the password from standard input: {0}")]
    Read(std::io::ErrorKind),
}

/// Reads a password from `input` (the command's standard input) and judges it by the policy.
///
/// The read is bounded: at most [`MAX_PASSWORD_BYTES`] plus a CRLF plus one byte are read, so an
/// endless input ends the read. One trailing LF or CRLF is removed and nothing else is trimmed;
/// an end of input without a newline is fine. A second line, a control character (a lone CR, a
/// tab and DEL included), a leading byte order mark, input that is not UTF-8, an empty password,
/// one under [`MIN_PASSWORD_CHARS`] characters and one over [`MAX_PASSWORD_BYTES`] bytes are
/// refused.
pub fn read_password(input: impl Read) -> Result<String, PasswordError> {
    let limit = (MAX_PASSWORD_BYTES + 3) as u64;
    let mut raw = Vec::new();
    input
        .take(limit)
        .read_to_end(&mut raw)
        .map_err(|e| PasswordError::Read(e.kind()))?;
    check_password(&raw)
}

/// The policy on the bytes read; see [`read_password`].
pub fn check_password(raw: &[u8]) -> Result<String, PasswordError> {
    if raw.len() > MAX_PASSWORD_BYTES + 2 {
        return Err(PasswordError::TooLong);
    }
    let text = std::str::from_utf8(raw).map_err(|_| PasswordError::NotUtf8)?;
    if text.starts_with('\u{feff}') {
        return Err(PasswordError::ByteOrderMark);
    }
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if text.is_empty() {
        return Err(PasswordError::Empty);
    }
    if text.contains('\n') {
        return Err(PasswordError::SecondLine);
    }
    if text.chars().any(char::is_control) {
        return Err(PasswordError::Control);
    }
    if text.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordError::TooLong);
    }
    if text.chars().count() < MIN_PASSWORD_CHARS {
        return Err(PasswordError::TooShort);
    }
    Ok(text.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Hashing

/// What a verification of a password against a stored hash found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Match,
    Mismatch,
    /// The stored text is not a PHC string, or not Argon2id, or its parameters are unusable.
    Malformed,
}

/// Why a password could not be hashed.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("operating-system randomness is unavailable")]
    Random(#[from] RandomError),
    #[error("the password could not be hashed")]
    Hash,
    #[error("the hashing task failed")]
    Task,
}

/// Hashes `password` with Argon2id at the policy's parameters and a new random salt; the result
/// is the PHC string. Takes seconds in a build without optimisation: callers run it on a
/// blocking thread ([`Hasher`]).
pub fn hash_password(password: &str) -> Result<String, HashError> {
    let seed = random_32()?;
    let salt = SaltString::encode_b64(&seed[..16]).map_err(|_| HashError::Hash)?;
    let hash = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| HashError::Hash)?;
    Ok(hash.to_string())
}

/// Checks `password` against the PHC string `stored`. Only Argon2id is accepted; the parameters
/// are the ones in the string.
pub fn verify_password(stored: &str, password: &str) -> Verdict {
    let Ok(hash) = PasswordHash::new(stored) else {
        return Verdict::Malformed;
    };
    if hash.algorithm.as_str() != "argon2id" {
        return Verdict::Malformed;
    }
    match Argon2::default().verify_password(password.as_bytes(), &hash) {
        Ok(()) => Verdict::Match,
        Err(password_hash::Error::Password) => Verdict::Mismatch,
        Err(_) => Verdict::Malformed,
    }
}

/// Counts a task as waiting while it lives, also when its future is dropped.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn enter(count: &'a AtomicUsize) -> Waiting<'a> {
        count.fetch_add(1, Ordering::SeqCst);
        Waiting(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The hashing of one server: one at a time, on a blocking thread, so that a flood of logins
/// cannot hold more than one hash's memory and cannot starve the runtime's threads. The permit
/// moves into the blocking task, so a request that is dropped mid-hash still holds it until the
/// hash ends.
#[derive(Debug, Clone)]
pub struct Hasher {
    permit: Arc<Semaphore>,
    waiting: Arc<AtomicUsize>,
    /// The dummy hash, made once from random bytes on first need (see [`Hasher::verify_dummy`]).
    dummy: Arc<tokio::sync::OnceCell<String>>,
}

impl Default for Hasher {
    fn default() -> Hasher {
        Hasher::new()
    }
}

impl Hasher {
    pub fn new() -> Hasher {
        Hasher {
            permit: Arc::new(Semaphore::new(1)),
            waiting: Arc::new(AtomicUsize::new(0)),
            dummy: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Takes the only permit, so that every hash waits until it is dropped. Tests only: it pins
    /// a login between its row read and its hash.
    #[doc(hidden)]
    pub async fn hold(&self) -> OwnedSemaphorePermit {
        self.permit
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed")
    }

    /// How many hash tasks are waiting for the permit right now. Tests only: a request counts
    /// from before it asks for the permit, so a count above zero shows that it has passed its
    /// admission and its row read.
    #[doc(hidden)]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, HashError> {
        let permit = {
            let _counted = Waiting::enter(&self.waiting);
            self.permit.clone().acquire_owned().await
        }
        .map_err(|_| HashError::Task)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| HashError::Task)
    }

    /// [`hash_password`], waiting for the permit.
    pub async fn hash(&self, password: String) -> Result<String, HashError> {
        self.run(move || hash_password(&password)).await?
    }

    /// [`verify_password`], waiting for the permit.
    pub async fn verify(&self, stored: String, password: String) -> Result<Verdict, HashError> {
        self.run(move || verify_password(&stored, &password)).await
    }

    /// The dummy hash of this hasher: an Argon2id hash under the policy's parameters of a
    /// password nobody holds (32 random bytes that are dropped at once). It is made on the first
    /// call, behind the permit, and kept; concurrent first calls share one computation. A
    /// failing random source fails the call, and the next call tries again.
    #[doc(hidden)]
    pub async fn dummy_hash(&self) -> Result<&str, HashError> {
        self.dummy
            .get_or_try_init(|| async {
                self.run(|| {
                    let secret = URL_SAFE_NO_PAD.encode(random_32()?);
                    hash_password(&secret)
                })
                .await?
            })
            .await
            .map(String::as_str)
    }

    /// A verification of `password` against the dummy hash, its result discarded: what a refusal
    /// that has no real hash to check costs, so that its answer takes about as long as a wrong
    /// password's. Fails only when the dummy hash cannot be made.
    pub async fn verify_dummy(&self, password: String) -> Result<(), HashError> {
        let dummy = self.dummy_hash().await?.to_owned();
        let _ = self.run(move || verify_password(&dummy, &password)).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The command's database operations

/// Why a `user` command did not change the database.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("a user with that name already exists")]
    AlreadyExists,
    #[error("no user has that name")]
    Unknown,
    #[error("the user is disabled")]
    Disabled,
    #[error("cannot write to the database: {0}")]
    Database(#[from] sqlx::Error),
}

/// A user that `add` created.
#[derive(Debug)]
pub struct Added {
    pub operator_id: String,
}

/// The result of `disable`.
#[derive(Debug, PartialEq, Eq)]
pub struct Disabled {
    pub operator_id: String,
    /// The time the user was disabled, UTC, `YYYY-MM-DDTHH:MM:SSZ`: now, or the earlier time
    /// when `already` is true.
    pub disabled_at: String,
    /// True when the user was disabled before this command, which then changed nothing.
    pub already: bool,
}

const UTC_TEXT: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// Creates the user in one transaction: the local tenant when it is absent, then the operator,
/// its roles and its credential. A user name that exists already, including one a concurrent
/// `add` commits first, is [`UserError::AlreadyExists`] and nothing is written. `password_hash`
/// is the PHC string of [`hash_password`], made before the transaction opens.
pub async fn add(pool: &PgPool, user: &NewUser, password_hash: &str) -> Result<Added, UserError> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1::uuid, $2) ON CONFLICT (id) DO NOTHING")
        .bind(LOCAL_TENANT_ID)
        .bind(LOCAL_TENANT_NAME)
        .execute(&mut *tx)
        .await?;
    let inserted: Result<String, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO operators (id, tenant_id, subject, display_name) \
         VALUES (gen_random_uuid(), $1::uuid, $2, $3) RETURNING id::text",
    )
    .bind(LOCAL_TENANT_ID)
    .bind(&user.username)
    .bind(&user.name)
    .fetch_one(&mut *tx)
    .await;
    let operator_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(UserError::AlreadyExists);
        }
        Err(e) => return Err(e.into()),
    };
    sqlx::query(
        "INSERT INTO operator_roles (tenant_id, operator_id, role) \
         SELECT $1::uuid, $2::uuid, unnest($3::text[])",
    )
    .bind(LOCAL_TENANT_ID)
    .bind(&operator_id)
    .bind(&user.roles)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO operator_credentials (operator_id, password_hash) VALUES ($1::uuid, $2)",
    )
    .bind(&operator_id)
    .bind(password_hash)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Added { operator_id })
}

/// Replaces the password of an enabled user, or creates its credential row when there is none,
/// and sets `updated_at` to the database's `now()`. Returns the operator ID. An unknown user and
/// a disabled one are refused. Every session of the user ends at its next request, since the
/// session layer compares the stored hash with the one the login verified (ADR-469).
pub async fn set_password(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
) -> Result<String, UserError> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT id::text, disabled_at IS NOT NULL FROM operators \
         WHERE tenant_id = $1::uuid AND subject = $2 FOR UPDATE",
    )
    .bind(LOCAL_TENANT_ID)
    .bind(username)
    .fetch_optional(&mut *tx)
    .await?;
    let operator_id = match row {
        None => return Err(UserError::Unknown),
        Some((_, true)) => return Err(UserError::Disabled),
        Some((id, false)) => id,
    };
    sqlx::query(
        "INSERT INTO operator_credentials (operator_id, password_hash) VALUES ($1::uuid, $2) \
         ON CONFLICT (operator_id) DO UPDATE \
         SET password_hash = EXCLUDED.password_hash, updated_at = now()",
    )
    .bind(&operator_id)
    .bind(password_hash)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(operator_id)
}

/// Disables the user: sets `disabled_at` and keeps the credential. A user that is disabled
/// already is left as it is and reported with its earlier time. An unknown user is refused.
pub async fn disable(pool: &PgPool, username: &str) -> Result<Disabled, UserError> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let row: Option<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id::text, to_char(disabled_at AT TIME ZONE 'UTC', {UTC_TEXT}) FROM operators \
         WHERE tenant_id = $1::uuid AND subject = $2 FOR UPDATE"
    )))
    .bind(LOCAL_TENANT_ID)
    .bind(username)
    .fetch_optional(&mut *tx)
    .await?;
    let (operator_id, earlier) = row.ok_or(UserError::Unknown)?;
    if let Some(disabled_at) = earlier {
        return Ok(Disabled {
            operator_id,
            disabled_at,
            already: true,
        });
    }
    let disabled_at: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE operators SET disabled_at = now() WHERE id = $1::uuid \
         RETURNING to_char(disabled_at AT TIME ZONE 'UTC', {UTC_TEXT})"
    )))
    .bind(&operator_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Disabled {
        operator_id,
        disabled_at,
        already: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(list: &[&str]) -> Vec<String> {
        list.iter().map(|r| (*r).to_owned()).collect()
    }

    /// A password made at run time: 43 characters of base64url of random bytes.
    fn secret() -> String {
        URL_SAFE_NO_PAD.encode(random_32().unwrap())
    }

    /// `n` characters, one byte each, drawn from a fresh secret.
    fn filler(n: usize) -> String {
        secret().chars().cycle().take(n).collect()
    }

    /// `n` characters of three bytes each, drawn from a fresh secret.
    fn wide(n: usize) -> String {
        secret()
            .bytes()
            .cycle()
            .take(n)
            .map(|b| char::from_u32(0x4e00 + u32::from(b)).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn the_dummy_hash_has_the_policys_parameters() {
        let hasher = Hasher::new();
        let dummy = hasher.dummy_hash().await.unwrap();
        let hash = PasswordHash::new(dummy).expect("the dummy is a PHC string");
        assert_eq!(hash.algorithm.as_str(), "argon2id");
        let params = Params::try_from(&hash).expect("the dummy's parameters");
        assert_eq!(params.m_cost(), ARGON_MEMORY_KIB);
        assert_eq!(params.t_cost(), ARGON_PASSES);
        assert_eq!(params.p_cost(), ARGON_LANES);
        assert_eq!(hash.version, Some(0x13));
        // The same parameters as a fresh hash of the policy.
        let fresh = hash_password(&secret()).unwrap();
        let fresh = PasswordHash::new(&fresh).unwrap();
        assert_eq!(Params::try_from(&fresh).unwrap(), params);
    }

    #[tokio::test]
    async fn each_hasher_makes_its_own_dummy_once() {
        let (a, b) = (Hasher::new(), Hasher::new());
        let first = a.dummy_hash().await.unwrap().to_owned();
        assert_eq!(a.dummy_hash().await.unwrap(), first, "made once");
        assert_eq!(
            a.clone().dummy_hash().await.unwrap(),
            first,
            "shared by clones"
        );
        assert_ne!(b.dummy_hash().await.unwrap(), first);
        // Concurrent first calls share one computation.
        let c = Hasher::new();
        let (x, y) = tokio::join!(c.dummy_hash(), c.dummy_hash());
        assert_eq!(x.unwrap(), y.unwrap());
        assert!(c.verify_dummy(secret()).await.is_ok());
    }

    #[test]
    fn a_hash_verifies_only_its_own_password_and_carries_its_parameters() {
        let password = secret();
        let stored = hash_password(&password).unwrap();
        assert!(
            stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{stored}"
        );
        assert_eq!(verify_password(&stored, &password), Verdict::Match);
        let other = format!("{password}!");
        assert_eq!(verify_password(&stored, &other), Verdict::Mismatch);
        // A new salt each time.
        assert_ne!(stored, hash_password(&password).unwrap());
    }

    #[test]
    fn a_stored_text_that_is_not_argon2id_is_malformed() {
        let real = hash_password(&secret()).unwrap();
        let argon2i = real.replacen("$argon2id$", "$argon2i$", 1);
        let plain = secret();
        for (what, stored) in [
            ("empty", String::new()),
            ("not a hash", "not a hash".to_owned()),
            ("plain text", plain),
            ("bcrypt", "$2b$12$abc".to_owned()),
            ("argon2i", argon2i),
        ] {
            assert_eq!(
                verify_password(&stored, &secret()),
                Verdict::Malformed,
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn the_hasher_runs_one_task_at_a_time() {
        let hasher = Hasher::new();
        let password = secret();
        let permit = hasher.hold().await;
        let waiting = tokio::spawn({
            let (hasher, password) = (hasher.clone(), password.clone());
            async move { hasher.hash(password).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "the hash ran without the permit");
        drop(permit);
        let hash = waiting.await.unwrap().unwrap();
        assert_eq!(verify_password(&hash, &password), Verdict::Match);
    }

    #[test]
    fn user_names_follow_the_pattern() {
        for ok in ["a", "alice", "a.b_c@d-e", "0", &"x".repeat(64)] {
            assert!(is_command_username(ok), "{ok}");
        }
        for bad in [
            "",
            "Alice",
            "a b",
            "a/b",
            "a\n",
            "\u{732b}",
            &"x".repeat(65),
        ] {
            assert!(!is_command_username(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_new_user_is_validated() {
        assert!(NewUser::new("alice", "Alice", &[]).is_ok());
        let all = roles(&ROLES);
        assert_eq!(
            NewUser::new("alice", "A", &all).unwrap().roles(),
            all.as_slice()
        );
        assert_eq!(NewUser::new("Alice", "A", &[]), Err(RequestError::Username));
        for bad in [
            String::new(),
            "x".repeat(129),
            "a\nb".into(),
            "\u{7f}".into(),
        ] {
            assert_eq!(NewUser::new("alice", &bad, &[]), Err(RequestError::Name));
        }
        assert!(NewUser::new("alice", &"\u{732b}".repeat(128), &[]).is_ok());
        assert_eq!(
            NewUser::new("alice", "A", &roles(&["root"])),
            Err(RequestError::Role)
        );
        assert_eq!(
            NewUser::new("alice", "A", &roles(&["Admin"])),
            Err(RequestError::Role)
        );
        assert_eq!(
            NewUser::new("alice", "A", &roles(&["admin", "viewer", "admin"])),
            Err(RequestError::DuplicateRole)
        );
    }

    fn pw(bytes: &[u8]) -> Result<String, PasswordError> {
        read_password(bytes)
    }

    #[test]
    fn one_trailing_newline_is_removed_and_nothing_else_is_trimmed() {
        let base = filler(MIN_PASSWORD_CHARS);
        let want = Ok(base.clone());
        assert_eq!(pw(base.as_bytes()), want);
        assert_eq!(pw(format!("{base}\n").as_bytes()), want);
        assert_eq!(pw(format!("{base}\r\n").as_bytes()), want);
        let padded = format!(" {base} ");
        assert_eq!(pw(padded.as_bytes()), Ok(padded.clone()));
        assert_eq!(pw(format!("{padded}\n").as_bytes()), Ok(padded));
        // Spaces are characters: twelve of them are a password.
        let blanks: String = base.chars().map(|_| ' ').collect();
        assert_eq!(pw(blanks.as_bytes()), Ok(blanks));
    }

    #[test]
    fn the_length_is_counted_in_characters_at_the_least_and_bytes_at_the_most() {
        assert_eq!(pw(b""), Err(PasswordError::Empty));
        assert_eq!(pw(b"\n"), Err(PasswordError::Empty));
        assert_eq!(pw(b"\r\n"), Err(PasswordError::Empty));
        let eleven = filler(MIN_PASSWORD_CHARS - 1);
        assert_eq!(pw(eleven.as_bytes()), Err(PasswordError::TooShort));
        assert_eq!(
            pw(format!("{eleven}\n").as_bytes()),
            Err(PasswordError::TooShort)
        );
        // Twelve characters of three bytes each: 36 bytes pass; eleven do not.
        assert!(pw(wide(MIN_PASSWORD_CHARS).as_bytes()).is_ok());
        assert_eq!(
            pw(wide(MIN_PASSWORD_CHARS - 1).as_bytes()),
            Err(PasswordError::TooShort)
        );
        let at = filler(MAX_PASSWORD_BYTES);
        assert!(pw(at.as_bytes()).is_ok());
        assert!(pw(format!("{at}\n").as_bytes()).is_ok());
        assert!(pw(format!("{at}\r\n").as_bytes()).is_ok());
        let over = filler(MAX_PASSWORD_BYTES + 1);
        assert_eq!(pw(over.as_bytes()), Err(PasswordError::TooLong));
        assert_eq!(
            pw(format!("{over}\n").as_bytes()),
            Err(PasswordError::TooLong)
        );
        // An endless input ends the read.
        let byte = filler(1).as_bytes()[0];
        assert_eq!(
            read_password(std::io::repeat(byte)),
            Err(PasswordError::TooLong)
        );
    }

    #[test]
    fn input_that_is_not_one_clean_line_of_utf8_is_refused() {
        let (a, b) = (filler(6), filler(6));
        let mut broken = a.clone().into_bytes();
        broken.extend_from_slice(&[0xff, 0xfe]);
        broken.extend_from_slice(b.as_bytes());
        assert_eq!(pw(&broken), Err(PasswordError::NotUtf8));
        assert_eq!(
            pw(format!("\u{feff}{a}{b}").as_bytes()),
            Err(PasswordError::ByteOrderMark)
        );
        let line = format!("{a}{b}");
        assert_eq!(
            pw(format!("{line}\n{a}").as_bytes()),
            Err(PasswordError::SecondLine)
        );
        assert_eq!(
            pw(format!("{line}\n\n").as_bytes()),
            Err(PasswordError::SecondLine)
        );
        assert_eq!(
            pw(format!("{line}\r\n\r\n").as_bytes()),
            Err(PasswordError::SecondLine)
        );
        for control in ["\t", "\r", "\u{0}", "\u{7f}", "\u{1b}", "\u{85}"] {
            let text = format!("{a}{control}{b}");
            assert_eq!(
                pw(text.as_bytes()),
                Err(PasswordError::Control),
                "control character {}",
                control.escape_unicode()
            );
        }
        // A trailing lone CR is not a line end.
        assert_eq!(
            pw(format!("{line}\r").as_bytes()),
            Err(PasswordError::Control)
        );
    }

    #[test]
    fn a_password_error_never_shows_the_input() {
        let secret = format!("{}\u{1b}", secret());
        let error = pw(secret.as_bytes()).unwrap_err();
        assert!(!error.to_string().contains(&secret[..8]));
    }
}
