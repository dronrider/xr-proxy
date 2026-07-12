//! `init` and `service` subcommands (XR-028): turn a freshly-installed binary
//! into a configured, autostarting agent.
//!
//! `init` walks the operator through a config: which directory to share, the
//! hub URL (whose public key it fetches automatically), and the `share_id` the
//! hub hands back after registering this agent's identity. `service` wires it
//! into the OS: systemd on Linux, a Scheduled Task on Windows, a LaunchDaemon on
//! macOS (a proper Windows service needs SCM integration, a follow-up).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Args;
use ed25519_dalek::SigningKey;

/// OS-appropriate default config location (used when `--config` is omitted).
pub fn default_config_path() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        PathBuf::from(base).join("xr-share").join("config.toml")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/xr-share/config.toml")
    }
}

#[derive(Args)]
pub struct InitArgs {
    /// Directory to share (read-only). Prompted if omitted.
    #[arg(long)]
    pub dir: Option<String>,
    /// Hub base URL, e.g. https://xr-hub.zoobr.top. Prompted if omitted.
    #[arg(long)]
    pub hub: Option<String>,
    /// Listen address (default 0.0.0.0:8443).
    #[arg(long)]
    pub listen: Option<String>,
    /// share_id returned by the hub after registering. Prompted if omitted.
    #[arg(long)]
    pub share_id: Option<String>,
    /// Registration token from the hub admin: the agent **self-registers** and
    /// the hub assigns the share_id — no manual paste (no-hands install).
    #[arg(long)]
    pub token: Option<String>,
    /// Reachable address to advertise. With `--token`, defaults to the source
    /// IP the hub sees; override for a domain or a forwarded port.
    #[arg(long)]
    pub addr: Option<String>,
    /// Human name for the share (defaults to the machine hostname).
    #[arg(long)]
    pub name: Option<String>,
    /// Don't prompt — take every value from flags (for automation).
    #[arg(long)]
    pub non_interactive: bool,
}

/// Interactively (or via flags) write a config + agent identity.
pub fn init(config_path: &Path, args: InitArgs) -> Result<()> {
    println!("xr-share init — настройка агента раздачи файлов\n");

    // 1. Directory to serve.
    let dir = resolve(args.dir, args.non_interactive, "Папка для раздачи (read-only)", None)?;
    let canon = Path::new(&dir)
        .canonicalize()
        .with_context(|| format!("папка не найдена или недоступна: {dir}"))?;
    if !canon.is_dir() {
        bail!("это не директория: {dir}");
    }

    // 2. Listen address.
    let listen = resolve(args.listen, args.non_interactive, "Адрес прослушивания", Some("0.0.0.0:8443"))?;

    // 3. Hub URL → fetch its public key (pinned into the config).
    let hub = resolve(args.hub, args.non_interactive, "URL хаба (напр. https://xr-hub.zoobr.top)", None)?;
    let hub_pubkey = fetch_hub_pubkey(&hub).context("не удалось получить публичный ключ хаба")?;
    println!("  ✓ публичный ключ хаба получен");

    // 4. Generate this agent's identity (the consumer pins it, TOFU).
    let identity = SigningKey::generate(&mut rand::thread_rng());
    let agent_pub = b64(identity.verifying_key().as_bytes());
    let identity_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("identity.key");

    // 5. share_id — three ways: a registration token (self-register, no hands),
    //    an explicit --share-id, or an interactive paste.
    let share_id = if let Some(token) = args.token.as_deref() {
        let name = args.name.clone().unwrap_or_else(hostname);
        let sid = self_register(&hub, token, &name, args.addr.as_deref(), port_of(&listen), &agent_pub)?;
        println!("  ✓ агент зарегистрирован в хабе автоматически (share_id {sid})");
        sid
    } else if let Some(s) = args.share_id {
        s
    } else if args.non_interactive {
        bail!("--non-interactive: нужен --token или --share-id");
    } else {
        println!("\nЗарегистрируй шару в админке хаба (раздел «Shares»):");
        println!("  адрес:порт   = <публичный адрес этой машины>:{}", port_of(&listen));
        println!("  agent_pubkey = {agent_pub}");
        println!("Хаб вернёт share_id.\n");
        prompt("Вставь share_id")?
    };

    // 6. Persist config + identity (private files).
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("создание {}", parent.display()))?;
    let cfg = format!(
        "listen = \"{listen}\"\ndir = \"{}\"\nshare_id = \"{share_id}\"\nhub_pubkey = \"{hub_pubkey}\"\n",
        canon.display(),
    );
    write_private(config_path, cfg.as_bytes())?;
    write_private(&identity_path, format!("{}\n", b64(&identity.to_bytes())).as_bytes())?;

    println!("\n✓ Конфиг записан: {}", config_path.display());
    println!("✓ Identity: {} (agent_pubkey {agent_pub})", identity_path.display());
    println!("\nЗапусти службу автозапуска:");
    if cfg!(windows) {
        println!("  xr-share service install");
    } else {
        println!("  sudo xr-share service install");
    }
    Ok(())
}

/// GET `{hub}/api/v1/public-key`, validating it's a 32-byte ed25519 key.
pub(crate) fn fetch_hub_pubkey(hub: &str) -> Result<String> {
    let url = format!("{}/api/v1/public-key", hub.trim_end_matches('/'));
    let body = ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .call()
        .context("запрос к хабу")?
        .into_string()
        .context("чтение ответа")?;
    let key = body.trim().to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&key)
        .context("ключ хаба не похож на base64")?;
    if bytes.len() != 32 {
        bail!("ключ хаба должен быть 32 байта, получено {}", bytes.len());
    }
    Ok(key)
}

/// Self-register with the hub using a registration token; returns the share_id
/// the hub assigned. The hub fills the address from the request source IP unless
/// `addr` is given.
fn self_register(
    hub: &str,
    token: &str,
    name: &str,
    addr: Option<&str>,
    port: &str,
    agent_pubkey: &str,
) -> Result<String> {
    let url = format!("{}/api/v1/share/register", hub.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "token": token,
        "name": name,
        "port": port.parse::<u16>().unwrap_or(8443),
        "agent_pubkey": agent_pubkey,
    });
    if let Some(a) = addr {
        body["addr"] = serde_json::Value::String(a.to_string());
    }
    let resp = match ureq::post(&url)
        .set("content-type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            bail!("хаб отклонил регистрацию (HTTP {code}): {}", r.into_string().unwrap_or_default())
        }
        Err(e) => bail!("сеть при регистрации в хабе: {e}"),
    };
    let v: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    v.get("share_id")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .context("хаб не вернул share_id")
}

/// Best-effort machine hostname for the default share name.
pub(crate) fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "xr-share agent".to_string())
}

/// Install + enable the OS autostart for this agent.
pub fn service_install(config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("определение пути к бинарю")?;
    #[cfg(target_os = "linux")]
    {
        systemd_install(&exe, config_path)
    }
    #[cfg(target_os = "windows")]
    {
        schtasks_install(&exe, config_path)
    }
    #[cfg(target_os = "macos")]
    {
        launchd_install(&exe, config_path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (exe, config_path);
        bail!("`service install` поддержан только на Linux/Windows/macOS; запускай xr-share вручную")
    }
}

pub fn service_uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = run("systemctl", &["disable", "--now", "xr-share"]);
        let _ = std::fs::remove_file("/etc/systemd/system/xr-share.service");
        let _ = run("systemctl", &["daemon-reload"]);
        println!("✓ systemd-служба xr-share удалена");
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let _ = run("schtasks", &["/end", "/tn", "xr-share"]);
        run("schtasks", &["/delete", "/tn", "xr-share", "/f"])?;
        println!("✓ задача автозапуска xr-share удалена");
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = run("launchctl", &["unload", LAUNCHD_PLIST]);
        let _ = std::fs::remove_file(LAUNCHD_PLIST);
        println!("launchd-служба xr-share удалена");
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        bail!("не поддержано на этой ОС")
    }
}

pub fn service_status() -> Result<()> {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("systemctl")
        .args(["status", "xr-share", "--no-pager"])
        .status();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("schtasks")
        .args(["/query", "/tn", "xr-share", "/v", "/fo", "LIST"])
        .status();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_install(exe: &Path, config_path: &Path) -> Result<()> {
    let unit = format!(
        "[Unit]\n\
         Description=xr-share file-sharing agent (LLD-19)\n\
         After=network.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} -c {}\n\
         Restart=always\n\
         RestartSec=5\n\
         NoNewPrivileges=true\n\
         ProtectSystem=strict\n\
         ProtectHome=true\n\
         PrivateTmp=true\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe.display(),
        config_path.display(),
    );
    let unit_path = Path::new("/etc/systemd/system/xr-share.service");
    std::fs::write(unit_path, unit)
        .with_context(|| format!("запись {} (нужны права root — sudo?)", unit_path.display()))?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", "xr-share"])?;
    println!("✓ systemd-служба xr-share установлена и запущена");
    println!("  статус: systemctl status xr-share");
    println!("  логи:   journalctl -u xr-share -f");
    Ok(())
}

#[cfg(target_os = "windows")]
fn schtasks_install(exe: &Path, config_path: &Path) -> Result<()> {
    let tr = format!("\"{}\" -c \"{}\"", exe.display(), config_path.display());
    run(
        "schtasks",
        &["/create", "/tn", "xr-share", "/sc", "onstart", "/ru", "SYSTEM", "/rl", "HIGHEST", "/tr", &tr, "/f"],
    )?;
    let _ = run("schtasks", &["/run", "/tn", "xr-share"]);
    println!("✓ задача автозапуска xr-share создана (запуск при старте системы)");
    println!("  состояние: schtasks /query /tn xr-share");
    Ok(())
}

#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "top.zoobr.xr-share";
#[cfg(target_os = "macos")]
const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/top.zoobr.xr-share.plist";

/// Install a LaunchDaemon so the agent starts at boot and stays up (KeepAlive).
/// A system daemon runs as root, which matches the default config under /etc.
/// `load -w` works across macOS versions (bootstrap/bootout are newer-only).
#[cfg(target_os = "macos")]
fn launchd_install(exe: &Path, config_path: &Path) -> Result<()> {
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{exe}</string>\n\
         \x20   <string>-c</string>\n\
         \x20   <string>{cfg}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>StandardOutPath</key><string>/var/log/xr-share.log</string>\n\
         \x20 <key>StandardErrorPath</key><string>/var/log/xr-share.log</string>\n\
         </dict>\n\
         </plist>\n",
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        cfg = config_path.display(),
    );
    std::fs::write(LAUNCHD_PLIST, plist)
        .with_context(|| format!("запись {LAUNCHD_PLIST} (нужны права root, sudo?)"))?;
    // Reload cleanly: drop any previous instance, then load it enabled.
    let _ = run("launchctl", &["unload", LAUNCHD_PLIST]);
    run("launchctl", &["load", "-w", LAUNCHD_PLIST])?;
    println!("launchd-служба xr-share установлена и запущена");
    println!("  состояние: sudo launchctl list | grep {LAUNCHD_LABEL}");
    println!("  логи:      /var/log/xr-share.log");
    Ok(())
}

#[allow(dead_code)]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("запуск {cmd}"))?;
    if !status.success() {
        bail!("`{cmd} {}` завершился с кодом {:?}", args.join(" "), status.code());
    }
    Ok(())
}

// ── prompt helpers ──────────────────────────────────────────────────

pub(crate) fn resolve(flag: Option<String>, non_interactive: bool, label: &str, default: Option<&str>) -> Result<String> {
    if let Some(v) = flag {
        return Ok(v);
    }
    if non_interactive {
        return match default {
            Some(d) => Ok(d.to_string()),
            None => bail!("--non-interactive: не задано «{label}»"),
        };
    }
    match default {
        Some(d) => {
            print!("{label} [{d}]: ");
            io::stdout().flush()?;
            let line = read_line()?;
            Ok(if line.is_empty() { d.to_string() } else { line })
        }
        None => prompt(label),
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let line = read_line()?;
    if line.is_empty() {
        bail!("пустой ввод для «{label}»");
    }
    Ok(line)
}

fn read_line() -> Result<String> {
    let mut s = String::new();
    io::stdin().read_line(&mut s).context("чтение ввода")?;
    Ok(s.trim().to_string())
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Write a file with owner-only permissions (0600 on Unix) — config and
/// identity hold secrets and addresses.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("запись {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("права на {}", path.display()))?;
    }
    Ok(())
}

/// Best-effort port extraction from a `host:port` listen string (for the hint).
pub(crate) fn port_of(listen: &str) -> &str {
    listen.rsplit(':').next().unwrap_or("8443")
}
