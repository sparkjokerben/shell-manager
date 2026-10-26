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

/// Headers applied to every response.
///
/// The UI is a same-origin single page with no inline scripts. WebSocket
/// connections need `connect-src`; xterm.js and React set inline styles, hence
/// the style exception.
const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ),
];

/// Apply the common security headers to a response builder.
pub fn with_security_headers(
    mut builder: axum::http::response::Builder,
) -> axum::http::response::Builder {
    for (name, value) in SECURITY_HEADERS {
        builder = builder.header(*name, *value);
    }
    builder
}

/// Whether a request's `Origin` and `Host` headers permit it.
///
/// WebSocket upgrades are not covered by the same-origin policy, so without
/// this check any web page could connect to a tokenless `smw` on loopback and
/// obtain a shell. Scripts (no `Origin`) are allowed because they already run
/// as a local process; browsers always send `Origin` on upgrades and on
/// cross-site requests.
///
/// When the listener is loopback-only and no token is configured, `Host` must
/// also be a loopback name. That blocks DNS rebinding: a public hostname that
/// resolves to 127.0.0.1 still sends its own name in `Host`.
pub fn origin_allowed(headers: &HeaderMap, loopback_only: bool, token_present: bool) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    if let Some(origin) = headers.get(header::ORIGIN) {
        let Some(origin) = origin.to_str().ok() else {
            return false;
        };
        let Ok(uri) = origin.parse::<axum::http::Uri>() else {
            return false;
        };
        let Some(authority) = uri.authority() else {
            return false;
        };
        if !authority.as_str().eq_ignore_ascii_case(host) {
            return false;
        }
    }

    if loopback_only && !token_present {
        return is_loopback_host(host);
    }
    true
}

/// Whether a `Host` header names the loopback interface.
fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((name, _)) => name,
            None => return false,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

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
    if !origin_allowed(
        request.headers(),
        state.loopback_only,
        state.token.is_some(),
    ) {
        return forbidden();
    }
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
    with_security_headers(Response::builder())
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(r#"{"error":"unauthorized"}"#))
        .expect("unauthorized response is well formed")
}

fn forbidden() -> Response {
    with_security_headers(Response::builder())
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(r#"{"error":"forbidden origin or host"}"#))
        .expect("forbidden response is well formed")
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

    #[test]
    fn origin_and_host_checks_block_cross_site_and_rebinding() {
        let local = headers(&[("host", "127.0.0.1:8787")]);
        assert!(origin_allowed(&local, true, false));

        let same_origin = headers(&[
            ("host", "127.0.0.1:8787"),
            ("origin", "http://127.0.0.1:8787"),
        ]);
        assert!(origin_allowed(&same_origin, true, false));

        let cross_site = headers(&[
            ("host", "127.0.0.1:8787"),
            ("origin", "https://evil.example"),
        ]);
        assert!(!origin_allowed(&cross_site, true, false));

        let rebinding = headers(&[
            ("host", "evil.example:8787"),
            ("origin", "http://evil.example:8787"),
        ]);
        assert!(!origin_allowed(&rebinding, true, false));
        // A configured token makes an external Host legitimate (frp, reverse
        // proxy); the token itself still has to match.
        assert!(origin_allowed(&rebinding, true, true));

        let null_origin = headers(&[("host", "127.0.0.1:8787"), ("origin", "null")]);
        assert!(!origin_allowed(&null_origin, true, false));

        let no_host = headers(&[("origin", "http://127.0.0.1:8787")]);
        assert!(!origin_allowed(&no_host, true, false));

        let lan = headers(&[
            ("host", "192.168.1.5:8787"),
            ("origin", "http://192.168.1.5:8787"),
        ]);
        assert!(origin_allowed(&lan, false, false));
    }

    #[test]
    fn loopback_hosts_accept_ipv4_ipv6_and_localhost() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:8787",
            "127.1.2.3:8787",
            "localhost:8787",
            "LOCALHOST",
            "[::1]:8787",
        ] {
            let map = headers(&[("host", host)]);
            assert!(origin_allowed(&map, true, false), "{}", host);
        }
        for host in [
            "0.0.0.0:8787",
            "192.168.1.5:8787",
            "[::]:8787",
            "evil.example",
        ] {
            let map = headers(&[("host", host)]);
            assert!(!origin_allowed(&map, true, false), "{}", host);
        }
    }
}
