//! `GET /api/v1/health`.

use axum::Json;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

/// The liveness answer: 200 `{"status":"ok"}`, uncacheable, with nothing else in the body
/// because other OS users can reach it (design 13.3).
///
/// It checks no dependency yet: the server has no database. Once it has one, this handler
/// answers 503 when the database does not respond.
pub async fn health() -> Response {
    let mut response = Json(serde_json::json!({ "status": "ok" })).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
