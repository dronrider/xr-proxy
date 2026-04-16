//! xr-core — platform-independent VPN engine for xr-proxy.
//!
//! Provides the core logic for mobile/desktop VPN clients:
//! - TUN packet processing via smoltcp (userspace TCP/IP stack)
//! - Fake DNS for domain-based routing
//! - Session management (proxy vs direct)
//! - Integration with xr-proto for obfuscated tunneling

pub mod dns;
pub mod engine;
pub mod ip_stack;
pub mod onboarding;
pub mod presets;
pub mod session;
pub mod state;
pub mod stats;
