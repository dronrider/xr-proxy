mod api;
mod config;
mod embed;
mod password_reset;
mod release;
mod signing;
mod state;
mod storage;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "xr-hub", about = "Control-plane for xr-proxy: presets & invites")]
struct Cli {
    /// Path to config file.
    #[arg(long, short, default_value = "/etc/xr-hub/config.toml")]
    config: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate argon2 password hash for config file.
    HashPassword {
        /// Password to hash.
        password: String,
    },
    /// Reset an admin user's password directly in the config file.
    /// Run on the server over SSH, then restart the service.
    ResetPassword {
        /// Username whose password to reset.
        #[arg(long, default_value = "admin")]
        user: String,
        /// New password. If omitted, you will be prompted (input hidden).
        password: Option<String>,
    },
    /// Generate a fresh ed25519 release keypair for APK self-update (LLD-12).
    /// Keep the private key OFFLINE; the public key goes into the app build.
    GenReleaseKey,
    /// Sign and stage a release APK (LLD-12). Run OFFLINE where the private
    /// release key lives — NOT on the VPS. Writes manifest.json + manifest.sig
    /// (+ a <version>.apk copy) into the output dir for upload to releases/.
    SignRelease {
        /// Path to the built APK.
        #[arg(long)]
        apk: String,
        /// Human version name, e.g. "0.2.0" (also the download path / filename).
        #[arg(long)]
        version: String,
        /// Android versionCode — monotonic; clients offer the update only when
        /// this exceeds the installed code.
        #[arg(long = "version-code")]
        version_code: u64,
        /// Path to the release PRIVATE key file (base64, 32 bytes).
        #[arg(long)]
        key: String,
        /// Hub base URL for the download link, e.g. https://xr-hub.example.com.
        #[arg(long = "base-url")]
        base_url: Option<String>,
        /// Explicit APK URL (overrides the one derived from --base-url).
        #[arg(long = "apk-url")]
        apk_url: Option<String>,
        /// Minimum supported Android SDK.
        #[arg(long = "min-sdk", default_value_t = 29)]
        min_sdk: u32,
        /// Release notes shown in the in-app banner.
        #[arg(long, default_value = "")]
        notes: String,
        /// Release date (default: today, UTC).
        #[arg(long = "released-at")]
        released_at: Option<String>,
        /// Output directory (default: the APK's directory).
        #[arg(long)]
        out: Option<String>,
    },
}

/// `xr-hub reset-password`: захешировать новый пароль и хирургически
/// заменить password_hash пользователя в конфиг-файле (см. password_reset).
fn reset_password(config_path: &str, user: &str, password: Option<&str>) -> Result<()> {
    let path = Path::new(config_path);
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let password = match password {
        Some(p) => p.to_string(),
        None => {
            let first = rpassword::prompt_password(format!("New password for '{user}': "))
                .context("reading password")?;
            let second =
                rpassword::prompt_password("Repeat password: ").context("reading password")?;
            if first != second {
                anyhow::bail!("passwords do not match");
            }
            first
        }
    };
    if password.is_empty() {
        anyhow::bail!("password must not be empty");
    }

    let hash = api::auth::hash_password(&password).map_err(|e| anyhow::anyhow!(e))?;
    let new_content = password_reset::replace_password_hash(&content, user, &hash)
        .map_err(|e| anyhow::anyhow!(e))?;

    // Страховка: правленый файл обязан остаться валидным конфигом.
    toml::from_str::<config::HubConfig>(&new_content)
        .context("edited config no longer parses — config file left untouched")?;

    // Атомарная запись: temp-файл рядом + rename, права как у оригинала.
    let perms = std::fs::metadata(path)?.permissions();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir).context("creating temp file")?;
    std::fs::write(tmp.path(), &new_content).context("writing temp file")?;
    std::fs::set_permissions(tmp.path(), perms)?;
    tmp.persist(path).context("replacing config file")?;

    println!("Password for '{user}' updated in {}.", path.display());
    println!("Apply with: systemctl restart xr-hub");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle subcommands.
    if let Some(Commands::HashPassword { password }) = &cli.command {
        let hash = api::auth::hash_password(password)
            .map_err(|e| anyhow::anyhow!(e))?;
        println!("{hash}");
        return Ok(());
    }

    if let Some(Commands::ResetPassword { user, password }) = &cli.command {
        reset_password(&cli.config, user, password.as_deref())?;
        return Ok(());
    }

    if let Some(Commands::GenReleaseKey) = &cli.command {
        release::gen_release_key();
        return Ok(());
    }

    if let Some(Commands::SignRelease {
        apk,
        version,
        version_code,
        key,
        base_url,
        apk_url,
        min_sdk,
        notes,
        released_at,
        out,
    }) = &cli.command
    {
        let released_at = released_at
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
        release::sign_release(release::SignReleaseArgs {
            apk: apk.clone(),
            version: version.clone(),
            version_code: *version_code,
            key: key.clone(),
            base_url: base_url.clone(),
            apk_url: apk_url.clone(),
            min_sdk: *min_sdk,
            notes: notes.clone(),
            released_at,
            out: out.clone(),
        })?;
        return Ok(());
    }

    // Load config.
    let config_path = Path::new(&cli.config);
    if !config_path.exists() {
        eprintln!("Config file not found: {}", cli.config);
        eprintln!();
        eprintln!("Minimal example:");
        eprintln!();
        eprintln!("[server]");
        eprintln!("bind = \"0.0.0.0:8080\"");
        eprintln!("data_dir = \"/var/lib/xr-hub\"");
        eprintln!();
        eprintln!("[[admin.users]]");
        eprintln!("username = \"admin\"");
        eprintln!("password_hash = \"<run: xr-hub hash-password YOUR_PASSWORD>\"");
        std::process::exit(2);
    }

    let config_str = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let hub_config: config::HubConfig =
        toml::from_str(&config_str).context("parsing config")?;

    // Logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_addr: SocketAddr = hub_config
        .server
        .bind
        .parse()
        .context("invalid bind address")?;
    let tls_config = hub_config.tls.clone();

    // Hydrate state from disk.
    let app_state = state::hydrate(hub_config)?;
    let app = api::router(app_state);

    match tls_config {
        Some(tls) => {
            tracing::info!("starting xr-hub on {} (TLS)", bind_addr);
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &tls.cert,
                &tls.key,
            )
            .await
            .context("loading TLS certificates")?;

            axum_server::bind_rustls(bind_addr, rustls_config)
                .serve(app.into_make_service())
                .await
                .context("running TLS server")?;
        }
        None => {
            tracing::warn!("TLS not configured — starting in plain HTTP mode (dev only!)");
            tracing::info!("starting xr-hub on {} (HTTP)", bind_addr);
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .context("binding listener")?;
            axum::serve(listener, app)
                .await
                .context("running server")?;
        }
    }

    Ok(())
}
