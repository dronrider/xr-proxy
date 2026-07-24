//! xr-setup: идемпотентный установщик xr-proxy (LLD-13, этап 1).
//! Цель `server` поднимает xr-server (и опционально xr-hub) на чистом VPS
//! и заканчивается одноразовым инвайтом; цель `router` появится в XR-177.

mod actions;
mod arch;
mod fetch;
mod hub_api;
mod render;
mod secrets;
mod server_profile;
mod steps;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use fetch::BinSource;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xr-setup", version, about = "Идемпотентная установка xr-proxy на VPS и роутер")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Поднять xr-server на этом VPS, при --with-hub вместе с xr-hub
    Server(ServerArgs),
    /// Настроить OpenWRT-роутер (реализация в XR-177)
    Router(RouterArgs),
}

#[derive(Args)]
struct ServerArgs {
    /// Домен хаба: ссылки инвайтов будут https://<домен>/invite/...
    #[arg(long)]
    hub_domain: Option<String>,
    /// Поставить рядом xr-hub и выдать инвайт по завершении
    #[arg(long, requires = "hub_domain")]
    with_hub: bool,
    /// Ключ обфускации base64 (без флага сгенерируется)
    #[arg(long)]
    key: Option<String>,
    /// Публичный адрес сервера для инвайтов (без флага определяется сам)
    #[arg(long)]
    server_addr: Option<String>,
    /// Порт xr-server
    #[arg(long, default_value_t = 8443)]
    port: u16,
    /// База раздачи бинарей, обычно https://<хаб>/api/v1/setup
    #[arg(long, conflicts_with = "from_dir")]
    dist_url: Option<String>,
    /// Локальная директория с бинарями (и SHA256SUMS, если есть)
    #[arg(long)]
    from_dir: Option<PathBuf>,
    /// Пароль админа хаба (нужен повторному запуску для минта инвайта)
    #[arg(long)]
    admin_pass: Option<String>,
    /// Перезаписать существующие конфиги заново
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct RouterArgs {
    /// Адрес xr-server (addr:port)
    #[arg(long)]
    server: Option<String>,
    /// Ключ обфускации base64
    #[arg(long)]
    key: Option<String>,
    /// Пресет маршрутизации с хаба
    #[arg(long)]
    preset: Option<String>,
    /// База раздачи бинарей (как у server)
    #[arg(long, conflicts_with = "from_dir")]
    dist_url: Option<String>,
    /// Локальная директория с бинарями
    #[arg(long)]
    from_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Server(args) => run_server(args),
        Cmd::Router(_) => bail!("router-профиль ещё не реализован, он едет в XR-177"),
    }
}

fn run_server(args: ServerArgs) -> Result<()> {
    ensure_linux_root()?;

    let source = match (args.from_dir, args.dist_url) {
        (Some(dir), _) => Some(BinSource::Dir(dir)),
        (None, Some(url)) => Some(BinSource::Url(url)),
        (None, None) => None,
    };
    let opts = server_profile::ServerOpts {
        with_hub: args.with_hub,
        hub_domain: args.hub_domain,
        key: args.key,
        server_addr: args.server_addr,
        port: args.port,
        source,
        admin_pass: args.admin_pass,
        force: args.force,
    };

    let resolved = server_profile::resolve(opts)?;
    println!("xr-setup server: приводим VPS к целевому состоянию");
    let report = steps::run(&server_profile::plan(&resolved))?;
    let applied = report
        .iter()
        .filter(|(_, o)| *o == steps::StepOutcome::Applied)
        .count();
    println!(
        "Шагов: {}, применено: {applied}, уже настроено: {}",
        report.len(),
        report.len() - applied
    );
    server_profile::finish(&resolved)
}

fn ensure_linux_root() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("установка работает только на Linux-цели");
    }
    // Установщик пишет в /etc и управляет systemd, без root это не жизнь.
    if unsafe { libc::geteuid() } != 0 {
        bail!("нужен root: запусти через sudo");
    }
    Ok(())
}
