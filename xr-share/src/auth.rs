//! Extracting and decoding the access token from a request (LLD-19 §2.2).
//!
//! The consumer presents a [`ShareToken`] (minted by the hub) as a URL-safe
//! base64 blob of its JSON. Accepted, in order:
//!
//! 1. `Authorization: Bearer <blob>`  — primary, used by the app
//! 2. `X-Share-Token: <blob>`         — explicit header alternative
//! 3. `?token=<blob>`                 — best-effort for browsers / curl
//!
//! base64url-no-pad keeps the blob safe in both headers and query strings. The
//! actual signature/expiry/share check is [`xr_proto::share::verify_share_token`];
//! tokens are never logged (§5.6).

use axum::http::{HeaderMap, Uri};
use base64::Engine;
use xr_proto::share::ShareToken;

/// Pull the raw token blob from headers or query, then decode it into a
/// [`ShareToken`]. Returns `None` if absent or malformed.
pub fn extract_token(headers: &HeaderMap, uri: &Uri) -> Option<ShareToken> {
    let blob = bearer(headers)
        .or_else(|| header_value(headers, "x-share-token"))
        .or_else(|| query_token(uri))?;
    decode_token_blob(&blob)
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = header_value(headers, "authorization")?;
    if let Some(blob) = value.strip_prefix("Bearer ") {
        return Some(blob.to_string());
    }
    // Git-контур: libgit2 и штатный git-клиент в HTTP умеют только basic-авторизацию
    // (токен в URL либо credentials callback), поэтому пароль basic-заголовка
    // принимается за тот же токен-блоб. Имя пользователя игнорируется.
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())?;
    decoded
        .split_once(':')
        .map(|(_, password)| password.trim().to_string())
        .filter(|password| !password.is_empty())
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        kv.strip_prefix("token=").map(str::to_string)
    })
}

/// Decode a base64url-no-pad JSON blob into a [`ShareToken`].
pub fn decode_token_blob(blob: &str) -> Option<ShareToken> {
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .ok()?;
    serde_json::from_slice(&json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn blob(t: &ShareToken) -> String {
        let json = serde_json::to_vec(t).unwrap();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    fn sample() -> ShareToken {
        ShareToken { share_id: "s1".into(), scope: "share:read".into(), exp: 123, signature: "AAAA".into() }
    }

    #[test]
    fn extracts_from_bearer() {
        let t = sample();
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {}", blob(&t)).parse().unwrap());
        assert_eq!(extract_token(&h, &Uri::from_static("/manifest")), Some(t));
    }

    #[test]
    fn extracts_from_custom_header() {
        let t = sample();
        let mut h = HeaderMap::new();
        h.insert("x-share-token", blob(&t).parse().unwrap());
        assert_eq!(extract_token(&h, &Uri::from_static("/manifest")), Some(t));
    }

    #[test]
    fn extracts_from_query() {
        let t = sample();
        let uri: Uri = format!("/file/a.txt?token={}", blob(&t)).parse().unwrap();
        assert_eq!(extract_token(&HeaderMap::new(), &uri), Some(t));
    }

    #[test]
    fn extracts_from_basic_password() {
        // Git-контур (LLD-33 п. 2.5): libgit2 идёт basic-авторизацией, токен это
        // пароль, имя пользователя произвольное.
        let t = sample();
        let b = blob(&t);
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("xr-share:{b}"));
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Basic {basic}").parse().unwrap());
        assert_eq!(extract_token(&h, &Uri::from_static("/W/git/head")), Some(t));
    }

    #[test]
    fn basic_without_password_is_ignored() {
        let basic = base64::engine::general_purpose::STANDARD.encode("xr-share:");
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Basic {basic}").parse().unwrap());
        assert_eq!(extract_token(&h, &Uri::from_static("/manifest")), None);
    }

    #[test]
    fn none_when_absent_or_garbage() {
        assert_eq!(extract_token(&HeaderMap::new(), &Uri::from_static("/manifest")), None);
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer not-base64-@@@".parse().unwrap());
        assert_eq!(extract_token(&h, &Uri::from_static("/manifest")), None);
    }
}
