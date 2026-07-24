//! xr-setup: идемпотентный установщик xr-proxy (LLD-13, этап 1).
//! Цель `server` поднимает xr-server (и опционально xr-hub) на чистом VPS
//! и заканчивается одноразовым инвайтом; цель `router` приводит OpenWRT
//! к раздающему обход роутеру.

mod actions;
mod arch;
mod fetch;
mod hub_api;
mod openwrt;
mod render;
mod router_profile;
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
    /// Настроить этот OpenWRT-роутер на готовый xr-server
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
    /// Сервер пула addr:port; можно повторять, порядок задаёт приоритет
    /// (первый = primary). Печатает установка server-цели.
    #[arg(long, required = true)]
    server: Vec<String>,
    /// Ключ обфускации base64 (тот же, что на сервере)
    #[arg(long, required = true)]
    key: String,
    /// Salt обфускации, 0xHEX или число (печатает установка server-цели)
    #[arg(long, required = true, value_parser = parse_salt)]
    salt: u32,
    /// URL хаба: пресет маршрутизации и enroll
    #[arg(long)]
    hub_url: Option<String>,
    /// Пресет маршрутизации с хаба
    #[arg(long, default_value = "russia", requires = "hub_url")]
    preset: String,
    /// Одноразовый enrollment-токен реестра хаба (LLD-17)
    #[arg(long, requires = "hub_url")]
    enroll_token: Option<String>,
    /// Имя роутера в реестре (без флага возьмётся hostname)
    #[arg(long, requires = "enroll_token")]
    name: Option<String>,
    /// Имя раздаваемой Wi-Fi-сети; применяется отложенно последним шагом
    #[arg(long)]
    ssid: Option<String>,
    /// Пароль Wi-Fi (psk2); без флага шифрование не трогается
    #[arg(long, requires = "ssid")]
    wifi_pass: Option<String>,
    /// База раздачи бинарей (как у server)
    #[arg(long, conflicts_with = "from_dir")]
    dist_url: Option<String>,
    /// Локальная директория с бинарями
    #[arg(long)]
    from_dir: Option<PathBuf>,
    /// Перезаписать существующий конфиг заново
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Server(args) => run_server(args),
        Cmd::Router(args) => run_router(args),
    }
}

fn run_router(args: RouterArgs) -> Result<()> {
    ensure_linux_root()?;

    let source = match (args.from_dir, args.dist_url) {
        (Some(dir), _) => Some(fetch::BinSource::Dir(dir)),
        (None, Some(url)) => Some(fetch::BinSource::Url(url)),
        (None, None) => None,
    };
    let opts = router_profile::RouterOpts {
        servers: args.server,
        key: args.key,
        salt: args.salt,
        hub_url: args.hub_url,
        preset: args.preset,
        enroll_token: args.enroll_token,
        name: args.name,
        ssid: args.ssid,
        wifi_pass: args.wifi_pass,
        source,
        force: args.force,
    };

    let resolved = router_profile::resolve(opts)?;
    println!("xr-setup router: приводим роутер к целевому состоянию");
    let report = steps::run(&router_profile::plan(&resolved))?;
    print_report(&report);
    router_profile::finish(&resolved)
}

/// Salt в том виде, в каком его печатает установка сервера (0xHEX),
/// десятичная запись тоже принимается.
fn parse_salt(s: &str) -> Result<u32, String> {
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    };
    parsed.map_err(|_| format!("'{s}' не salt: ожидается 0xHEX или число"))
}

#[cfg(test)]
mod tests {
    use super::parse_salt;

    #[test]
    fn salt_accepts_server_output_and_decimal() {
        assert_eq!(parse_salt("0xDEADBEEF"), Ok(0xDEAD_BEEF));
        assert_eq!(parse_salt("0Xdeadbeef"), Ok(0xDEAD_BEEF));
        assert_eq!(parse_salt("12345"), Ok(12345));
        assert!(parse_salt("beef").is_err(), "hex без префикса неоднозначен");
        assert!(parse_salt("0x").is_err());
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
    print_report(&report);
    server_profile::finish(&resolved)
}

fn print_report(report: &[(String, steps::StepOutcome)]) {
    let applied = report
        .iter()
        .filter(|(_, o)| *o == steps::StepOutcome::Applied)
        .count();
    println!(
        "Шагов: {}, применено: {applied}, уже настроено: {}",
        report.len(),
        report.len() - applied
    );
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
