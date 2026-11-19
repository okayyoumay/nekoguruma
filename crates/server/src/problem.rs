//! The RFC 9457 error body (`Problem` in `api/openapi.yaml`). It carries `type`, `title` and
//! `status` and never repeats a value the client sent.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// A problem response, with a `code` when the client has to tell the case from others of the
/// same status.
pub struct Problem {
    status: StatusCode,
    title: &'static str,
    code: Option<&'static str>,
    state: Option<String>,
}

impl Problem {
    pub fn new(status: StatusCode, title: &'static str) -> Problem {
        Problem {
            status,
            title,
            code: None,
            state: None,
        }
    }

    pub fn with_code(status: StatusCode, title: &'static str, code: &'static str) -> Problem {
        Problem {
            status,
            title,
            code: Some(code),
            state: None,
        }
    }
}

impl Problem {
    /// A problem that names the state of the resource the request found (the upload routes' 409).
    pub fn with_state(status: StatusCode, title: &'static str, state: String) -> Problem {
        Problem {
            status,
            title,
            code: None,
            state: Some(state),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({
            "type": "about:blank",
            "title": self.title,
            "status": self.status.as_u16(),
        });
        if let Some(code) = self.code {
            body["code"] = code.into();
        }
        if let Some(state) = self.state {
            body["state"] = state.into();
        }
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
