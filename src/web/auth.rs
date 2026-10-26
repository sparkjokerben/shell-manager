//! Token authentication for the smw HTTP and WebSocket endpoints.
//!
//! Static assets and `/api/health` are public so a browser can load the page
//! and ask whether a token is required. Everything else needs the token from
//! `Authorization: Bearer ...` or `X-SMW-Token: ...`; WebSocket upgrades may
//! additionally pass it as the `token` query parameter, because the browser
//! WebSocket API cannot set request headers.
//!
//! Without a configured token every request is allowed on the assumption that
//! the server only listens on loopback. Tokens are never logged.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Compare two byte strings without an early exit.
///
/// The loop always runs over the longer input, so the running time depends on
/// the input length but not on the position of the first mismatch. Different
/// lengths still traverse the whole longer input.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = (left.len() ^ right.len()) as u64;
    let longest = left.len().max(right.len());
    for i in 0..longest {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        diff |= (a ^ b) as u64;
    }
    diff == 0
}

/// Whether a request may proceed.
///
/// No configured token means "open"; otherwise the provided token is compared
/// in constant time.
pub fn is_authorized(expected: Option<&str>, provided: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(expected) => match provided {
            Some(provided) => constant_time_eq(expected.as_bytes(), provided.as_bytes()),
            None => false,
        },
    }
}

/// Token from `X-SMW-Token: ...`, else from `Authorization: Bearer ...`.
///
/// The header name lookup is case-insensitive (HTTP header names are), and the
/// scheme is matched case-insensitively as well.
pub fn header_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-smw-token") {
        return value.to_str().ok().map(|value| value.to_string());
    }
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(token.trim().to_string())
    } else {
        None
    }
}

/// Raw value of one query parameter, without decoding.
pub fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    let query = query?;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            return Some(value);
        }
    }
    None
}

/// Token from the WebSocket query string, percent-decoded.
pub fn query_token(query: Option<&str>) -> Option<String> {
    percent_decode(query_param(query, "token")?)
}

/// Decode `%XX` escapes; returns `None` for malformed escapes or non-UTF-8.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Middleware for the JSON API: headers only.
pub async fn require_http(State(state): State<AppState>, request: Request, next: Next) -> Response {
    check(state, request, next, false).await
}

/// Middleware for WebSocket upgrades: headers or `?token=`.
pub async fn require_ws(State(state): State<AppState>, request: Request, next: Next) -> Response {
    check(state, request, next, true).await
}

async fn check(state: AppState, request: Request, next: Next, allow_query: bool) -> Response {
    let provided = header_token(request.headers()).or_else(|| {
        if allow_query {
            query_token(request.uri().query())
        } else {
            None
        }
    });
    let expected = state.token.as_deref().map(String::as_str);
    if is_authorized(expected, provided.as_deref()) {
        next.run(request).await
    } else {
        unauthorized()
    }
}

fn unauthorized() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Content-Type-Options", "nosniff")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(r#"{"error":"unauthorized"}"#))
        .expect("unauthorized response is well formed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::try_from(*name).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn authorization_scheme_is_parsed() {
        let map = headers(&[("authorization", "Bearer abc123")]);
        assert_eq!(header_token(&map).as_deref(), Some("abc123"));

        let lower = headers(&[("authorization", "bearer abc123")]);
        assert_eq!(header_token(&lower).as_deref(), Some("abc123"));

        let custom = headers(&[("x-smw-token", "from-header")]);
        assert_eq!(header_token(&custom).as_deref(), Some("from-header"));
    }

    #[test]
    fn non_bearer_and_missing_headers_are_rejected() {
        assert_eq!(
            header_token(&headers(&[("authorization", "Basic abc")])),
            None
        );
        assert_eq!(header_token(&headers(&[("authorization", "abc")])), None);
        assert_eq!(header_token(&HeaderMap::new()), None);
    }

    #[test]
    fn x_smw_token_wins_over_authorization() {
        let map = headers(&[
            ("x-smw-token", "custom"),
            ("authorization", "Bearer standard"),
        ]);
        assert_eq!(header_token(&map).as_deref(), Some("custom"));
    }

    #[test]
    fn no_configured_token_allows_everything() {
        assert!(is_authorized(None, None));
        assert!(is_authorized(None, Some("anything")));
    }

    #[test]
    fn configured_token_requires_a_matching_value() {
        assert!(is_authorized(Some("secret"), Some("secret")));
        assert!(!is_authorized(Some("secret"), Some("Secret")));
        assert!(!is_authorized(Some("secret"), Some("secret-longer")));
        assert!(!is_authorized(Some("secret"), None));
    }

    #[test]
    fn query_token_is_decoded() {
        assert_eq!(
            query_token(Some("cols=80&token=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(query_token(Some("token=a%2Bb")).as_deref(), Some("a+b"));
        assert_eq!(query_token(Some("token=")).as_deref(), Some(""));
        assert_eq!(query_token(Some("cols=80")), None);
        assert_eq!(query_token(None), None);
        // Malformed escapes do not panic and yield no token.
        assert_eq!(query_token(Some("token=%zz")), None);
        assert_eq!(query_token(Some("token=%4")), None);
    }
}
