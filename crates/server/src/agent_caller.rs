//! The judgment of an agent's session token that the data channel's routes share (ADR-493 item 2,
//! ADR-397): the uploads and the downloads both call [`authenticate`], so one function decides who
//! the caller is.

use axum::http::header::HeaderMap;
use shared_proto::AgentId;
use sqlx::PgPool;

use crate::control::bearer_token;
use crate::db;
use crate::session::Sessions;

/// The agent a request comes from, and its tenant.
pub(crate) struct Caller {
    pub(crate) agent: AgentId,
    pub(crate) tenant: String,
}

/// Why [`authenticate`] did not name a caller.
pub(crate) enum Refusal {
    /// The token is absent, malformed, unknown or expired, or the agent is revoked or has no row
    /// (its tokens are dropped): the routes answer the same 401.
    Unauthorized,
    /// The agent's row could not be read.
    Database(sqlx::Error),
}

/// The agent's tenant, and whether its row is revoked; `None` when there is no row.
async fn agent_standing(pool: &PgPool, agent: &str) -> Result<Option<(String, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT tenant_id::text, revoked_at IS NOT NULL FROM agents WHERE id = $1::uuid",
    )
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row)
}

/// Judges the token, then reads the agent's row (ADR-493 item 2): the answer to a token that is
/// unknown or expired and to a row that is revoked or missing is the same, and the latter drops
/// the agent's tokens. The log line names the agent ID only.
pub(crate) async fn authenticate(
    pool: &PgPool,
    sessions: &Sessions,
    headers: &HeaderMap,
) -> Result<Caller, Refusal> {
    let Some(agent) = bearer_token(headers).and_then(|token| sessions.authenticate(token)) else {
        return Err(Refusal::Unauthorized);
    };
    match agent_standing(pool, &agent.0).await {
        Ok(Some((tenant, false))) => Ok(Caller { agent, tenant }),
        Ok(_) => {
            sessions.revoke_agent(&agent);
            tracing::info!(agent_id = %agent.0, "data channel request refused: the agent is revoked or unknown");
            Err(Refusal::Unauthorized)
        }
        Err(error) => Err(Refusal::Database(error)),
    }
}
