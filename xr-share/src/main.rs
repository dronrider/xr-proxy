//! `xr-share` — the file-sharing agent (LLD-19). Serves one directory read-only
//! over HTTP(S), exposes a signed-hash manifest, and verifies hub-minted access
//! tokens offline. The hub only indexes the address; bytes flow straight from
//! here to the consumer.

mod auth;
mod cli;
mod config;
mod manifest;
mod pull;
#[cfg(feature = "relay")]
mod relay;
mod safepath;
mod server;
mod setup;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use base64::Engine;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use config::AgentConfig;
use server::AgentState;
use setup::InitArgs;

#[derive(Parser)]
#[command(name = "xr-share", about = "Read-only file-sharing agent for xr-proxy (LLD-19)")]
struct Cli {
    /// Path to config file. Defaults to /etc/xr-share/config.toml (Linux) or
    /// %PROGRAMDATA%\xr-share\config.toml (Windows).
    #[arg(long, short)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate an ed25519 identity keypair for this agent. Register the printed
    /// public key in the hub as the share's `agent_pubkey` (the consumer pins
    /// it, TOFU); keep the private key safe.
    Keygen,
    /// Interactively create a config (and agent identity) for this machine:
    /// pick a directory, fetch the hub's key, and write the config file.
    Init(InitArgs),
    /// Install the agent (binary already in place) without binding to a folder;
    /// with `--token`, immediately exchange the reg-token for an agent mandate
    /// so you can `share` any number of paths afterwards (§9.3).
    Install(cli::InstallArgs),
    /// Share a path (a directory or a single file) and print a link for it.
    Share(cli::ShareArgs),
    /// List the shares this agent currently serves.
    List,
    /// Stop sharing a path, by its share_id or its path.
    Unshare {
        /// share_id or path to remove.
        target: String,
    },
    /// Receive (desktop): list the shares on an invite, pick files, download them.
    Pull(pull::PullArgs),
    /// Manage OS autostart (systemd on Linux, Scheduled Task on Windows).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install + enable autostart for this agent.
    Install,
    /// Stop and remove the autostart entry.
    Uninstall,
    /// Show the service / task status.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path: PathBuf = cli
        .config
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(setup::default_config_path);

    // Synchronous management subcommands — no async runtime needed.
    match cli.command {
        Some(Commands::Keygen) => {
            keygen();
            return Ok(());
        }
        Some(Commands::Init(args)) => return setup::init(&config_path, args),
        Some(Commands::Install(args)) => return cli::install(&config_path, args),
        Some(Commands::Share(args)) => return cli::share(&config_path, args),
        Some(Commands::List) => return cli::list(&config_path),
        Some(Commands::Unshare { target }) => return cli::unshare(&config_path, &target),
        Some(Commands::Pull(args)) => return pull::pull(args),
        Some(Commands::Service { action }) => {
            return match action {
                ServiceAction::Install => setup::service_install(&config_path),
                ServiceAction::Uninstall => setup::service_uninstall(),
                ServiceAction::Status => setup::service_status(),
            }
        }
        None => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // Tokio runtime built by hand so the sync subcommands above stay simple.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(run(&config_path))
}

async fn run(path: &Path) -> Result<()> {
    if !path.exists() {
        eprintln!("Config file not found: {}", path.display());
        eprintln!();
        eprintln!("Run `xr-share init` to create one, or write it by hand:");
        eprintln!("  listen = \"0.0.0.0:8443\"");
        eprintln!("  hub_pubkey = \"<base64 from GET /api/v1/public-key>\"");
        eprintln!("  [[share]]");
        eprintln!("  share_id = \"<id from hub>\"");
        eprintln!("  path = \"/srv/share\"   # a directory or a single file");
        std::process::exit(2);
    }

    let cfg: AgentConfig = toml::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing config")?;

    let hub_key = cfg.hub_verifying_key()?;
    let bind_addr: SocketAddr = cfg.listen.parse().context("invalid listen address")?;

    // Manifest signing identity (XR-046). Absence is tolerated so a legacy
    // config keeps serving, but consumers that pin agent_pubkey will refuse the
    // unsigned listing, hence the loud warning.
    let identity = cfg.identity_signing_key(path)?;
    match &identity {
        Some(key) => {
            let pub_b64 =
                base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
            tracing::info!("manifest signing enabled (agent identity {pub_b64})");
        }
        None => tracing::warn!(
            "no identity key (config identity_key / identity.key next to it): manifests are \
             served UNSIGNED and pinning consumers will reject them; re-run `xr-share install`"
        ),
    }

    // Resolve the configured shares. An empty set is allowed (the agent runs and
    // waits for `xr-share share <path>` to add one, picked up by hot-reload).
    let shares = server::build_shares(&cfg.resolved_shares());
    if shares.is_empty() {
        tracing::warn!("no shares configured yet — add one with `xr-share share <path>`");
    } else {
        tracing::info!("serving {} share(s)", shares.len());
    }

    let state = Arc::new(AgentState {
        shares: RwLock::new(Arc::new(shares)),
        hub_key,
        hash_cache: manifest::HashCache::new(),
        identity,
    });

    // Hot reload: pick up `share`/`unshare` edits to the config without restart.
    spawn_config_watcher(state.clone(), path.to_path_buf());
    // Keep manifests cheap to serve even for large shares.
    spawn_manifest_warmer(state.clone());
    // Reverse tunnel to the relay for shares behind NAT (LLD-23), only in a build
    // with the `relay` feature and a configured relay + credential + identity.
    spawn_relay_uplink(&cfg, path, state.clone());

    let app = server::router(state);

    match &cfg.tls {
        #[cfg(feature = "tls")]
        Some(tls) => {
            tracing::info!("xr-share listening on {} (TLS)", bind_addr);
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .context("loading TLS cert/key")?;
            axum_server::bind_rustls(bind_addr, rustls)
                .serve(app.into_make_service())
                .await
                .context("running TLS server")?;
        }
        // HTTP-only build: a [tls] block is configured but unsupported here.
        #[cfg(not(feature = "tls"))]
        Some(_) => anyhow::bail!(
            "config has a [tls] block, but this is an HTTP-only build — \
             rebuild with `--features tls`, or run behind a TLS terminator and drop [tls]"
        ),
        None => {
            tracing::info!("xr-share listening on {} (HTTP)", bind_addr);
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .context("binding listener")?;
            axum::serve(listener, app).await.context("running server")?;
        }
    }

    Ok(())
}

/// Bring up the relay reverse tunnel (LLD-23) when the build supports it and the
/// config has a relay, a credential and an identity. Missing pieces are logged,
/// not fatal: the agent still serves its direct listener.
#[cfg(feature = "relay")]
fn spawn_relay_uplink(cfg: &AgentConfig, path: &Path, state: Arc<AgentState>) {
    let Some(relay) = cfg.relay.clone() else { return };
    if !relay::relay_obf_ok(&relay.obf) {
        tracing::warn!("relay configured but obfuscation params are invalid; reverse tunnel disabled");
        return;
    }
    let (Some(cred), Ok(Some(identity))) = (cfg.agent_credential.clone(), cfg.identity_signing_key(path))
    else {
        tracing::warn!(
            "relay configured but no agent_credential/identity; reverse tunnel disabled \
             (re-run `xr-share install --token`)"
        );
        return;
    };
    match relay::spawn(state, relay, cred, identity) {
        Ok(()) => tracing::info!("relay reverse tunnel enabled"),
        Err(e) => tracing::warn!("relay reverse tunnel disabled: {e:#}"),
    }
}

/// Direct-only build: a `[relay]` in the config can't be honoured, say so once.
#[cfg(not(feature = "relay"))]
fn spawn_relay_uplink(cfg: &AgentConfig, _path: &Path, _state: Arc<AgentState>) {
    if cfg.relay.is_some() {
        tracing::warn!(
            "config has a [relay] block, but this build has no relay support \
             (rebuild with `--features relay` to reach shares behind NAT)"
        );
    }
}

/// Poll the config file's mtime and swap in the new share set when it changes,
/// so `xr-share share`/`unshare` take effect without a restart (§9.1).
fn spawn_config_watcher(state: Arc<AgentState>, path: PathBuf) {
    tokio::spawn(async move {
        let mut last = mtime_of(&path);
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let cur = mtime_of(&path);
            if cur == last {
                continue;
            }
            last = cur;
            match reload_share_entries(&path) {
                Ok(entries) => {
                    let map = server::build_shares(&entries);
                    let n = map.len();
                    *state.shares.write().expect("shares lock poisoned") = Arc::new(map);
                    tracing::info!("config changed, {n} share(s) now served");
                }
                Err(e) => tracing::warn!("config reload failed, keeping current shares: {e:#}"),
            }
        }
    });
}

/// Keep the per-file hash cache warm so `/manifest` is fast even for a large
/// share: build every share's manifest on startup and once a minute after.
/// Cheap once warm (only changed files re-hash). Runs off the async executor via
/// `spawn_blocking`, so a slow first pass over a big share never stalls requests.
fn spawn_manifest_warmer(state: Arc<AgentState>) {
    tokio::spawn(async move {
        loop {
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || st.warm_manifests()).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn reload_share_entries(path: &Path) -> Result<Vec<config::ShareEntry>> {
    let cfg: AgentConfig =
        toml::from_str(&std::fs::read_to_string(path).context("reading config")?)
            .context("parsing config")?;
    Ok(cfg.resolved_shares())
}

/// Generate and print an ed25519 identity keypair (base64).
fn keygen() {
    use ed25519_dalek::SigningKey;
    let key = SigningKey::generate(&mut rand::thread_rng());
    let priv_b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
    println!("# xr-share agent identity (ed25519)");
    println!("# Register the PUBLIC key in the hub as the share's agent_pubkey.");
    println!("private_key = \"{priv_b64}\"");
    println!("public_key  = \"{pub_b64}\"");
}
