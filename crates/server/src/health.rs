//! `GET /api/v1/health`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

use crate::problem::Problem;

/// How long the database may take to answer `SELECT 1` before the server reports it down.
pub const DATABASE_TIMEOUT: Duration = Duration::from_secs(2);

/// A failed check is logged at most this often: a probe may poll every second, and the log must
/// not fill while the database is down.
const LOG_EVERY: Duration = Duration::from_secs(60);

static LAST_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);

/// True when a failure may be logged at `now`, and then records it.
pub(crate) fn may_log(last: &Mutex<Option<Instant>>, now: Instant, every: Duration) -> bool {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        Some(at) if now.duration_since(at) < every => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

/// 200 `{"status":"ok"}` when the database answers `SELECT 1` within [`DATABASE_TIMEOUT`], else
/// 503 `application/problem+json`. Uncacheable, with nothing else in the body because other OS
/// users can reach it (design 13.3); the cause goes to the log only.
pub async fn health(State(pool): State<PgPool>) -> Response {
    let check = tokio::time::timeout(DATABASE_TIMEOUT, sqlx::query("SELECT 1").execute(&pool));
    match check.await {
        Ok(Ok(_)) => {
            let mut response = Json(serde_json::json!({ "status": "ok" })).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(Err(error)) => {
            if may_log(&LAST_LOGGED, Instant::now(), LOG_EVERY) {
                tracing::warn!(external = %error, "health check: the database query failed");
            }
            unavailable()
        }
        Err(_) => {
            if may_log(&LAST_LOGGED, Instant::now(), LOG_EVERY) {
                tracing::warn!(
                    timeout_secs = DATABASE_TIMEOUT.as_secs(),
                    "health check: the database did not answer in time"
                );
            }
            unavailable()
        }
    }
}

fn unavailable() -> Response {
    Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_logged_once_per_interval() {
        let last = Mutex::new(None);
        let t0 = Instant::now();
        let every = Duration::from_secs(60);
        assert!(may_log(&last, t0, every));
        assert!(!may_log(&last, t0 + Duration::from_secs(59), every));
        assert!(may_log(&last, t0 + Duration::from_secs(61), every));
        assert!(!may_log(&last, t0 + Duration::from_secs(62), every));
    }
}
