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

#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "/etc/systemd/system/xr-share.service";
/// Registered task XML on Windows: UTF-16 file, no codepage guesswork (the
/// `schtasks /query` console output comes in the OEM codepage and mangles
/// non-ASCII paths).
#[cfg(target_os = "windows")]
fn windows_task_file() -> PathBuf {
    let root = std::env::var("SYSTEMROOT").unwrap_or_else(|_| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("Tasks").join("xr-share")
}

/// Config path recorded in the installed autostart service (`-c <path>`), if an
/// agent was already set up on this machine. This is how a re-run of `install`
/// finds the existing identity even when the config lives off the default path
/// (a prior `-c <elsewhere>` install): a fresh key would orphan every share
/// registered under the old one (XR-134).
pub(crate) fn service_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        config_arg_from_unit(&std::fs::read_to_string(SYSTEMD_UNIT).ok()?)
    }
    #[cfg(target_os = "macos")]
    {
        config_arg_from_plist(&std::fs::read_to_string(LAUNCHD_PLIST).ok()?)
    }
    #[cfg(target_os = "windows")]
    {
        let bytes = std::fs::read(windows_task_file()).ok()?;
        config_arg_from_schtasks(&xml_unescape(&utf16_or_utf8(&bytes)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Whether this machine carries traces of a previously installed agent (an
/// autostart definition), even if its config is gone or unreadable.
pub(crate) fn service_definition_exists() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(SYSTEMD_UNIT).exists()
    }
    #[cfg(target_os = "macos")]
    {
        Path::new(LAUNCHD_PLIST).exists()
    }
    #[cfg(target_os = "windows")]
    {
        windows_task_file().exists()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Decode a Windows task/config file that may be UTF-16 (schtasks XML) or plain
/// UTF-8, by BOM.
#[allow(dead_code)]
fn utf16_or_utf8(bytes: &[u8]) -> String {
    let decode16 = |bytes: &[u8], le: bool| -> String {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|p| if le { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => decode16(rest, true),
        [0xFE, 0xFF, rest @ ..] => decode16(rest, false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Extract the `-c <path>` argument from a systemd unit's ExecStart line. The
/// path is the single token after `-c` (the unit is written unquoted, so a path
/// with spaces would not survive systemd's own splitting either); tokenizing
/// keeps the parse correct if flags are ever appended after the path.
#[allow(dead_code)]
fn config_arg_from_unit(unit: &str) -> Option<PathBuf> {
    let line = unit.lines().find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let (_, rest) = line.split_once(" -c ")?;
    let path = rest.trim_start().split_whitespace().next()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Extract the `-c <path>` argument from a launchd plist's ProgramArguments.
/// Inverse of the escaping done at install time (see `xml_escape`).
#[allow(dead_code)]
fn config_arg_from_plist(plist: &str) -> Option<PathBuf> {
    let after = plist.split("<string>-c</string>").nth(1)?;
    let raw = after.split("<string>").nth(1)?.split("</string>").next()?;
    let path = xml_unescape(raw);
    (!path.trim().is_empty()).then(|| PathBuf::from(path.trim()))
}

/// Extract the `-c "<path>"` argument from Windows task text: either the
/// registered task XML (`<Arguments>-c "..."</Arguments>`) or `schtasks /query
/// /v /fo LIST` output; both carry the quoted path `schtasks_install` wrote.
#[allow(dead_code)]
fn config_arg_from_schtasks(listing: &str) -> Option<PathBuf> {
    let (_, rest) = listing.split_once("-c \"")?;
    let path = rest.split('"').next()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
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
        let _ = std::fs::remove_file(SYSTEMD_UNIT);
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
    let unit_path = Path::new(SYSTEMD_UNIT);
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

/// Escape a path for embedding as XML character data in the plist. A path with
/// `&` or `<` (legal on macOS) would otherwise produce a malformed plist that
/// `launchctl load` rejects.
#[allow(dead_code)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Inverse of [`xml_escape`], for reading paths back out of service XML (the
/// launchd plist, the Windows task file). `&amp;` goes last so escaped ampersands
/// don't double-decode.
#[allow(dead_code)]
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

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
        exe = xml_escape(&exe.display().to_string()),
        cfg = xml_escape(&config_path.display().to_string()),
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

#[cfg(test)]
mod service_config_path_tests {
    use super::*;

    // The `-c` argument recorded by the OS autostart definitions is how a re-run
    // of `install` finds an existing agent's config off the default path (XR-134);
    // each parser must match exactly what the matching install writes.
    #[test]
    fn unit_exec_start_yields_config_path() {
        let unit = "[Service]\nType=simple\nExecStart=/usr/local/bin/xr-share -c /home/u/xr/config.toml\nRestart=always\n";
        assert_eq!(config_arg_from_unit(unit), Some(PathBuf::from("/home/u/xr/config.toml")));
        // Flags appended after the path must not glue onto it.
        let with_flags = "ExecStart=/usr/bin/xr-share -c /srv/xr/config.toml --log-level info\n";
        assert_eq!(config_arg_from_unit(with_flags), Some(PathBuf::from("/srv/xr/config.toml")));
        assert_eq!(config_arg_from_unit("[Service]\nExecStart=/usr/bin/xr-share\n"), None);
        assert_eq!(config_arg_from_unit(""), None);
    }

    #[test]
    fn plist_program_arguments_yield_config_path() {
        let plist = "<array>\n  <string>/usr/local/bin/xr-share</string>\n  <string>-c</string>\n  <string>/Users/u/a&amp;b/config.toml</string>\n</array>";
        assert_eq!(config_arg_from_plist(plist), Some(PathBuf::from("/Users/u/a&b/config.toml")));
        assert_eq!(config_arg_from_plist("<plist><dict></dict></plist>"), None);
    }

    #[test]
    fn schtasks_text_yields_config_path() {
        // `schtasks /query /v /fo LIST` output form.
        let listing = "TaskName: \\xr-share\nTask To Run: \"C:\\Program Files\\xr-share.exe\" -c \"C:\\ProgramData\\xr-share\\config.toml\"\nStatus: Ready\n";
        assert_eq!(
            config_arg_from_schtasks(listing),
            Some(PathBuf::from("C:\\ProgramData\\xr-share\\config.toml"))
        );
        // Registered task XML form (the file under System32\Tasks).
        let xml = "<Exec><Command>\"C:\\Program Files\\xr-share.exe\"</Command><Arguments>-c \"C:\\Users\\Андрей\\xr\\config.toml\"</Arguments></Exec>";
        assert_eq!(
            config_arg_from_schtasks(xml),
            Some(PathBuf::from("C:\\Users\\Андрей\\xr\\config.toml"))
        );
        assert_eq!(config_arg_from_schtasks("Task To Run: \"C:\\x.exe\"\n"), None);
    }

    // The Windows task file is UTF-16 with a BOM; a Cyrillic path must survive
    // the decode (the schtasks console output would mangle it, XR-134).
    #[test]
    fn utf16_task_file_decodes_by_bom() {
        let text = "-c \"C:\\Users\\Андрей\\config.toml\"";
        let mut le = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(utf16_or_utf8(&le), text);
        let mut be = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(utf16_or_utf8(&be), text);
        assert_eq!(utf16_or_utf8(text.as_bytes()), text);
    }

    // The unescape used for reading paths back must invert the escape used when
    // writing them into service XML.
    #[test]
    fn xml_escape_roundtrips_through_unescape() {
        for path in ["/opt/a&b/x.toml", "/o/<x>/c", "C:\\plain\\config.toml"] {
            assert_eq!(xml_unescape(&xml_escape(path)), path);
        }
        assert_eq!(xml_unescape("&quot;x&quot; &amp;lt;"), "\"x\" &lt;");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    // A config path with '&'/'<'/'>' must not break the launchd plist XML.
    #[test]
    fn xml_escape_neutralizes_markup() {
        assert_eq!(xml_escape("/opt/a&b/x.toml"), "/opt/a&amp;b/x.toml");
        assert_eq!(xml_escape("/o/<x>/c"), "/o/&lt;x&gt;/c");
        assert_eq!(xml_escape("/usr/local/bin/xr-share"), "/usr/local/bin/xr-share");
    }
}
