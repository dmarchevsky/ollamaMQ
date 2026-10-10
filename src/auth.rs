//! Credential helpers shared by the proxy (API keys) and the web UI (basic
//! auth).

use axum::http::HeaderMap;
use base64::Engine;

/// Constant-time string comparison to avoid timing side-channels when checking API keys.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The API key a request presents: `X-Api-Key`, else `Authorization: Bearer`.
pub fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                // Auth-scheme is case-insensitive per RFC 7235; "Bearer " is 7 ASCII bytes.
                .and_then(|v| {
                    if v.len() > 7 && v.get(..7)?.eq_ignore_ascii_case("Bearer ") {
                        Some(v[7..].trim().to_string())
                    } else {
                        None
                    }
                })
        })
}

/// `(user, password)` from an `Authorization: Basic` header.
pub fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let v = headers.get("authorization")?.to_str().ok()?;
    if v.len() <= 6 || !v.get(..6)?.eq_ignore_ascii_case("Basic ") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(v[6..].trim())
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn api_key_prefers_x_api_key_then_bearer() {
        assert_eq!(
            extract_api_key(&headers(&[
                ("x-api-key", "a"),
                ("authorization", "Bearer b")
            ])),
            Some("a".into())
        );
        assert_eq!(
            extract_api_key(&headers(&[("authorization", "bearer  b ")])),
            Some("b".into())
        );
        assert_eq!(
            extract_api_key(&headers(&[("authorization", "Basic Zm9v")])),
            None
        );
        assert_eq!(extract_api_key(&HeaderMap::new()), None);
    }

    #[test]
    fn basic_credentials_decode() {
        // "admin:s3cr:et" — the password may itself contain ':'.
        let h = headers(&[("authorization", "Basic YWRtaW46czNjcjpldA==")]);
        assert_eq!(
            basic_credentials(&h),
            Some(("admin".into(), "s3cr:et".into()))
        );
        assert_eq!(
            basic_credentials(&headers(&[("authorization", "Basic !!")])),
            None
        );
        assert_eq!(
            basic_credentials(&headers(&[("authorization", "Bearer x")])),
            None
        );
    }
}
