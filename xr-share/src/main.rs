//! `xr-share` — the file-sharing agent (LLD-19). Serves one directory read-only
//! over HTTP(S), exposes a signed-hash manifest, and verifies hub-minted access
//! tokens offline. The hub only indexes the address; bytes flow straight from
//! here to the consumer.

mod auth;
mod config;
mod manifest;
mod safepath;
mod server;
mod setup;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        eprintln!("  dir = \"/srv/share\"");
        eprintln!("  share_id = \"<id from hub>\"");
        eprintln!("  hub_pubkey = \"<base64 from GET /api/v1/public-key>\"");
        std::process::exit(2);
    }

    let cfg: AgentConfig = toml::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing config")?;

    let hub_key = cfg.hub_verifying_key()?;

    // Canonicalize the share root up front: fail fast on a bad path and give
    // safepath a stable, symlink-resolved base to compare against.
    let root = Path::new(&cfg.dir)
        .canonicalize()
        .with_context(|| format!("share dir does not exist or is unreadable: {}", cfg.dir))?;
    if !root.is_dir() {
        anyhow::bail!("share dir is not a directory: {}", cfg.dir);
    }

    let bind_addr: SocketAddr = cfg.listen.parse().context("invalid listen address")?;
    let state = Arc::new(AgentState {
        root: root.clone(),
        share_id: cfg.share_id.clone(),
        hub_key,
    });
    let app = server::router(state);

    tracing::info!("serving share '{}' from {}", cfg.share_id, root.display());

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
