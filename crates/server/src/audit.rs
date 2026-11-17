//! Rows of `audit_log` (design 8.2.3, 4.3, ADR-484).
//!
//! Only job creation writes one so far. The row is copied from the job's own row by the database,
//! inside the creating transaction, so it cannot differ from the job and is never written for a
//! creation that rolls back.

use sqlx::PgConnection;

use crate::jobs::JobError;

/// Writes the `jobCreated` row of a job inserted in this transaction (ADR-484).
///
/// The detail holds the kind, the IR digests, the artifact digest when there is one and the
/// signing key ID. It holds no VIN (the `vin` column does), no request hash, no idempotency key
/// and no VCI ID (design 4.3). A failed insert, or a count other than one row, is an internal
/// error: the creation is not committed without its record.
pub(crate) async fn job_created(
    tx: &mut PgConnection,
    tenant_id: &str,
    job_id: &str,
) -> Result<(), JobError> {
    let written = sqlx::query(
        "INSERT INTO audit_log (tenant_id, operator_id, agent_id, job_id, vin, action, \
             confirmation_level, detail) \
         SELECT tenant_id, operator_id, agent_id, id, vin, 'jobCreated', confirmation_level, \
             jsonb_strip_nulls(jsonb_build_object('kind', kind, 'irDigests', to_jsonb(ir_digests), \
                 'artifactDigest', artifact_digest, 'signingKeyId', signing_key_id)) \
         FROM jobs WHERE tenant_id = $1::uuid AND id = $2::uuid",
    )
    .bind(tenant_id)
    .bind(job_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if written != 1 {
        return Err(JobError::Internal(format!(
            "the audit row of a created job was written {written} times"
        )));
    }
    Ok(())
}
