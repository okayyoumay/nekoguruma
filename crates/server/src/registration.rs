//! Agent registration, server side (design 6.4, 6.7, 14.4; ADR-463, ADR-465). This module holds
//! issuance: the administrator's `ngr-server registration-code` command writes one row of
//! `agent_registration_codes` and prints the code once. Redemption comes with the route.
//!
//! A code is [`RegistrationCode`], 128 random bits; only its SHA-256 is stored, so the text
//! exists in the process that issued it and nowhere else. Errors are fixed text, and no value of
//! the request is repeated in them.

use shared_crypto::RandomError;
use shared_crypto::registration::RegistrationCode;
use sqlx::PgPool;

/// The lifetime of a code when the command does not give one, in minutes.
pub const DEFAULT_TTL_MINUTES: u32 = 10;
/// The longest lifetime the command accepts, in minutes. The database does not check it: the
/// command owns this bound.
pub const MAX_TTL_MINUTES: u32 = 60;
/// The longest display name, in characters.
pub const MAX_NAME_CHARS: usize = 128;

/// A request value that fails validation. The text names the flag, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("--owner must not be empty")]
    Owner,
    #[error("--name must be 1 to 128 characters without control characters")]
    Name,
    #[error("--agent-key must be 64 hexadecimal digits")]
    AgentKey,
    #[error("--ttl-minutes must be a whole number from 1 to 60")]
    Ttl,
}

/// A validated issuance request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRequest {
    owner: String,
    name: String,
    agent_key_id: String,
    ttl_minutes: u32,
}

impl IssueRequest {
    /// Validates the command's values. `agent_key` is accepted in either case and kept in lower
    /// case, the form of `agent_registration_codes.agent_key_id`; `ttl_minutes` is the flag's
    /// text, absent for the default.
    pub fn new(
        owner: &str,
        name: &str,
        agent_key: &str,
        ttl_minutes: Option<&str>,
    ) -> Result<IssueRequest, RequestError> {
        if owner.is_empty() {
            return Err(RequestError::Owner);
        }
        if name.is_empty()
            || name.chars().count() > MAX_NAME_CHARS
            || name.chars().any(char::is_control)
        {
            return Err(RequestError::Name);
        }
        if agent_key.len() != 64 || !agent_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RequestError::AgentKey);
        }
        let ttl_minutes = match ttl_minutes {
            None => DEFAULT_TTL_MINUTES,
            // Digits only: no sign, no space, no exponent.
            Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => text
                .parse::<u32>()
                .ok()
                .filter(|m| (1..=MAX_TTL_MINUTES).contains(m))
                .ok_or(RequestError::Ttl)?,
            Some(_) => return Err(RequestError::Ttl),
        };
        Ok(IssueRequest {
            owner: owner.to_owned(),
            name: name.to_owned(),
            agent_key_id: agent_key.to_ascii_lowercase(),
            ttl_minutes,
        })
    }

    pub fn ttl_minutes(&self) -> u32 {
        self.ttl_minutes
    }

    pub fn agent_key_id(&self) -> &str {
        &self.agent_key_id
    }
}

/// Why a code was not issued.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("the database records no active instruction key; start ngr-server once to create it")]
    NoActiveKey,
    #[error("the instruction key was retired while the code was being issued")]
    KeyRetired,
    #[error("no operator has that subject")]
    OwnerUnknown,
    #[error("the operator with that subject is disabled")]
    OwnerDisabled,
    #[error("more than one tenant has an operator with that subject")]
    OwnerAmbiguous,
    #[error("operating-system randomness is unavailable")]
    Random(#[from] RandomError),
    #[error("cannot write the registration code to the database: {0}")]
    Database(#[from] sqlx::Error),
}

/// An issued code, to be shown once.
#[derive(Debug)]
pub struct Issued {
    pub code: RegistrationCode,
    /// The expiry, UTC, `YYYY-MM-DDTHH:MM:SSZ`, from the database's clock.
    pub expires_at: String,
    /// The key ID of the instruction key the code was issued under: what the agent is given to
    /// check the server against (ADR-463 item 11).
    pub server_key_id: String,
}

/// Issues a code in one transaction: the active instruction key, the owner (exactly one enabled
/// operator with the subject), then the row. The code is returned only after the commit, so
/// nothing that fails earlier shows a code that is not stored. The expiry is the database's
/// `now()` plus the lifetime, the same `now()` as `created_at`.
pub async fn issue(pool: &PgPool, request: &IssueRequest) -> Result<Issued, IssueError> {
    let code = RegistrationCode::generate()?;
    let hash = code.sha256();
    let mut tx = pool.begin().await?;

    let server_key_id: String =
        sqlx::query_scalar("SELECT key_id FROM instruction_keys WHERE retired_at IS NULL")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(IssueError::NoActiveKey)?;

    // The subject is unique per tenant, not across tenants: refuse a name that is ambiguous.
    let owners: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, disabled_at IS NOT NULL FROM operators WHERE subject = $1",
    )
    .bind(&request.owner)
    .fetch_all(&mut *tx)
    .await?;
    let (owner_id, tenant_id) = match owners.as_slice() {
        [] => return Err(IssueError::OwnerUnknown),
        [(_, _, true)] => return Err(IssueError::OwnerDisabled),
        [(owner, tenant, false)] => (owner, tenant),
        _ => return Err(IssueError::OwnerAmbiguous),
    };

    // The key is checked again in the statement itself: it may have been retired since the read.
    let expires_at: Option<String> = sqlx::query_scalar(
        "INSERT INTO agent_registration_codes \
             (code_sha256, tenant_id, owner_id, display_name, agent_key_id, instruction_key_id, \
              expires_at) \
         SELECT $1, $2::uuid, $3::uuid, $4, $5, key_id, now() + make_interval(mins => $6) \
         FROM instruction_keys WHERE key_id = $7 AND retired_at IS NULL \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(&hash[..])
    .bind(tenant_id)
    .bind(owner_id)
    .bind(&request.name)
    .bind(&request.agent_key_id)
    .bind(i32::try_from(request.ttl_minutes).expect("at most 60"))
    .bind(&server_key_id)
    .fetch_optional(&mut *tx)
    .await?;
    let expires_at = expires_at.ok_or(IssueError::KeyRetired)?;

    tx.commit().await?;
    Ok(Issued {
        code,
        expires_at,
        server_key_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn request(
        owner: &str,
        name: &str,
        key: &str,
        ttl: Option<&str>,
    ) -> Result<IssueRequest, RequestError> {
        IssueRequest::new(owner, name, key, ttl)
    }

    #[test]
    fn a_complete_request_gets_the_default_lifetime() {
        let r = request("alice", "Workshop PC", KEY, None).unwrap();
        assert_eq!(r.ttl_minutes(), 10);
        assert_eq!(r.agent_key_id(), KEY);
    }

    #[test]
    fn the_lifetime_is_one_to_sixty_whole_minutes() {
        for ok in ["1", "10", "60", "007"] {
            assert!(request("a", "n", KEY, Some(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "0",
            "61",
            "-1",
            "+5",
            "1.5",
            "",
            " 5",
            "5 ",
            "1e1",
            "99999999999",
        ] {
            assert_eq!(
                request("a", "n", KEY, Some(bad)),
                Err(RequestError::Ttl),
                "{bad:?}"
            );
        }
        assert_eq!(
            request("a", "n", KEY, Some("60")).unwrap().ttl_minutes(),
            60
        );
    }

    #[test]
    fn the_agent_key_is_64_hex_digits_and_is_stored_in_lower_case() {
        let upper = KEY.to_ascii_uppercase();
        assert_eq!(request("a", "n", &upper, None).unwrap().agent_key_id(), KEY);
        for bad in [
            "",
            &KEY[1..],
            &format!("{KEY}0"),
            &format!("g{}", &KEY[1..]),
        ] {
            assert_eq!(request("a", "n", bad, None), Err(RequestError::AgentKey));
        }
    }

    #[test]
    fn the_name_is_one_to_128_characters_without_control_characters() {
        assert!(request("a", &"x".repeat(128), KEY, None).is_ok());
        // Characters, not bytes.
        assert!(request("a", &"\u{732b}".repeat(128), KEY, None).is_ok());
        for bad in [
            String::new(),
            "x".repeat(129),
            "a\nb".into(),
            "a\u{0}b".into(),
            "\u{7f}".into(),
        ] {
            assert_eq!(request("a", &bad, KEY, None), Err(RequestError::Name));
        }
    }

    #[test]
    fn the_owner_must_not_be_empty() {
        assert_eq!(request("", "n", KEY, None), Err(RequestError::Owner));
    }

    #[test]
    fn a_request_error_never_shows_the_value() {
        let secret = "zz-not-hex-zz";
        let error = request("a", "n", secret, None).unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}
