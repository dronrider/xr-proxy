pub mod app_update;
pub mod config;
pub mod invite_url;
pub mod mux;
pub mod mux_pool;
pub mod obfuscation;
pub mod preset;
pub mod protocol;
/// Consumer-side relay client (LLD-23). Gated with the `share` feature (and in
/// tests): only file-sharing consumers/agents pull it, never the OpenWRT client.
#[cfg(any(feature = "share", test))]
pub mod relay_client;
pub mod routing;
pub mod server_pool;
pub mod share;
pub mod sni;
pub mod tunnel;
pub mod udp_relay;
pub mod user_rule;
