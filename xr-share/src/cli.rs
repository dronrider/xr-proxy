//! v2 multishare subcommands (LLD-19 §9.3): `install`, `share`, `list`,
//! `unshare`.
//!
//! These let the operator install once and then share any number of paths with a
//! single command each. The agent talks to the hub on the operator's behalf using
//! the long-lived **agent credential** obtained at install time, so no admin
//! action is needed per share. The config is the single source of truth: `share`
//! and `unshare` rewrite it, and the running agent hot-reloads it.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Args;
use ed25519_dalek::SigningKey;

use crate::config::{AgentConfig, ShareEntry};
use crate::setup;

/// Split a `--setup` blob back into (reg_token, invite_token). Inverse of the
/// hub's pack (XR-127): base64url of "<reg>.<invite>".
fn unpack_setup_token(blob: &str) -> Result<(String, String)> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .context("setup-токен не base64url")?;
    let joined = String::from_utf8(raw).context("setup-токен не utf8")?;
    let (reg, inv) = joined
        .split_once('.')
        .context("setup-токен без разделителя reg.invite")?;
    if reg.is_empty() || inv.is_empty() {
        bail!("setup-токен: пустая часть reg или invite");
    }
    Ok((reg.to_string(), inv.to_string()))
}

/// Attach targets for a `share`: an explicit `--invite` wins; otherwise the
/// install-time default invite from a `--setup` token, if any (XR-127).
fn resolve_invites(explicit: &[String], default_invite: Option<&str>) -> Vec<String> {
    if explicit.is_empty() {
        default_invite.map(str::to_string).into_iter().collect()
    } else {
        explicit.to_vec()
    }
}

/// Whether to advertise a share through the hub relay. On by default once the
/// agent holds a relay descriptor; `--relay` forces it, `--no-relay` opts out.
fn resolve_via_relay(force_relay: bool, no_relay: bool, has_descriptor: bool) -> bool {
    if no_relay {
        false
    } else if force_relay {
        true
    } else {
        has_descriptor
    }
}

#[derive(Args)]
pub struct InstallArgs {
    /// Hub base URL, e.g. https://xr-hub.zoobr.top. Prompted if omitted.
    #[arg(long)]
    pub hub: Option<String>,
    /// Registration token from the hub admin "install command", exchanged for a
    /// long-lived agent credential so later `share`s need no admin action.
    #[arg(long)]
    pub token: Option<String>,
    /// One-token onboarding (XR-127): a setup blob from the hub admin that packs
    /// the reg-token together with an invite. Redeems the reg half for a mandate
    /// and pins the invite as the default for later `share`s, so the whole flow is
    /// a single command. Takes the place of `--token`.
    #[arg(long)]
    pub setup: Option<String>,
    /// Listen address (default 0.0.0.0:8443).
    #[arg(long)]
    pub listen: Option<String>,
    /// Don't install the OS autostart service (just write the config).
    #[arg(long)]
    pub no_service: bool,
    /// Don't prompt — take every value from flags.
    #[arg(long)]
    pub non_interactive: bool,
    /// Clean reinstall: new identity, drop existing shares. Without it an existing
    /// agent's identity, shares and mandate are kept (re-running install is safe).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct ShareArgs {
    /// Path to share: a directory (its tree) or a single file.
    pub path: String,
    /// Human label for the share (defaults to the path's file name).
    #[arg(long)]
    pub name: Option<String>,
    /// Reachable address to advertise; defaults to the source IP the hub sees.
    #[arg(long)]
    pub addr: Option<String>,
    /// Access-token lifetime in seconds (hub default 7 days, cap 30 days).
    #[arg(long)]
    pub ttl: Option<u64>,
    /// Attach the new share to this invite, so everyone holding that invite
    /// reaches it (the access anchor, §9.5). Repeatable.
    #[arg(long = "invite")]
    pub invites: Vec<String>,
    /// Force the share reachable through the hub's relay (LLD-23 п. 2.4) even if
    /// the agent has no relay descriptor yet. Relay is on by default whenever the
    /// mandate carried one (XR-127); this flag only matters to force it on.
    #[arg(long)]
    pub relay: bool,
    /// Opt out of the relay leg for this share (XR-127): advertise direct-only
    /// even though the agent has a relay descriptor. Use on a public-IP host where
    /// the relay uplink would be dead weight.
    #[arg(long)]
    pub no_relay: bool,
}

/// `xr-share install` — set up the binary + service with **no** folder binding.
/// With `--token`, swaps the reg-token for an agent credential right away (§9.3).
pub fn install(config_path: &Path, args: InstallArgs) -> Result<()> {
    println!("xr-share install — установка агента (без привязки к папке)\n");

    // A --setup blob carries the reg-token and an invite in one (XR-127): the reg
    // half stands in for --token, the invite becomes the default attach target.
    let (token, setup_invite) = match args.setup.as_deref() {
        Some(blob) => {
            let (reg, inv) = unpack_setup_token(blob).context("разбор setup-токена")?;
            (Some(reg), Some(inv))
        }
        None => (args.token.clone(), None),
    };

    // Re-running install must not orphan an existing agent's shares (XR-037):
    // generating a fresh identity and an empty share list would leave every
    // registered share unreachable on the hub. Keep the existing config and just
    // refresh the autostart service. A clean wipe is opt-in via --force.
    if !args.force {
        if let Ok(mut existing) = read_config(config_path) {
            let mut changed = false;

            // Self-heal a half-installed agent: a config without a mandate (a prior
            // tokenless install) plus a token now means we can redeem it, reusing
            // the existing identity, so the one command completes whatever the
            // prior state (XR-127).
            if existing.agent_credential.is_none() {
                if let (Some(tok), Some(hub)) = (token.as_deref(), existing.hub_url.clone()) {
                    if let Some(id) = existing.identity_signing_key(config_path)? {
                        let agent_pub = setup::b64(id.verifying_key().as_bytes());
                        let (cred, relay) =
                            exchange(&hub, tok, &agent_pub).context("обмен reg-токена на мандат")?;
                        existing.agent_credential = Some(cred);
                        if let Some(d) = relay {
                            existing.relay = Some(crate::config::RelayAgentConfig::from_descriptor(&d));
                        }
                        changed = true;
                        println!("  мандат получен для существующего конфига");
                    }
                }
            }

            // A fresh --setup can re-point where future shares attach.
            if let Some(inv) = &setup_invite {
                if existing.default_invite.as_deref() != Some(inv.as_str()) {
                    existing.default_invite = Some(inv.clone());
                    changed = true;
                    println!("  инвайт по умолчанию обновлён из setup-токена");
                }
            }

            if changed {
                normalize_legacy(&mut existing);
                write_config(config_path, &existing)?;
            }

            println!("  ✓ найден конфиг: личность, шары ({}) и мандат сохранены", existing.shares.len());
            if !args.no_service {
                setup::service_install(config_path)?;
            }
            println!("\n✓ Агент обновлён, существующие шары на месте.");
            println!("  Перезапусти службу, чтобы поднять новый бинарь.");
            println!("  Полная переустановка с нуля: xr-share install --force ...");
            return Ok(());
        }
    }

    let hub = setup::resolve(args.hub, args.non_interactive, "URL хаба (напр. https://xr-hub.zoobr.top)", None)?;
    let listen = setup::resolve(args.listen, args.non_interactive, "Адрес прослушивания", Some("0.0.0.0:8443"))?;

    let hub_pubkey = setup::fetch_hub_pubkey(&hub).context("не удалось получить публичный ключ хаба")?;
    println!("  ✓ публичный ключ хаба получен");

    // This agent's identity. The hub binds the credential to its public key.
    let identity = SigningKey::generate(&mut rand::thread_rng());
    let agent_pub = setup::b64(identity.verifying_key().as_bytes());

    let (agent_credential, relay_cfg) = match token.as_deref() {
        Some(token) => {
            let (cred, relay) = exchange(&hub, token, &agent_pub).context("обмен reg-токена на мандат")?;
            println!("  ✓ мандат агента получен (можно шарить без админки)");
            if relay.is_some() {
                println!("  ✓ relay-дескриптор получен (шары за NAT доступны через relay)");
            }
            (Some(cred), relay.map(|d| crate::config::RelayAgentConfig::from_descriptor(&d)))
        }
        None => {
            println!("  ! без --token/--setup мандата нет: запросишь его позже через `install --token <reg-токен>`");
            (None, None)
        }
    };

    let cfg = AgentConfig {
        listen,
        hub_pubkey,
        hub_url: Some(hub.trim_end_matches('/').to_string()),
        agent_credential,
        identity_key: Some(setup::b64(&identity.to_bytes())),
        tls: None,
        relay: relay_cfg,
        default_invite: setup_invite,
        shares: Vec::new(),
        dir: None,
        share_id: None,
    };
    write_config(config_path, &cfg)?;
    println!("\n✓ Конфиг записан: {} (agent_pubkey {agent_pub})", config_path.display());

    if args.no_service {
        println!("Служба не установлена (--no-service). Запусти агента вручную: xr-share -c {}", config_path.display());
    } else {
        println!("\nУстанавливаю службу автозапуска…");
        setup::service_install(config_path)?;
    }
    println!("\nТеперь шарь сколько угодно путей:");
    println!("  xr-share share /srv/photos");
    println!("  xr-share share /srv/report.pdf");
    Ok(())
}

/// `xr-share share <path>` — register one path with the hub and print its link.
pub fn share(config_path: &Path, args: ShareArgs) -> Result<()> {
    let mut cfg = read_config(config_path)?;
    let cred = cfg
        .agent_credential
        .clone()
        .context("в конфиге нет agent_credential — сначала `xr-share install --token <reg-токен>`")?;
    let hub = cfg
        .hub_url
        .clone()
        .context("в конфиге нет hub_url, переустанови через `xr-share install`")?;

    // Attach targets: explicit --invite wins; otherwise the install-time default
    // invite from a --setup token (XR-127), so onboarding needs no per-share flag.
    let invites = resolve_invites(&args.invites, cfg.default_invite.as_deref());

    // Relay is on by default whenever the agent holds a relay descriptor (the
    // mandate carried one). --relay forces it on even before the descriptor
    // arrives; --no-relay opts a public-IP host out of the uplink (XR-127).
    let via_relay = resolve_via_relay(args.relay, args.no_relay, cfg.relay.is_some());

    let canon = Path::new(&args.path)
        .canonicalize()
        .with_context(|| format!("путь не найден или недоступен: {}", args.path))?;
    if !canon.is_file() && !canon.is_dir() {
        bail!("путь не файл и не директория: {}", args.path);
    }
    let name = args.name.clone().unwrap_or_else(|| {
        canon
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "share".into())
    });
    let port: u16 = setup::port_of(&cfg.listen).parse().unwrap_or(8443);

    let mut body = serde_json::json!({
        "credential": cred,
        "name": name,
        "port": port,
    });
    if let Some(ttl) = args.ttl {
        body["ttl_seconds"] = serde_json::json!(ttl);
    }
    if let Some(addr) = args.addr.as_deref() {
        body["addr"] = serde_json::json!(addr);
    }
    if via_relay {
        body["via_relay"] = serde_json::json!(true);
    }
    let resp = hub_post(&format!("{}/api/v1/share/add", hub.trim_end_matches('/')), &body)
        .context("регистрация шары в хабе")?;
    let share_id = str_field(&resp, "share_id")?;
    let addr = str_field(&resp, "addr")?;
    let token = str_field(&resp, "token")?;

    // A relay-reachable share gets the relay descriptor back; store it so the
    // running agent brings up its reverse tunnel (LLD-23 §2.4).
    if let Some(relay) = resp.get("relay").filter(|v| !v.is_null()) {
        let desc: xr_proto::share::RelayDescriptor =
            serde_json::from_value(relay.clone()).context("разбор relay-дескриптора")?;
        cfg.relay = Some(crate::config::RelayAgentConfig::from_descriptor(&desc));
    }

    // Persist the share locally; the running agent hot-reloads it.
    normalize_legacy(&mut cfg);
    cfg.shares.push(ShareEntry {
        share_id: share_id.clone(),
        path: canon.display().to_string(),
        name: Some(name.clone()),
    });
    write_config(config_path, &cfg)?;

    // Attach to invites so their holders get access (the access anchor, п. 9.5).
    for invite in &invites {
        let body = serde_json::json!({ "credential": cred, "share_id": share_id, "invite_token": invite });
        match hub_post(&format!("{}/api/v1/share/attach", hub.trim_end_matches('/')), &body) {
            Ok(_) => println!("  ✓ привязана к инвайту {}", short(invite)),
            Err(e) => println!("  ! не удалось привязать к инвайту {}: {e}", short(invite)),
        }
    }

    let kind = if canon.is_file() { "файл" } else { "папка" };
    println!("✓ Шара добавлена ({kind}): {name}");
    println!("  путь:     {}", canon.display());
    println!("  share_id: {share_id}");
    println!("  адрес:    {addr}:{port}");
    if addr_is_private(&addr) {
        eprintln!("\n  ВНИМАНИЕ: адрес {addr} приватный, шара видна только в локальной сети.");
        eprintln!("  Снаружи она недоступна. Регистрируй с хоста агента (хаб подставит белый IP сам),");
        eprintln!("  либо передай --addr <публичный IP или DDNS> и пробрось порт {port} на эту машину.");
    }
    if invites.is_empty() {
        // No invite: hand out a self-contained link (receiver pulls directly).
        println!("\n  Ссылка для получателя (отправь её в мессенджере):");
        println!("  xrshare://{addr}:{port}/{share_id}?token={token}");
    } else {
        println!("\n  Получатели с привязанным инвайтом уже видят шару (xr-share pull / приложение).");
    }
    Ok(())
}

/// True if a resolved address is a private/loopback/link-local IP, so a share at
/// it is reachable only inside the LAN. A hostname (DDNS) is treated as public.
fn addr_is_private(addr: &str) -> bool {
    use std::net::IpAddr;
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => false,
    }
}

fn short(s: &str) -> String {
    if s.len() > 12 { format!("{}…", &s[..10]) } else { s.to_string() }
}

/// `xr-share list` — show the shares this agent serves.
pub fn list(config_path: &Path) -> Result<()> {
    let cfg = read_config(config_path)?;
    let shares = cfg.resolved_shares();
    if shares.is_empty() {
        println!("Нет шар. Добавь: xr-share share <путь>");
        return Ok(());
    }
    println!("Шары этого агента ({}):", shares.len());
    for s in &shares {
        let kind = if Path::new(&s.path).is_file() { "файл " } else { "папка" };
        println!("  {}  [{kind}]  {}  {}", s.share_id, s.name.as_deref().unwrap_or("-"), s.path);
    }
    Ok(())
}

/// `xr-share unshare <id|path>` — drop a share (locally and on the hub index).
pub fn unshare(config_path: &Path, target: &str) -> Result<()> {
    let mut cfg = read_config(config_path)?;
    normalize_legacy(&mut cfg);

    let canon_target = Path::new(target).canonicalize().ok();
    let idx = cfg.shares.iter().position(|s| {
        s.share_id == target
            || s.path == target
            || canon_target.as_deref().is_some_and(|c| Path::new(&s.path) == c)
    });
    let Some(idx) = idx else {
        bail!("шара не найдена по id или пути: {target}");
    };
    let share_id = cfg.shares[idx].share_id.clone();

    // Best-effort hub removal so the index entry disappears. Keep going on
    // failure (hub down): the local removal still stops serving the bytes.
    if let (Some(cred), Some(hub)) = (cfg.agent_credential.clone(), cfg.hub_url.clone()) {
        let body = serde_json::json!({ "credential": cred, "share_id": share_id });
        match hub_post(&format!("{}/api/v1/share/unshare", hub.trim_end_matches('/')), &body) {
            Ok(_) => println!("  ✓ запись удалена из индекса хаба"),
            Err(e) => println!("  ! хаб не подтвердил удаление ({e}); запись в индексе может остаться, повтори позже"),
        }
    }

    cfg.shares.remove(idx);
    write_config(config_path, &cfg)?;
    println!("✓ Шара {share_id} убрана из раздачи");
    Ok(())
}

// ── hub client ──────────────────────────────────────────────────────

/// Trade a reg-token for an agent credential blob (`POST /share/exchange`),
/// plus the hub's relay descriptor if it advertises one (LLD-23 §2.4).
fn exchange(
    hub: &str,
    token: &str,
    agent_pubkey: &str,
) -> Result<(String, Option<xr_proto::share::RelayDescriptor>)> {
    let body = serde_json::json!({ "token": token, "agent_pubkey": agent_pubkey });
    let resp = hub_post(&format!("{}/api/v1/share/exchange", hub.trim_end_matches('/')), &body)?;
    let cred = str_field(&resp, "credential")?;
    let relay = resp
        .get("relay")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("разбор relay-дескриптора")?;
    Ok((cred, relay))
}

fn hub_post(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    match ureq::post(url)
        .timeout(Duration::from_secs(15))
        .set("content-type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(r) => {
            let s = r.into_string().unwrap_or_default();
            if s.trim().is_empty() {
                Ok(serde_json::Value::Null)
            } else {
                serde_json::from_str(&s).context("разбор ответа хаба")
            }
        }
        Err(ureq::Error::Status(code, r)) => bail!(
            "хаб отклонил запрос (HTTP {code}): {}",
            r.into_string().unwrap_or_default()
        ),
        Err(e) => bail!("сеть при запросе к хабу: {e}"),
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .with_context(|| format!("хаб не вернул поле `{key}`"))
}

// ── config read/write ───────────────────────────────────────────────

fn read_config(path: &Path) -> Result<AgentConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("чтение {} (запусти `xr-share install`?)", path.display()))?;
    toml::from_str(&text).with_context(|| format!("разбор {}", path.display()))
}

/// Write the config 0600. Legacy `dir`/`share_id` must already be folded into
/// `shares` (see [`normalize_legacy`]) so the TOML stays valid (no scalar key
/// after an array-of-tables).
fn write_config(path: &Path, cfg: &AgentConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("создание {}", parent.display()))?;
    }
    let text = toml::to_string(cfg).context("сериализация конфига")?;
    setup::write_private(path, text.as_bytes())
}

/// Fold a legacy single-share (`dir` + `share_id`) into the `[[share]]` list and
/// clear the legacy fields, so the rewritten config is pure v2.
fn normalize_legacy(cfg: &mut AgentConfig) {
    if let (Some(dir), Some(id)) = (cfg.dir.take(), cfg.share_id.take()) {
        if !cfg.shares.iter().any(|s| s.share_id == id) {
            cfg.shares.push(ShareEntry { share_id: id, path: dir, name: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(reg: &str, inv: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{reg}.{inv}").as_bytes())
    }

    // The setup blob the hub packs must split back into exactly its two tokens
    // (the reg-token itself is base64url and holds no dot, so the first dot is the
    // separator), XR-127.
    #[test]
    fn unpack_setup_token_roundtrips() {
        let (reg, inv) = unpack_setup_token(&pack("regTok-9", "invTok_1")).unwrap();
        assert_eq!(reg, "regTok-9");
        assert_eq!(inv, "invTok_1");
    }

    #[test]
    fn unpack_setup_token_rejects_malformed() {
        // Not base64url at all.
        assert!(unpack_setup_token("!!!nope!!!").is_err());
        // Valid base64url but no separator.
        let no_dot = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"regonly");
        assert!(unpack_setup_token(&no_dot).is_err());
        // Empty invite half.
        let empty = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"reg.");
        assert!(unpack_setup_token(&empty).is_err());
    }

    #[test]
    fn via_relay_defaults_to_having_a_descriptor() {
        // Default: follows whether the agent holds a relay descriptor.
        assert!(resolve_via_relay(false, false, true));
        assert!(!resolve_via_relay(false, false, false));
        // --relay forces on even without a descriptor yet.
        assert!(resolve_via_relay(true, false, false));
        // --no-relay wins over everything, including --relay.
        assert!(!resolve_via_relay(false, true, true));
        assert!(!resolve_via_relay(true, true, true));
    }

    #[test]
    fn invites_prefer_explicit_then_default() {
        // Explicit --invite wins and the default is ignored.
        assert_eq!(
            resolve_invites(&["a".into(), "b".into()], Some("def")),
            vec!["a".to_string(), "b".to_string()]
        );
        // No explicit invite: fall back to the default from a setup token.
        assert_eq!(resolve_invites(&[], Some("def")), vec!["def".to_string()]);
        // Neither: no attach.
        assert!(resolve_invites(&[], None).is_empty());
    }
}
