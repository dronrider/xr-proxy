//! Exercise the JNI-facing share functions (`list_shares`, `sync_share`) and
//! print the exact JSON the Android bridge hands Kotlin (LLD-19 step 5).
//!
//! ```sh
//! cargo run -p xr-core --example files_demo -- <hub_url> <agent_url> <dest_dir> <token.json> [agent_pubkey]
//! ```
//!
//! `agent_pubkey` (base64, from the grant) pins the agent identity for the
//! manifest fetch (XR-046); omit it to skip verification.

use std::path::Path;
use std::time::Duration;

use xr_core::sync::{list_shares, sync_share};
use xr_proto::share::ShareToken;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 5 && a.len() != 6 {
        eprintln!("usage: files_demo <hub_url> <agent_url> <dest_dir> <token.json> [agent_pubkey]");
        std::process::exit(2);
    }
    let (hub, agent, dest) = (&a[1], &a[2], Path::new(&a[3]));
    let token: ShareToken =
        serde_json::from_str(&std::fs::read_to_string(&a[4]).unwrap()).unwrap();
    let pubkey = a.get(5).map(String::as_str).unwrap_or("");
    let t = Duration::from_secs(30);
    std::fs::create_dir_all(dest).unwrap();

    println!("=== nativeListShares (hub index) ===");
    match list_shares(hub, t).await {
        Ok(shares) => println!("{}", serde_json::to_string_pretty(&shares).unwrap()),
        Err(e) => println!("error: {e}"),
    }

    println!("\n=== nativeSyncShare dry_run=true (preview, no changes) ===");
    let preview = sync_share(agent, &token, pubkey, dest, true, t).await.unwrap();
    println!("{}", serde_json::to_string_pretty(&preview).unwrap());

    println!("\n=== nativeSyncShare dry_run=false (apply mirror) ===");
    let applied = sync_share(agent, &token, pubkey, dest, false, t).await.unwrap();
    println!("{}", serde_json::to_string_pretty(&applied).unwrap());
}
