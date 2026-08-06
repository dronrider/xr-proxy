//! Бинарь `xr-web`: браузерный вход к публикациям агентов (LLD-38 фаза 2).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use xr_web::app::{router, WebState};
use xr_web::config::WebFile;
use xr_web::hub::HttpHub;
use xr_web::listen::{serve, tls_acceptor};
use xr_web::pool::{AgentPool, RelayDialer};

#[derive(Parser)]
#[command(name = "xr-web", about = "XR web: браузерный вход к публикациям агентов")]
struct Cli {
    #[arg(short, long, default_value = "/etc/xr-web/config.toml")]
    config: PathBuf,
    /// Перебить уровень логов из конфига.
    #[arg(short, long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = WebFile::load(&cli.config)?;

    let log_level = cli.log_level.as_deref().unwrap_or(&cfg.web.log_level);
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let hub = Arc::new(HttpHub::new(&cfg.web.hub_url, &cfg.web.shared_secret)?);
    // Проба на старте: секрет, разошедшийся с хабовым, иначе всплыл бы только
    // первым запросом браузера, и то страницей отказа.
    match hub.probe().await {
        Ok(n) => tracing::info!("хаб {} принял секрет, публикаций: {n}", cfg.web.hub_url),
        Err(e) => tracing::warn!(
            "хаб {} не подтвердил доступ ({e}): проверь [web] shared_secret в обоих конфигах",
            cfg.web.hub_url
        ),
    }

    let pool = AgentPool::new(Arc::new(RelayDialer::new()));
    let state = Arc::new(WebState::new(
        cfg.web.domain.clone(),
        hub,
        pool,
        cfg.web.session_ttl_secs,
    ));

    let tls = match &cfg.tls {
        Some(tls) => Some(tls_acceptor(tls)?),
        None => None,
    };
    let listener = TcpListener::bind(&cfg.web.bind).await?;
    tracing::info!(
        "xr-web слушает {} ({}), публикации на *.{}",
        cfg.web.bind,
        if tls.is_some() { "свой TLS" } else { "HTTP за фронтом" },
        cfg.web.domain
    );

    let mut outcome = Ok(());
    tokio::select! {
        r = serve(listener, tls, router(state)) => {
            if let Err(e) = &r {
                tracing::error!("листенер xr-web умер: {e}");
            }
            outcome = r;
        }
        _ = tokio::signal::ctrl_c() => tracing::info!("xr-web останавливается"),
    }
    outcome
}
