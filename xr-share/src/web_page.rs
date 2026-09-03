//! The share's embedded web page (LLD-33 п. 2.8).
//!
//! One HTML file with inline CSS and JS, baked into the agent binary at build
//! time: no CDN, no external requests, nothing to host separately. The page is
//! a shell only, and every byte of share data it shows arrives over this
//! agent's own routes with the caller's token, so a read token renders a read
//! view and a write token unlocks history and editing. Kept dependency-free on
//! purpose: the agent must serve a usable page on an isolated LAN with no
//! internet.

/// The page served at `GET /{share_id}/web`.
pub const SHARE_WEB_HTML: &str = include_str!("web/share.html");
