//! The local profile's request guard (design 13.3, ADR-456): Host validation, the redirect from
//! the other loopback name to the pinned origin, and the Origin rule. It is one middleware
//! that wraps the whole route table as a service (`router()`), so it runs before routing for every
//! request and a rejected request never gets a method router's answer.
//!
//! P is the listener's port. A Host or Origin port may be omitted only when P is 80. The rows are
//! checked in order and the first that matches decides:
//!
//! 1. A request target that is not in origin form: 400.
//! 2. No Host header, more than one, an empty or malformed one, or one with userinfo: 400.
//! 3. Host `localhost:P`: 307 to the pinned URL, whatever the method; no handler runs.
//! 4. Any other Host but `127.0.0.1:P`: 421.
//! 5. Host `127.0.0.1:P` and more than one Origin header, or one that is not the pinned origin: 403.
//! 6. Otherwise: the route.
//!
//! An absent Origin passes: the agent, curl and same-origin browser GETs send none, and Origin
//! never authorizes anything (design 5.7.1).

use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::problem::Problem;

/// The pinned origin for port `port`: `http://127.0.0.1:<port>`, without the port when it is 80.
pub fn pinned_origin(port: u16) -> String {
    if port == 80 {
        "http://127.0.0.1".to_owned()
    } else {
        format!("http://127.0.0.1:{port}")
    }
}

/// What the guard does with a request.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Hand the request to the router.
    Pass,
    /// Answer with this status and title instead of routing.
    Reject(StatusCode, &'static str),
    /// Answer 307 with this absolute `Location`.
    Redirect(String),
}

const BAD_TARGET: &str = "Unsupported request target";
const BAD_HOST: &str = "Invalid Host header";
const WRONG_AUTHORITY: &str = "Not served at this authority";
const BAD_ORIGIN: &str = "Origin not allowed";

/// The pure decision for one request (the table in the module documentation).
pub fn decide(port: u16, uri: &Uri, headers: &HeaderMap) -> Decision {
    // Row 1.
    if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with('/') {
        return Decision::Reject(StatusCode::BAD_REQUEST, BAD_TARGET);
    }
    // Row 2.
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return Decision::Reject(StatusCode::BAD_REQUEST, BAD_HOST);
    };
    let Some((name, host_port)) = host.to_str().ok().and_then(parse_host) else {
        return Decision::Reject(StatusCode::BAD_REQUEST, BAD_HOST);
    };
    let same_port = host_port.unwrap_or(80) == port;
    // Row 3.
    if same_port && name.eq_ignore_ascii_case("localhost") {
        let target = uri.path_and_query().map_or("/", |p| p.as_str());
        return Decision::Redirect(format!("{}{target}", pinned_origin(port)));
    }
    // Row 4.
    if !(same_port && name == "127.0.0.1") {
        return Decision::Reject(StatusCode::MISDIRECTED_REQUEST, WRONG_AUTHORITY);
    }
    // Rows 5 and 6.
    let pinned = pinned_origin(port);
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (None, _) => Decision::Pass,
        (Some(origin), None) if origin.as_bytes().eq_ignore_ascii_case(pinned.as_bytes()) => {
            Decision::Pass
        }
        _ => Decision::Reject(StatusCode::FORBIDDEN, BAD_ORIGIN),
    }
}

/// Splits `host[:port]` (RFC 9110 section 7.2) into the host name and the optional port.
/// Userinfo, an empty host, an empty or non-numeric port and characters outside a host name or
/// an IP literal give `None`. Nothing else is normalized.
fn parse_host(value: &str) -> Option<(&str, Option<u16>)> {
    let (name, port) = if value.starts_with('[') {
        let end = value.find(']')?;
        let inner = &value[1..end];
        if inner.is_empty()
            || !inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return None;
        }
        let rest = &value[end + 1..];
        let port = match rest {
            "" => None,
            _ => Some(rest.strip_prefix(':')?),
        };
        (&value[..=end], port)
    } else {
        let (name, port) = match value.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (value, None),
        };
        let valid = |b: u8| b.is_ascii_alphanumeric() || b"-._~%!$&'()*+,;=".contains(&b);
        if name.is_empty() || !name.bytes().all(valid) {
            return None;
        }
        (name, port)
    };
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
            Some(p.parse::<u16>().ok()?)
        }
        Some(_) => return None,
    };
    Some((name, port))
}

/// The middleware: applies [`decide`] with the listener's port.
pub async fn guard(State(port): State<u16>, request: Request, next: Next) -> Response {
    match decide(port, request.uri(), request.headers()) {
        Decision::Pass => next.run(request).await,
        Decision::Reject(status, title) => Problem::new(status, title).into_response(),
        Decision::Redirect(location) => {
            // The location holds only the pinned origin and a request target that the HTTP
            // parser accepted, so it is a valid header value; refuse rather than panic if not.
            match HeaderValue::from_str(&location) {
                Ok(value) => {
                    let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
                    let headers = response.headers_mut();
                    headers.insert(header::LOCATION, value);
                    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    response
                }
                Err(_) => Problem::new(StatusCode::BAD_REQUEST, BAD_TARGET).into_response(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    fn run(port: u16, target: &str, headers: &[(&str, &str)]) -> Decision {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        decide(port, &target.parse::<Uri>().unwrap(), &map)
    }

    fn status(d: &Decision) -> Option<u16> {
        match d {
            Decision::Reject(s, _) => Some(s.as_u16()),
            _ => None,
        }
    }

    #[test]
    fn pinned_origin_omits_port_80_only() {
        assert_eq!(pinned_origin(8080), "http://127.0.0.1:8080");
        assert_eq!(pinned_origin(80), "http://127.0.0.1");
    }

    #[test]
    fn row_1_targets_not_in_origin_form_are_400() {
        for target in ["http://127.0.0.1:8080/api", "127.0.0.1:8080", "*"] {
            let d = run(8080, target, &[("host", "127.0.0.1:8080")]);
            assert_eq!(status(&d), Some(400), "{target}");
        }
    }

    #[test]
    fn row_2_bad_host_is_400() {
        assert_eq!(status(&run(8080, "/", &[])), Some(400));
        let twice = [("host", "127.0.0.1:8080"), ("host", "127.0.0.1:8080")];
        assert_eq!(status(&run(8080, "/", &twice)), Some(400));
        for host in [
            "",
            "user@127.0.0.1:8080",
            "127.0.0.1:",
            "127.0.0.1:80x",
            "127.0.0.1:99999",
            "::1",
            "[::1",
            "[]:8080",
            "a b",
        ] {
            assert_eq!(
                status(&run(8080, "/", &[("host", host)])),
                Some(400),
                "{host}"
            );
        }
    }

    #[test]
    fn row_3_the_alias_is_redirected_to_the_pinned_url() {
        for host in ["localhost:8080", "LocalHost:8080"] {
            let d = run(8080, "/api/v1/health?x=1", &[("host", host)]);
            assert_eq!(
                d,
                Decision::Redirect("http://127.0.0.1:8080/api/v1/health?x=1".into())
            );
        }
        // The alias without a port stands for port 80.
        let d = run(80, "/a", &[("host", "localhost")]);
        assert_eq!(d, Decision::Redirect("http://127.0.0.1/a".into()));
    }

    #[test]
    fn row_3_a_double_slash_path_stays_under_the_pinned_authority() {
        let d = run(8080, "//other.example/x", &[("host", "localhost:8080")]);
        assert_eq!(
            d,
            Decision::Redirect("http://127.0.0.1:8080//other.example/x".into())
        );
    }

    #[test]
    fn row_3_does_not_look_at_origin() {
        let d = run(
            8080,
            "/",
            &[
                ("host", "localhost:8080"),
                ("origin", "http://evil.example"),
            ],
        );
        assert!(matches!(d, Decision::Redirect(_)));
    }

    #[test]
    fn row_4_any_other_authority_is_421() {
        for host in [
            "example.com:8080",
            "127.0.0.1:8081",
            "127.0.0.1",
            "localhost",
            "localhost:8081",
            "[::1]:8080",
            "127.0.0.2:8080",
            "localhost.:8080",
            "192.168.1.5:8080",
        ] {
            assert_eq!(
                status(&run(8080, "/", &[("host", host)])),
                Some(421),
                "{host}"
            );
        }
    }

    #[test]
    fn row_4_the_port_may_be_omitted_only_for_port_80() {
        assert_eq!(status(&run(80, "/", &[("host", "127.0.0.1")])), None);
        assert_eq!(status(&run(80, "/", &[("host", "127.0.0.1:80")])), None);
        assert_eq!(status(&run(8080, "/", &[("host", "127.0.0.1")])), Some(421));
    }

    #[test]
    fn rows_5_and_6_origin() {
        let h = ("host", "127.0.0.1:8080");
        assert_eq!(run(8080, "/", &[h]), Decision::Pass);
        assert_eq!(
            run(8080, "/", &[h, ("origin", "http://127.0.0.1:8080")]),
            Decision::Pass
        );
        for origin in [
            "null",
            "http://localhost:8080",
            "https://127.0.0.1:8080",
            "http://127.0.0.1:8081",
            "http://127.0.0.1",
            "http://evil.example",
            "",
        ] {
            let d = run(8080, "/", &[h, ("origin", origin)]);
            assert_eq!(status(&d), Some(403), "{origin}");
        }
        let two = [
            h,
            ("origin", "http://127.0.0.1:8080"),
            ("origin", "http://127.0.0.1:8080"),
        ];
        assert_eq!(status(&run(8080, "/", &two)), Some(403));
        // With port 80 the pinned origin has no port.
        let h80 = ("host", "127.0.0.1");
        assert_eq!(
            run(80, "/", &[h80, ("origin", "http://127.0.0.1")]),
            Decision::Pass
        );
        assert_eq!(
            status(&run(80, "/", &[h80, ("origin", "http://127.0.0.1:80")])),
            Some(403)
        );
    }

    #[test]
    fn order_a_bad_host_beats_a_bad_origin() {
        let d = run(
            8080,
            "/",
            &[("host", "evil.example"), ("origin", "http://evil.example")],
        );
        assert_eq!(status(&d), Some(421));
        let d = run(8080, "/", &[("origin", "http://evil.example")]);
        assert_eq!(status(&d), Some(400));
    }

    #[test]
    fn an_upgrade_request_with_a_foreign_origin_is_403() {
        let d = run(
            8080,
            "/ws/v1",
            &[
                ("host", "127.0.0.1:8080"),
                ("upgrade", "websocket"),
                ("connection", "Upgrade"),
                ("origin", "http://evil.example"),
            ],
        );
        assert_eq!(status(&d), Some(403));
    }

    #[test]
    fn host_values_parse_as_documented() {
        assert_eq!(
            parse_host("127.0.0.1:8080"),
            Some(("127.0.0.1", Some(8080)))
        );
        assert_eq!(parse_host("example.com"), Some(("example.com", None)));
        assert_eq!(parse_host("[::1]:8080"), Some(("[::1]", Some(8080))));
        assert_eq!(parse_host("[::1]"), Some(("[::1]", None)));
        assert_eq!(parse_host("a@b"), None);
    }
}
