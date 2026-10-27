//! Точка входа `xr-macos`. Сервис ставится командой `install` (профиль
//! приложения, конфиг, бинарь и плист launchd), дальше живёт под launchd и
//! управляется командами `status`, `start`, `stop`, `recover`, `uninstall`.
//! Сам демон это `run`: подъём сети, пакетные петли и сторож в одном
//! процессе. Инструкция для владельца машины в docs/MAC-UTUN.md.

mod config;
mod dnsconf;
mod exec;
mod frame;
mod launchd;
mod pf;
mod profile;
mod routes;
mod service;
mod state;
mod utun;
mod vpncheck;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use clap::{Parser, Subcommand};
use exec::{run_capture, run_quiet, Cmd};
use state::{StateDir, StatusFile};
use tokio::runtime::Runtime;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use crate::config::ServiceConfig;
use crate::dnsconf::ServiceDns;
use crate::launchd::ServicePaths;
use crate::service::{packet_loops, Daemon, TickOutcome};
use crate::vpncheck::VpnConflict;

const PROFILE_PATH: &str = "/etc/xr-proxy/profile.json";

#[derive(Parser)]
#[command(
    name = "xr-macos",
    about = "Фоновый VPN-сервис xr-proxy на прямом utun, без Network Extension (XR-309)"
)]
struct Cli {
    #[arg(long, global = true, default_value = launchd::CONFIG_PATH)]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Запуск демона. Обычно его зовёт launchd, а не человек.
    Run,
    /// Установить сервис: профиль, конфиг, бинарь, плист launchd.
    Install {
        /// Путь к JSON-профилю приложения (QR или с хаба).
        #[arg(long)]
        profile: String,
    },
    /// Остановить сервис и убрать его из системы, откатив правки сети.
    Uninstall,
    /// Вернуть сеть в исходное состояние, когда демон не может сам.
    Recover,
    /// Состояние сервиса и сети с вердиктом и причиной отказа.
    Status,
    /// Проверить конфиг и профиль без изменения системы.
    Validate,
    /// Запустить установленный сервис.
    Start,
    /// Остановить сервис с откатом правок сети.
    Stop,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run => cmd_run(&cli.config),
        Command::Install { profile } => cmd_install(&profile, &cli.config),
        Command::Uninstall => cmd_uninstall(),
        Command::Recover => cmd_recover(),
        Command::Status => cmd_status(),
        Command::Validate => cmd_validate(&cli.config),
        Command::Start => cmd_start(),
        Command::Stop => cmd_stop(),
    };
    std::process::exit(code);
}

fn require_root() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("нужны права администратора: запустите команду через sudo".into());
    }
    Ok(())
}

fn load_config(path: &str) -> Result<ServiceConfig, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("предупреждение: конфиг {path} не найден, беру значения по умолчанию");
            String::new()
        }
        Err(e) => return Err(format!("конфиг {path}: {e}")),
    };
    ServiceConfig::load(&text).map_err(|e| e.to_string())
}

fn load_profile(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("профиль {path}: {e}"))
}

fn write_file_mode(path: &str, text: &str, mode: u32) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("запись {path}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("права {path}: {e}"))
}

/// Снимок DNS всех включённых сервисов системы, читается до перевода DNS в
/// туннель. Провал не роняет демон: резолверы нужны только доменам с
/// действием direct, остальной DNS идёт через туннель.
fn system_dns_snapshot() -> Vec<ServiceDns> {
    service::snapshot_dns().unwrap_or_default()
}

fn live_vpn_conflict(own_utun: &str) -> Option<VpnConflict> {
    let nc = run_capture(&vpncheck::nc_list_cmd()).unwrap_or_default();
    let netstat = run_capture(&vpncheck::netstat_cmd()).unwrap_or_default();
    vpncheck::check(&nc, &netstat, own_utun)
}

/// Записать статус отказа: `status` обязан называть причину и тогда, когда
/// демон не поднялся вовсе.
fn refuse(state: &StateDir, reason: &str) {
    let status = StatusFile {
        state: "Refused".into(),
        last_error: reason.to_string(),
        ..Default::default()
    };
    if let Err(e) = state.save_status(&status) {
        eprintln!("статус не записан: {e}");
    }
}

fn cmd_run(config_path: &str) -> i32 {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let state = StateDir::system();
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("{e}");
            refuse(&state, &e);
            return 1;
        }
    };
    let profile_json = match load_profile(&cfg.profile_path) {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("{e}");
            refuse(&state, &e);
            return 1;
        }
    };
    if let Err(e) = profile::validate_profile(&profile_json) {
        tracing::error!("{e}");
        refuse(&state, &e);
        return 1;
    }

    // Чужой VPN: запуск отклоняется до первой правки сети, причина
    // попадает в журнал и в статус.
    if let Some(conflict) = live_vpn_conflict("") {
        tracing::error!("запуск отклонён: {}", conflict.reason);
        refuse(&state, &conflict.reason);
        return 1;
    }

    let snapshot = system_dns_snapshot();
    let resolvers = dnsconf::usable_for_engine(&snapshot);
    if resolvers.is_empty() {
        tracing::warn!(
            "системные резолверы не найдены: домены с действием direct не разрешатся, \
             остальной DNS идёт через туннель"
        );
    }

    let setup = match profile::build_engine_setup(&profile_json, &resolvers) {
        Ok(setup) => setup,
        Err(e) => {
            tracing::error!("{e}");
            refuse(&state, &e);
            return 1;
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut daemon = Daemon::new(
        cfg.clone(),
        setup,
        profile_json,
        resolvers,
        state,
    );

    daemon.resolve_servers();
    daemon.setup_network();
    if !daemon.killswitch_armed() || daemon.tun_handle().is_none() {
        daemon.teardown_network();
        tracing::error!("сеть не поднялась, выхожу; launchd повторит запуск");
        return 1;
    }
    daemon.start_engine();
    daemon.write_status();

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("рантайм: {e}");
            daemon.teardown_network();
            return 1;
        }
    };
    rt.block_on(run_supervisor(
        daemon,
        shutdown_tx,
        shutdown_rx,
        cfg.check_interval_secs,
    ))
}

/// Главный цикл демона: сигналы, пакетные петли и сторож по интервалу.
/// Возврат это код выхода процесса: ноль для штатной остановки, единица
/// для потери туннеля, launchd поднимет демон заново.
async fn run_supervisor(
    mut daemon: Daemon,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    interval_secs: u64,
) -> i32 {
    // Сигналы остановки переводятся в канал: bootout launchd присылает
    // SIGTERM, демону надо успеть откатить правки сети.
    let sig_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = sig_tx.send(true);
    });

    let tun = match daemon.tun_handle() {
        Some(tun) => tun,
        None => {
            daemon.teardown_network();
            return 1;
        }
    };
    let mut loops = tokio::spawn(packet_loops(
        tun,
        daemon.queue_handle(),
        shutdown_rx.clone(),
    ));

    let mut shutdown = shutdown_rx;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let code;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                code = 0;
                break;
            }
            result = &mut loops => {
                code = match result {
                    Ok(Ok(())) => 0,
                    Ok(Err(e)) => {
                        tracing::error!("{e}");
                        1
                    }
                    Err(_) => 1,
                };
                break;
            }
            _ = interval.tick() => {
                if matches!(daemon.tick(), TickOutcome::TunGone) {
                    code = 1;
                    break;
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    if code == 0 {
        tracing::info!("остановка по сигналу, откатываю правки сети");
    } else {
        tracing::error!("туннель потерян, откатываю правки сети и выхожу; launchd поднимет заново");
    }
    daemon.teardown_network();
    code
}

fn cmd_validate(config_path: &str) -> i32 {
    let result = (|| -> Result<(), String> {
        let cfg = load_config(config_path)?;
        let profile = load_profile(&cfg.profile_path)?;
        profile::validate_profile(&profile)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            println!("ok");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cmd_install(profile_path: &str, config_path: &str) -> i32 {
    if let Err(e) = require_root() {
        eprintln!("{e}");
        return 1;
    }
    match install(profile_path, config_path) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("установка не завершена: {e}");
            1
        }
    }
}

fn install(profile_path: &str, config_path: &str) -> Result<(), String> {
    // Всё читается и проверяется до первой правки системы: отказ не
    // оставляет хвостов.
    let profile_json = load_profile(profile_path)?;
    profile::validate_profile(&profile_json)?;
    let cfg_text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("предупреждение: конфиг {config_path} не найден, ставлю значения по умолчанию");
            String::new()
        }
        Err(e) => return Err(format!("конфиг {config_path}: {e}")),
    };
    let cfg = ServiceConfig::load(&cfg_text).map_err(|e| e.to_string())?;
    let endpoints = profile::build_engine_setup(&profile_json, &[])?.endpoints;

    let paths = ServicePaths::default_paths();
    let profile_dir = Path::new(&cfg.profile_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new("/etc/xr-proxy").to_path_buf());
    std::fs::create_dir_all(&profile_dir).map_err(|e| format!("каталог {}: {e}", profile_dir.display()))?;

    // Профиль несёт ключ обфускации: файл закрыт для всех, кроме root, и
    // содержимое не печатается.
    write_file_mode(&cfg.profile_path, &profile_json, 0o600)?;
    write_file_mode(&paths.config, &cfg_text, 0o644)?;

    let exe = std::env::current_exe().map_err(|e| format!("свой бинарь: {e}"))?;
    if exe != Path::new(&paths.binary) {
        std::fs::copy(&exe, &paths.binary)
            .map_err(|e| format!("копирование в {}: {e}", paths.binary))?;
        std::fs::set_permissions(&paths.binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("права {}: {e}", paths.binary))?;
    }
    write_file_mode(launchd::PLIST_PATH, &launchd::render_plist(&paths), 0o644)?;

    // Кэш адресов серверов сеется при живой сети: старту после перезагрузки
    // не нужен резолвер, которого kill-switch ещё не пускает.
    let state = StateDir::system();
    match profile::resolve_endpoints(&endpoints) {
        Ok(ips) if !ips.is_empty() => {
            if let Err(e) = state.save_servers(&ips) {
                eprintln!("предупреждение: кэш серверов не сохранён: {e}");
            }
        }
        _ => {
            eprintln!(
                "предупреждение: адреса серверов не разрешены, демон разрешит их сам при старте"
            );
        }
    }

    if !run_quiet(&launchd::load_cmd()) && !run_quiet(&launchd::load_legacy_cmd()) {
        return Err("плист не загрузился в launchd, смотрите /var/log/xr-macos.log".into());
    }
    run_quiet(&launchd::kickstart_cmd());
    println!("сервис установлен и запущен");
    println!("диагностика: sudo /usr/local/bin/xr-macos status");
    Ok(())
}

fn cmd_uninstall() -> i32 {
    if let Err(e) = require_root() {
        eprintln!("{e}");
        return 1;
    }
    match uninstall() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("удаление не завершено: {e}");
            1
        }
    }
}

fn uninstall() -> Result<(), String> {
    // Остановка шлёт демону SIGTERM, тот сам откатывает правки сети.
    let _ = run_quiet(&launchd::unload_cmd()) || run_quiet(&launchd::unload_legacy_cmd());
    // Страховка на случай мёртвого демона: повторный откат безвреден.
    recover_network(false)?;

    let paths = ServicePaths::default_paths();
    let mut removed = Vec::new();
    for path in [
        launchd::PLIST_PATH,
        &paths.binary,
        &paths.config,
        PROFILE_PATH,
    ] {
        if std::fs::remove_file(path).is_ok() {
            removed.push(path);
        }
    }
    if std::fs::remove_dir_all(state::STATE_DIR).is_ok() {
        removed.push(state::STATE_DIR);
    }
    if removed.is_empty() {
        eprintln!("файлов сервиса не найдено: удалять нечего");
    }
    println!("сервис удалён, сеть возвращена в исходное состояние");
    Ok(())
}

/// Откат правок сети по сохранённому состоянию: снимает свои маршруты,
/// возвращает DNS сервисам, сносит анкор PF и отпускает ссылку включения.
/// Работает и без живого демона, поэтому годится для восстановления после
/// аварии.
fn recover_network(report: bool) -> Result<(), String> {
    let state = StateDir::system();
    let mut acted = false;

    if let Some(specs) = state.load_routes() {
        for spec in &specs {
            run_quiet(&spec.del_cmd());
        }
        state.clear_routes();
        if report {
            println!("маршруты сняты: {} записей", specs.len());
        }
        acted = true;
    }
    if let Some(snapshot) = state.load_dns() {
        for svc in &snapshot {
            if run_quiet(&dnsconf::set_dns_cmd(&svc.service, &svc.servers)) && report {
                println!("DNS сервиса {} восстановлен", svc.service);
            }
        }
        state.clear_dns();
        acted = true;
    }
    run_quiet(&pf::flush_cmd());
    if let Some(reference) = state.load_pf_ref() {
        run_quiet(&pf::release_cmd(&reference));
        state.clear_pf_ref();
        acted = true;
    }

    if report {
        if acted {
            println!("сеть возвращена в исходное состояние");
        } else {
            println!("правок сети не найдено: восстанавливать нечего");
        }
    }
    Ok(())
}

fn cmd_recover() -> i32 {
    if let Err(e) = require_root() {
        eprintln!("{e}");
        return 1;
    }
    match recover_network(true) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cmd_status() -> i32 {
    let state = StateDir::system();
    println!("== сервис ==");

    let launchd_out = run_capture(&launchd::state_cmd());
    let loaded = launchd_out.as_deref().is_some();
    let running = launchd_out
        .as_deref()
        .map(|text| text.contains("state = running"))
        .unwrap_or(false);
    if loaded {
        println!("launchd: {}", if running { "запущен" } else { "загружен, стоит" });
    } else {
        println!("launchd: сервис не загружен");
    }

    let mut verdict_problem = String::new();
    if let Some(st) = state.load_status() {
        println!("состояние: {}", st.state);
        println!(
            "аптайм: {} с, соединений активно: {}",
            st.uptime_seconds, st.active_connections
        );
        println!(
            "трафик: {} КБ вверх / {} КБ вниз, QUIC срезано: {}",
            st.bytes_up / 1024,
            st.bytes_down / 1024,
            st.quic_dropped
        );
        println!(
            "туннель: {}, физический интерфейс: {}",
            if st.tun.is_empty() { "-" } else { &st.tun },
            if st.gateway.is_empty() { "-" } else { &st.gateway }
        );
        println!("DNS в туннеле: {}", if st.dns_applied { "да" } else { "нет" });
        println!("kill-switch: {}", if st.killswitch_armed { "взведён" } else { "выключен" });
        if !st.servers.is_empty() {
            println!("серверы: {}", st.servers.join(", "));
        }
        if !st.last_error.is_empty() {
            println!("последняя ошибка: {}", st.last_error);
            verdict_problem = st.last_error.clone();
        } else if st.state != "Connected" {
            verdict_problem = format!("состояние {}", st.state);
        }
    } else {
        println!("статус-файл не найден: демон ещё не писал статус");
        verdict_problem = "статус-файл не найден".into();
    }

    println!("== сеть ==");
    if let Some(st) = state.load_status() {
        if !st.tun.is_empty() {
            let interfaces = run_capture(&Cmd::new(exec::IFCONFIG, &["-l"])).unwrap_or_default();
            let alive = interfaces.split_whitespace().any(|i| i == st.tun);
            println!("интерфейс {}: {}", st.tun, if alive { "жив" } else { "отсутствует" });
            if !alive && verdict_problem.is_empty() {
                verdict_problem = format!("интерфейс {} отсутствует", st.tun);
            }
        }
    }
    if unsafe { libc::geteuid() } == 0 {
        let rules = run_capture(&pf::rules_cmd()).unwrap_or_default();
        let armed = rules.contains("block out quick all");
        println!("правила PF: {}", if armed { "анкор активен" } else { "анкор пуст" });
    } else {
        println!("правила PF: проверка требует sudo");
    }

    if !loaded {
        println!("итог: сервис не установлен или не запущен");
    } else if running && verdict_problem.is_empty() {
        println!("итог: работает");
    } else if verdict_problem.is_empty() {
        println!("итог: запуск ещё не дошёл до сети, смотрите статус через минуту");
    } else {
        println!("итог: проблема, {verdict_problem}");
    }
    0
}

fn cmd_start() -> i32 {
    if let Err(e) = require_root() {
        eprintln!("{e}");
        return 1;
    }
    if run_capture(&launchd::state_cmd()).is_some() {
        if run_quiet(&launchd::kickstart_cmd()) {
            println!("сервис перезапущен");
            return 0;
        }
        eprintln!("перезапуск не удался, смотрите /var/log/xr-macos.log");
        return 1;
    }
    if run_quiet(&launchd::load_cmd()) || run_quiet(&launchd::load_legacy_cmd()) {
        println!("сервис запущен");
        0
    } else {
        eprintln!("плист не загрузился, смотрите /var/log/xr-macos.log");
        1
    }
}

fn cmd_stop() -> i32 {
    if let Err(e) = require_root() {
        eprintln!("{e}");
        return 1;
    }
    if run_quiet(&launchd::unload_cmd()) || run_quiet(&launchd::unload_legacy_cmd()) {
        println!("сервис остановлен, сеть возвращена в исходное состояние");
        0
    } else {
        eprintln!("сервис не загружен; если сеть осталась в туннеле, поможет sudo xr-macos recover");
        1
    }
}
