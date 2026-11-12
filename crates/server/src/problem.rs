//! The RFC 9457 error body (`Problem` in `api/openapi.yaml`). It carries `type`, `title` and
//! `status` and never repeats a value the client sent.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// A problem response with no `code`.
pub struct Problem {
    status: StatusCode,
    title: &'static str,
}

impl Problem {
    pub fn new(status: StatusCode, title: &'static str) -> Problem {
        Problem { status, title }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": "about:blank",
            "title": self.title,
            "status": self.status.as_u16(),
        });
        let mut response = (self.status, body.to_string()).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}
