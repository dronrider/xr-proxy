//! Parsing and construction of invite URLs for client onboarding.
//!
//! Two formats are accepted:
//! - HTTPS (primary, what the hub's QR encodes): `https://<host>/invite/<token>`
//! - Custom (deep-link-only fallback): `xr://invite/<token>?hub=<host>`
//!
//! Both carry the same semantic pair `(hub_url, token)`. The custom scheme
//! exists only for hand-built links; QR codes from xr-hub always use HTTPS
//! so that users without the app get a sensible browser landing.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use url::Url;

/// Expected token length in characters: base64url(16 bytes) without padding.
const TOKEN_LEN: usize = 22;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InviteLink {
    /// `https://<hub-host>/invite/<token>`
    Https { hub_url: String, token: String },
    /// `xr://invite/<token>?hub=<hub-host>`
    Custom { hub_url: String, token: String },
}

impl InviteLink {
    pub fn hub_url(&self) -> &str {
        match self {
            InviteLink::Https { hub_url, .. } | InviteLink::Custom { hub_url, .. } => hub_url,
        }
    }

    pub fn token(&self) -> &str {
        match self {
            InviteLink::Https { token, .. } | InviteLink::Custom { token, .. } => token,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteLinkError {
    Parse(String),
    UnsupportedScheme,
    EmptyHost,
    PrivateOrLoopbackHost,
    InvalidPath,
    InvalidToken,
    UnexpectedQuery,
    MissingHubQuery,
}

impl fmt::Display for InviteLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "invalid URL: {e}"),
            Self::UnsupportedScheme => write!(f, "unsupported scheme (expected https or xr)"),
            Self::EmptyHost => write!(f, "host is empty"),
            Self::PrivateOrLoopbackHost => write!(f, "host is a private or loopback IP literal"),
            Self::InvalidPath => write!(f, "path must be /invite/<token>"),
            Self::InvalidToken => write!(f, "token must be 22 base64url characters"),
            Self::UnexpectedQuery => write!(f, "unexpected query parameters"),
            Self::MissingHubQuery => write!(f, "xr:// link is missing the hub query parameter"),
        }
    }
}

impl std::error::Error for InviteLinkError {}

/// Parse a raw string as an invite link.
pub fn parse_invite_link(s: &str) -> Result<InviteLink, InviteLinkError> {
    let url = Url::parse(s.trim()).map_err(|e| InviteLinkError::Parse(e.to_string()))?;

    match url.scheme() {
        "https" => parse_https(&url),
        "xr" => parse_custom(&url),
        _ => Err(InviteLinkError::UnsupportedScheme),
    }
}

fn parse_https(url: &Url) -> Result<InviteLink, InviteLinkError> {
    let host = url.host_str().ok_or(InviteLinkError::EmptyHost)?;
    if host.is_empty() {
        return Err(InviteLinkError::EmptyHost);
    }
    if is_private_or_loopback(host) {
        return Err(InviteLinkError::PrivateOrLoopbackHost);
    }
    if url.query().is_some() {
        return Err(InviteLinkError::UnexpectedQuery);
    }

    let token = extract_token_from_invite_path(url.path())?;

    let mut hub_url = format!("https://{host}");
    if let Some(port) = url.port() {
        hub_url.push(':');
        hub_url.push_str(&port.to_string());
    }

    Ok(InviteLink::Https { hub_url, token })
}

fn parse_custom(url: &Url) -> Result<InviteLink, InviteLinkError> {
    if url.host_str() != Some("invite") {
        return Err(InviteLinkError::InvalidPath);
    }

    let token = url.path().trim_start_matches('/');
    validate_token(token)?;

    let mut hub_host: Option<String> = None;
    for (k, v) in url.query_pairs() {
        if k == "hub" {
            hub_host = Some(v.into_owned());
        } else {
            return Err(InviteLinkError::UnexpectedQuery);
        }
    }
    let hub_host = hub_host.ok_or(InviteLinkError::MissingHubQuery)?;
    if hub_host.is_empty() {
        return Err(InviteLinkError::EmptyHost);
    }
    if is_private_or_loopback(&hub_host) {
        return Err(InviteLinkError::PrivateOrLoopbackHost);
    }

    Ok(InviteLink::Custom {
        hub_url: format!("https://{hub_host}"),
        token: token.to_string(),
    })
}

fn extract_token_from_invite_path(path: &str) -> Result<String, InviteLinkError> {
    let rest = path
        .strip_prefix("/invite/")
        .ok_or(InviteLinkError::InvalidPath)?;
    if rest.contains('/') {
        return Err(InviteLinkError::InvalidPath);
    }
    validate_token(rest)?;
    Ok(rest.to_string())
}

fn validate_token(token: &str) -> Result<(), InviteLinkError> {
    if token.len() != TOKEN_LEN {
        return Err(InviteLinkError::InvalidToken);
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(InviteLinkError::InvalidToken);
    }
    Ok(())
}

fn is_private_or_loopback(host: &str) -> bool {
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Build the canonical HTTPS invite URL from a hub base URL and token.
/// `hub_url` may or may not have a trailing slash; the result never does.
pub fn build_https_url(hub_url: &str, token: &str) -> String {
    let trimmed = hub_url.trim_end_matches('/');
    format!("{trimmed}/invite/{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abcdefghij0123456789AB";

    #[test]
    fn parses_https_url() {
        let link = parse_invite_link(&format!("https://hub.example.com/invite/{TOKEN}")).unwrap();
        assert_eq!(
            link,
            InviteLink::Https {
                hub_url: "https://hub.example.com".into(),
                token: TOKEN.into(),
            }
        );
    }

    #[test]
    fn parses_https_url_with_port() {
        let link = parse_invite_link(&format!("https://hub.example.com:8443/invite/{TOKEN}"))
            .unwrap();
        assert_eq!(link.hub_url(), "https://hub.example.com:8443");
        assert_eq!(link.token(), TOKEN);
    }

    #[test]
    fn parses_custom_url() {
        let link =
            parse_invite_link(&format!("xr://invite/{TOKEN}?hub=hub.example.com")).unwrap();
        assert_eq!(
            link,
            InviteLink::Custom {
                hub_url: "https://hub.example.com".into(),
                token: TOKEN.into(),
            }
        );
    }

    #[test]
    fn trims_whitespace() {
        let link =
            parse_invite_link(&format!("  https://hub.example.com/invite/{TOKEN}  ")).unwrap();
        assert_eq!(link.hub_url(), "https://hub.example.com");
    }

    #[test]
    fn rejects_http_scheme() {
        let err = parse_invite_link(&format!("http://hub.example.com/invite/{TOKEN}")).unwrap_err();
        assert_eq!(err, InviteLinkError::UnsupportedScheme);
    }

    #[test]
    fn rejects_loopback_host() {
        let err = parse_invite_link(&format!("https://127.0.0.1/invite/{TOKEN}")).unwrap_err();
        assert_eq!(err, InviteLinkError::PrivateOrLoopbackHost);
    }

    #[test]
    fn rejects_private_host() {
        for host in ["10.0.0.1", "192.168.1.1", "172.16.0.1"] {
            let err = parse_invite_link(&format!("https://{host}/invite/{TOKEN}")).unwrap_err();
            assert_eq!(err, InviteLinkError::PrivateOrLoopbackHost, "host={host}");
        }
    }

    #[test]
    fn accepts_public_ip_literal() {
        let link = parse_invite_link(&format!("https://1.2.3.4/invite/{TOKEN}")).unwrap();
        assert_eq!(link.hub_url(), "https://1.2.3.4");
    }

    #[test]
    fn rejects_short_token() {
        let err = parse_invite_link("https://hub.example.com/invite/short").unwrap_err();
        assert_eq!(err, InviteLinkError::InvalidToken);
    }

    #[test]
    fn rejects_token_with_bad_chars() {
        let bad = "abcdefghij0123456789A!";
        let err = parse_invite_link(&format!("https://hub.example.com/invite/{bad}")).unwrap_err();
        assert_eq!(err, InviteLinkError::InvalidToken);
    }

    #[test]
    fn rejects_extra_path_segments() {
        let err = parse_invite_link(&format!("https://hub.example.com/invite/{TOKEN}/extra"))
            .unwrap_err();
        assert_eq!(err, InviteLinkError::InvalidPath);
    }

    #[test]
    fn rejects_wrong_https_path() {
        let err =
            parse_invite_link(&format!("https://hub.example.com/api/v1/invite/{TOKEN}")).unwrap_err();
        assert_eq!(err, InviteLinkError::InvalidPath);
    }

    #[test]
    fn rejects_https_with_query() {
        let err = parse_invite_link(&format!("https://hub.example.com/invite/{TOKEN}?x=1"))
            .unwrap_err();
        assert_eq!(err, InviteLinkError::UnexpectedQuery);
    }

    #[test]
    fn rejects_custom_without_hub() {
        let err = parse_invite_link(&format!("xr://invite/{TOKEN}")).unwrap_err();
        assert_eq!(err, InviteLinkError::MissingHubQuery);
    }

    #[test]
    fn rejects_custom_with_extra_query() {
        let err =
            parse_invite_link(&format!("xr://invite/{TOKEN}?hub=x.com&foo=bar")).unwrap_err();
        assert_eq!(err, InviteLinkError::UnexpectedQuery);
    }

    #[test]
    fn rejects_custom_with_wrong_host() {
        let err = parse_invite_link(&format!("xr://welcome/{TOKEN}?hub=x.com")).unwrap_err();
        assert_eq!(err, InviteLinkError::InvalidPath);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_invite_link("not a url").unwrap_err(),
            InviteLinkError::Parse(_)
        ));
    }

    #[test]
    fn build_roundtrips_with_parse() {
        let url = build_https_url("https://hub.example.com", TOKEN);
        assert_eq!(url, format!("https://hub.example.com/invite/{TOKEN}"));
        let parsed = parse_invite_link(&url).unwrap();
        assert_eq!(parsed.hub_url(), "https://hub.example.com");
        assert_eq!(parsed.token(), TOKEN);
    }

    #[test]
    fn build_strips_trailing_slash() {
        assert_eq!(
            build_https_url("https://hub.example.com/", TOKEN),
            format!("https://hub.example.com/invite/{TOKEN}")
        );
    }
}
