//! Hands-on demo of the LLD-19 share types and token sign/verify.
//!
//! Library crate has no binary, so this example is how you "touch" step 1:
//!
//! ```sh
//! cargo run -p xr-proto --example share_demo --features share
//! ```
//!
//! (`--features share` is required — the crypto is gated behind it. `serde_json`
//! comes from dev-dependencies, available to examples.)

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::SigningKey;
use xr_proto::share::{
    sign_share_token, verify_share_token, ShareManifest, ShareManifestEntry, ShareRecord,
    ShareToken,
};

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn pretty<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap()
}

fn main() {
    // The hub's signing key (fixed seed here so the demo is reproducible; in
    // production it's loaded from the hub's offline key, LLD-01).
    let hub_key = SigningKey::from_bytes(&[42u8; 32]);
    let hub_pub = hub_key.verifying_key();
    let hub_pub_b64 = base64::engine::general_purpose::STANDARD.encode(hub_pub.as_bytes());

    println!("=== Hub identity ===");
    println!("hub pubkey (base64): {hub_pub_b64}\n");

    // --- What the hub stores: address + metadata only, never bytes. ---
    let record = ShareRecord {
        share_id: "vacation-2026".into(),
        name: "Отпуск 2026".into(),
        owner: "andrew".into(),
        addr: "203.0.113.7".into(),
        port: 8443,
        // The agent's own TOFU-pinned identity key (separate from the hub key).
        agent_pubkey: "QWdlbnRQdWJLZXlQbGFjZWhvbGRlcjEyMzQ1Njc4OTA=".into(),
        created_at: "2026-06-24T12:00:00Z".into(),
        comment: "фотки с поездки".into(),
    };
    println!("=== ShareRecord (hub-stored — index entry, NO file bytes) ===");
    println!("{}\n", pretty(&record));

    println!("=== ShareInfo (consumer view — owner/comment dropped) ===");
    println!("{}\n", pretty(&record.info()));

    // --- What the agent serves: a manifest (listing only). ---
    let manifest = ShareManifest {
        entries: vec![
            ShareManifestEntry {
                path: "photos/beach.jpg".into(),
                size: 2_481_152,
                mtime: 1_750_000_000,
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            },
            ShareManifestEntry {
                path: "notes/readme.txt".into(),
                size: 142,
                mtime: 1_750_100_000,
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
            },
        ],
    };
    println!("=== ShareManifest (agent-served — path/size/mtime/sha256) ===");
    println!("{}\n", pretty(&manifest));

    // --- The capability: hub mints a token, agent verifies it offline. ---
    let now = now_unix();
    let exp = now + 3600; // valid for 1h
    let token = sign_share_token(&hub_key, "vacation-2026", exp);
    println!("=== ShareToken (hub-minted, 1h TTL) ===");
    println!("{}\n", pretty(&token));

    println!("=== Offline verification by the agent (now = {now}) ===");
    show("valid token, right share", &token, &hub_pub, "vacation-2026", now);
    show("wrong share_id requested", &token, &hub_pub, "someone-else", now);
    show("token already expired", &token, &hub_pub, "vacation-2026", exp + 1);

    // Forged by a different key — the agent's pinned hub key rejects it.
    let attacker = SigningKey::from_bytes(&[7u8; 32]);
    let forged = sign_share_token(&attacker, "vacation-2026", exp);
    show("forged by attacker key", &forged, &hub_pub, "vacation-2026", now);

    // Tampered claims (push out exp without re-signing).
    let mut tampered = token.clone();
    tampered.exp = now + 999_999;
    show("tampered exp, not re-signed", &tampered, &hub_pub, "vacation-2026", now);
}

fn show(label: &str, token: &ShareToken, hub_pub: &ed25519_dalek::VerifyingKey, share: &str, now: u64) {
    match verify_share_token(token, hub_pub, share, now) {
        Ok(()) => println!("  [ACCEPT] {label}"),
        Err(e) => println!("  [REJECT] {label} -> {e}"),
    }
}
