//! v2 multishare subcommands (LLD-19 §9.3): `install`, `share`, `list`,
//! `unshare`.
//!
//! These let the operator install once and then share any number of paths with a
//! single command each. The agent talks to the hub on the operator's behalf using
//! the long-lived **agent credential** obtained at install time, so no admin
//! action is needed per share. The config is the single source of truth: `share`
//! and `unshare` rewrite it, and the running agent hot-reloads it.

use std::path::{Path, PathBuf};
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
    /// Clean reinstall: new identity, drop existing shares (best-effort removing
    /// them from the hub index too, so they don't linger dead). Without it an
    /// existing agent's identity, shares and mandate are kept (re-running install
    /// is safe), even when the config lives off the requested path (XR-134).
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
    // refresh the autostart service. The config is looked up beyond the requested
    // path (service definition, OS default) so a re-onboarding finds the identity
    // even after a prior `-c <elsewhere>` install (XR-134). A clean wipe is
    // opt-in via --force.
    let mut previous: Option<(PathBuf, AgentConfig)> = None;
    let mut skipped_keyless = false;
    if args.force {
        // Capture the previous agent now, but touch the hub only at the very end,
        // once the fresh install actually succeeded: an aborted --force run
        // (bad token, Ctrl-C at a prompt) must leave the hub index intact.
        previous = locate_config(config_path);
        if previous.is_none() && setup::service_definition_exists() {
            println!("  ! прежний конфиг не найден или не читается: его шары останутся на хабе, сними их в админке");
        }
    } else if let Some((source, existing)) = locate_config(config_path) {
        // Adopting a config from another path is only worth it together with its
        // identity: without the key it would relocate a broken agent whose
        // shares stay dead anyway.
        let adoptable = source.as_path() == config_path
            || matches!(existing.identity_signing_key(&source), Ok(Some(_)));
        if adoptable {
            return refresh_existing(
                config_path,
                &source,
                existing,
                args.hub.as_deref(),
                token.as_deref(),
                setup_invite,
                args.no_service,
            );
        }
        println!(
            "  ! найден конфиг {} без читаемой личности: не переношу, его шары останутся без агента",
            source.display()
        );
        skipped_keyless = true;
    }

    let hub = setup::resolve(args.hub, args.non_interactive, "URL хаба (напр. https://xr-hub.zoobr.top)", None)?;
    let listen = setup::resolve(args.listen, args.non_interactive, "Адрес прослушивания", Some("0.0.0.0:8443"))?;

    let hub_pubkey = setup::fetch_hub_pubkey(&hub).context("не удалось получить публичный ключ хаба")?;
    println!("  ✓ публичный ключ хаба получен");

    // A service definition without a readable config means an agent lived here
    // before: the fresh identity below won't match what the hub has on record.
    // The key-less-config case printed its own warning above.
    if !args.force && !skipped_keyless && setup::service_definition_exists() {
        println!("  ! на этой машине уже ставился агент, но его конфиг не найден:");
        println!("    прежние шары останутся без агента, сними их в админке хаба");
    }

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

    // The wiped identity leaves the previous agent's shares dead on consumers'
    // invites; now that the fresh agent is in place, take them off the hub index.
    if let Some((path, old)) = &previous {
        drop_previous_shares(path, old);
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
    if let Some((hub, cred)) = unshare_target(&cfg) {
        match hub_unshare(&hub, &cred, &share_id) {
            Ok(()) => println!("  запись удалена из индекса хаба"),
            Err(e) => println!("  ! хаб не подтвердил удаление ({e}); запись в индексе может остаться, повтори позже"),
        }
    }

    cfg.shares.remove(idx);
    write_config(config_path, &cfg)?;
    println!("✓ Шара {share_id} убрана из раздачи");
    Ok(())
}

/// Refresh an already-installed agent in place: keep its identity, shares and
/// mandate, redeem a token if the mandate is missing, re-point the default
/// invite from a fresh `--setup`. When the config was found off the requested
/// path (a prior `-c <elsewhere>` install, XR-134) it moves to `config_path`
/// and the source file is parked, so exactly one mandated config remains and
/// the service and future commands agree on one location.
fn refresh_existing(
    config_path: &Path,
    source: &Path,
    mut existing: AgentConfig,
    hub_arg: Option<&str>,
    token: Option<&str>,
    setup_invite: Option<String>,
    no_service: bool,
) -> Result<()> {
    // Re-onboarding to a different hub is not an in-place refresh: keeping the
    // old hub's mandate with the new hub's invite breaks every later `share`
    // quietly, so this asks for an explicit clean reinstall instead.
    if let (Some(new_hub), Some(old_hub)) = (hub_arg, existing.hub_url.as_deref()) {
        if new_hub.trim_end_matches('/') != old_hub.trim_end_matches('/') {
            bail!(
                "агент уже привязан к хабу {old_hub}; перепривязка к {new_hub} это новая установка: \
                 xr-share install --force (шары на прежнем хабе останутся без агента)"
            );
        }
    }

    let migrating = source != config_path;
    let mut changed = migrating;

    // A legacy config (the init flow) carries no hub_url: take it from --hub so
    // the token below can still be redeemed and later commands know the hub.
    if existing.hub_url.is_none() {
        if let Some(h) = hub_arg {
            existing.hub_url = Some(h.trim_end_matches('/').to_string());
            changed = true;
        }
    }

    if migrating {
        println!("  найден конфиг существующего агента: {}", source.display());
        // Inline the identity stored next to the source (the init flow's
        // identity.key file), so the moved config is self-contained.
        if existing.identity_key.is_none() {
            if let Some(id) = existing.identity_signing_key(source)? {
                existing.identity_key = Some(setup::b64(&id.to_bytes()));
            }
        }
    }

    // Self-heal a half-installed agent: a config without a mandate (a prior
    // tokenless install) plus a token now means we can redeem it, reusing
    // the existing identity, so the one command completes whatever the
    // prior state (XR-127).
    if existing.agent_credential.is_none() {
        if let (Some(tok), Some(hub)) = (token, existing.hub_url.clone()) {
            if let Some(id) = existing.identity_signing_key(source)? {
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
    } else if token.is_some() {
        println!("  мандат уже есть, токен не использован (сбросить мандат: install --force)");
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
    if migrating {
        // Park the source so exactly one mandated config remains: a later `share`
        // without -c must not register shares into a file no service serves.
        let parked = source.with_extension("toml.imported");
        match std::fs::rename(source, &parked) {
            Ok(()) => println!(
                "  конфиг перенесён в {}, старый убран в {}",
                config_path.display(),
                parked.display()
            ),
            Err(e) => println!(
                "  ! конфиг перенесён в {}, но старый файл не убрался ({e}): удали {} сам",
                config_path.display(),
                source.display()
            ),
        }
        if no_service {
            println!(
                "  ! служба не переустановлена (--no-service): перезапусти агента с -c {}",
                config_path.display()
            );
        }
    }

    if existing.agent_credential.is_some() {
        println!("  личность, шары ({}) и мандат сохранены", existing.shares.len());
    } else {
        println!(
            "  личность и шары ({}) сохранены; мандата нет, запроси его: install --token <reg-токен>",
            existing.shares.len()
        );
    }
    if !no_service {
        setup::service_install(config_path)?;
    }
    println!("\nАгент обновлён, существующие шары на месте.");
    println!("  Перезапусти службу, чтобы поднять новый бинарь.");
    println!("  Полная переустановка с нуля: xr-share install --force ...");
    Ok(())
}

/// The existing agent's config: the requested path first, then the `-c`
/// recorded in the autostart service, then the OS default (XR-134).
fn locate_config(requested: &Path) -> Option<(PathBuf, AgentConfig)> {
    let mut candidates = vec![requested.to_path_buf()];
    candidates.extend(setup::service_config_path());
    candidates.push(setup::default_config_path());
    first_readable(&candidates)
}

/// First candidate that parses as a config.
fn first_readable(candidates: &[PathBuf]) -> Option<(PathBuf, AgentConfig)> {
    candidates
        .iter()
        .find_map(|c| read_config(c).ok().map(|cfg| (c.clone(), cfg)))
}

/// Hub coordinates for de-indexing a config's shares: needs both the mandate
/// and the hub URL.
fn unshare_target(cfg: &AgentConfig) -> Option<(String, String)> {
    Some((cfg.hub_url.clone()?, cfg.agent_credential.clone()?))
}

/// Remove one share from the hub index (shared by `unshare` and the `--force`
/// cleanup, so the wire contract lives in one place).
fn hub_unshare(hub: &str, cred: &str, share_id: &str) -> Result<()> {
    let body = serde_json::json!({ "credential": cred, "share_id": share_id });
    hub_post(&format!("{}/api/v1/share/unshare", hub.trim_end_matches('/')), &body).map(|_| ())
}

/// Take the previous agent's shares off the hub index after a `--force`
/// reinstall (XR-134): the wiped identity leaves them dead on consumers'
/// invites otherwise. Best-effort: a hub error leaves the entry for the admin.
fn drop_previous_shares(path: &Path, old: &AgentConfig) {
    let shares = old.resolved_shares();
    if shares.is_empty() {
        return;
    }
    let Some((hub, cred)) = unshare_target(old) else {
        println!(
            "  ! прежний конфиг ({}) держит шары ({}), но без мандата снять их с хаба нельзя, удали их в админке",
            path.display(),
            shares.len()
        );
        return;
    };
    for s in &shares {
        match hub_unshare(&hub, &cred, &s.share_id) {
            Ok(()) => println!("  прежняя шара {} снята с хаба", s.share_id),
            Err(e) => {
                println!("  ! прежняя шара {} осталась на хабе ({e}), сними её в админке", s.share_id);
                // A transport error repeats for every entry: stop instead of
                // burning a 15s timeout per share against a dead hub.
                if e.to_string().starts_with("сеть при запросе") {
                    if shares.len() > 1 {
                        println!("  ! хаб недоступен, остальные шары тоже остались в его индексе");
                    }
                    break;
                }
            }
        }
    }
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

    fn minimal_cfg() -> AgentConfig {
        AgentConfig {
            listen: "0.0.0.0:8443".into(),
            hub_pubkey: "QQ==".into(),
            hub_url: Some("https://hub".into()),
            agent_credential: Some("mandate".into()),
            identity_key: None,
            tls: None,
            relay: None,
            default_invite: None,
            shares: vec![ShareEntry { share_id: "s1".into(), path: "/srv/x".into(), name: None }],
            dir: None,
            share_id: None,
        }
    }

    fn write_cfg(path: &Path, cfg: &AgentConfig) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, toml::to_string(cfg).unwrap()).unwrap();
    }

    // Re-onboarding must find the existing agent's config wherever it lives
    // (XR-134): the requested path wins when readable, unreadable candidates are
    // skipped, the first parseable one wins.
    #[test]
    fn first_readable_prefers_earlier_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let requested = dir.path().join("new/config.toml");
        let old = dir.path().join("old/config.toml");
        write_cfg(&old, &minimal_cfg());
        let broken = dir.path().join("broken/config.toml");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "not toml [").unwrap();

        // The requested path is missing: the first readable candidate after it wins.
        let found = first_readable(&[requested.clone(), broken.clone(), old.clone()]);
        let (path, cfg) = found.expect("должен найти конфиг по соседнему пути");
        assert_eq!(path, old);
        assert_eq!(cfg.shares.len(), 1);

        // A readable requested path wins over the rest.
        write_cfg(&requested, &minimal_cfg());
        let (path, _) = first_readable(&[requested.clone(), old]).unwrap();
        assert_eq!(path, requested);

        assert!(first_readable(&[broken, dir.path().join("нет/config.toml")]).is_none());
    }

    // The incident of XR-134: reinstall at another path used to mint a fresh
    // identity and orphan the hub-registered shares. Adopting the old config must
    // carry the identity (inlined from the identity.key file next to it), the
    // shares and the mandate over to the requested path, and park the source so
    // exactly one mandated config remains.
    #[test]
    fn refresh_existing_migrates_identity_to_requested_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("old/config.toml");
        let requested = dir.path().join("etc/config.toml");
        let cfg = minimal_cfg();
        write_cfg(&source, &cfg);
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let key_b64 = setup::b64(&key.to_bytes());
        std::fs::write(source.parent().unwrap().join("identity.key"), format!("{key_b64}\n")).unwrap();

        refresh_existing(&requested, &source, cfg, None, None, None, true).unwrap();

        let moved = read_config(&requested).unwrap();
        assert_eq!(moved.identity_key.as_deref(), Some(key_b64.as_str()));
        assert_eq!(moved.shares.len(), 1);
        assert_eq!(moved.shares[0].share_id, "s1");
        assert_eq!(moved.agent_credential.as_deref(), Some("mandate"));
        // The source is parked, not left as a second mandated config.
        assert!(!source.exists());
        assert!(dir.path().join("old/config.toml.imported").exists());
    }

    // A fresh --setup on a re-run re-points where future shares attach, without
    // touching identity or shares; the same hub (modulo trailing slash) is not a
    // mismatch.
    #[test]
    fn refresh_existing_repoints_default_invite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = minimal_cfg();
        cfg.identity_key = Some("inline".into());
        cfg.default_invite = Some("oldInv".into());
        write_cfg(&path, &cfg);

        refresh_existing(&path, &path, cfg, Some("https://hub/"), None, Some("newInv".into()), true).unwrap();

        let updated = read_config(&path).unwrap();
        assert_eq!(updated.default_invite.as_deref(), Some("newInv"));
        assert_eq!(updated.identity_key.as_deref(), Some("inline"));
        assert_eq!(updated.shares.len(), 1);
    }

    // A legacy init-flow config has no hub_url: the --hub argument must fill it
    // in (and persist), or a supplied token would be dropped silently.
    #[test]
    fn refresh_existing_fills_hub_url_from_arg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = minimal_cfg();
        cfg.hub_url = None;
        cfg.identity_key = Some("inline".into());
        write_cfg(&path, &cfg);

        refresh_existing(&path, &path, cfg, Some("https://hub2/"), None, None, true).unwrap();

        assert_eq!(read_config(&path).unwrap().hub_url.as_deref(), Some("https://hub2"));
    }

    // Re-onboarding an agent onto a DIFFERENT hub must not mix the old hub's
    // mandate with the new hub's invite: that is an explicit --force reinstall.
    #[test]
    fn refresh_existing_bails_on_hub_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = minimal_cfg();
        write_cfg(&path, &cfg);

        let err = refresh_existing(&path, &path, cfg, Some("https://other-hub"), None, None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "ошибка должна вести к --force: {err}");
        // The config is untouched.
        assert_eq!(read_config(&path).unwrap().hub_url.as_deref(), Some("https://hub"));
    }

    // The --force cleanup needs both the mandate and the hub URL to de-index.
    #[test]
    fn unshare_target_requires_mandate_and_hub() {
        let mut cfg = minimal_cfg();
        assert_eq!(
            unshare_target(&cfg),
            Some(("https://hub".into(), "mandate".into()))
        );
        cfg.agent_credential = None;
        assert!(unshare_target(&cfg).is_none());
        cfg.agent_credential = Some("mandate".into());
        cfg.hub_url = None;
        assert!(unshare_target(&cfg).is_none());
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
