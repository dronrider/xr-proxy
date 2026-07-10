//! Drive the one-way mirror engine against a live `xr-share` agent (LLD-19 §6).
//!
//! ```sh
//! cargo run -p xr-core --example sync_demo -- <agent_url> <dest_dir> <token.json> [agent_pubkey]
//! ```
//!
//! `<token.json>` is the ShareToken JSON as minted by the hub
//! (`POST /admin/shares/:id/token`). The demo fetches the manifest, diffs it
//! against the local destination, prints the plan, applies it, then re-plans to
//! show convergence. `agent_pubkey` (base64) pins the agent identity for the
//! manifest fetch (XR-046); omit it to skip verification.

use std::path::Path;
use std::time::Duration;

use xr_core::sync::{apply_plan, fetch_manifest, plan_sync, scan_local_dir};
use xr_proto::share::ShareToken;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 && args.len() != 5 {
        eprintln!("usage: sync_demo <agent_url> <dest_dir> <token.json> [agent_pubkey]");
        std::process::exit(2);
    }
    let agent_url = &args[1];
    let dest = Path::new(&args[2]);
    let token: ShareToken =
        serde_json::from_str(&std::fs::read_to_string(&args[3]).expect("read token file"))
            .expect("parse ShareToken json");
    let pubkey = args.get(4).map(String::as_str).unwrap_or("");
    let timeout = Duration::from_secs(30);

    std::fs::create_dir_all(dest).unwrap();

    println!("=== fetch manifest from agent ===");
    let manifest = fetch_manifest(agent_url, &token, pubkey, timeout)
        .await
        .expect("fetch manifest");
    println!("  server has {} file(s)", manifest.entries.len());

    let local = scan_local_dir(dest).expect("scan local");
    println!("  local has {} file(s)", local.len());

    let plan = plan_sync(&manifest, &local);
    println!(
        "=== plan: fetch {} ({:?}), delete {} ({:?}) ===",
        plan.fetch.len(),
        plan.fetch.iter().map(|e| &e.path).collect::<Vec<_>>(),
        plan.delete.len(),
        plan.delete,
    );

    if plan.is_empty() {
        println!("  already in sync — nothing to do");
        return;
    }

    let report = apply_plan(agent_url, &token, &plan, dest, timeout).await;
    println!(
        "=== applied: fetched {:?}, deleted {:?}, failed {:?} ===",
        report.fetched, report.deleted, report.failed
    );

    // Re-plan to prove convergence.
    let local2 = scan_local_dir(dest).expect("rescan");
    let plan2 = plan_sync(&manifest, &local2);
    println!(
        "=== re-plan after apply: {} ===",
        if plan2.is_empty() { "EMPTY — converged ✓" } else { "NOT empty ✗" }
    );
}
