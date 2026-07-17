//! Desktop sender: `xr-share push` / `xr-share rm` (LLD-28 п. 2.4).
//!
//! The mirror image of [`crate::pull`]: authenticate by an invite, pick a share
//! it grants, and write to it (`PUT`) or remove from it (`DELETE`) over the same
//! plain-`ureq` transport, so the agent binary still cross-compiles to Windows
//! without pulling reqwest/aws-lc. This is both the no-device test tool for the
//! write path and a real way to put a file into someone's writable share from a
//! laptop. The Android sender uses `xr-core` over JNI (a later task).
//!
//! Write access is gated three times end to end (LLD-28 п. 0): the invite must
//! carry a write binding (else the grant's token has no `share:write` scope and
//! `push`/`rm` refuse locally, before any network), the share must be writable on
//! the hub, and the agent's own config must allow writes. On overwrite `push`
//! sends `If-Match` with the hash from the just-fetched manifest, so it cannot
//! silently clobber a newer version; `--force` drops that guard.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};
use xr_proto::share::{scope_contains, SCOPE_WRITE};

use crate::pull::{encode_path, fetch_manifest_verified, get_json, InviteShareDto, HUB_DEFAULT};

#[derive(Args)]
pub struct PushArgs {
    /// Invite token granting access (the access anchor, LLD-19 п. 9.5).
    #[arg(long)]
    pub invite: String,
    /// Hub base URL (default https://xr-hub.zoobr.top).
    #[arg(long)]
    pub hub: Option<String>,
    /// Which share to write to, by its share_id or name.
    #[arg(long)]
    pub share: String,
    /// Local file to upload.
    pub file: String,
    /// Destination path inside the share (default: the file's own name). May
    /// contain subdirectories, which the agent creates.
    #[arg(long)]
    pub to: Option<String>,
    /// Reach the agent over https (default http; the distributed agent serves HTTP).
    #[arg(long)]
    pub https: bool,
    /// Overwrite without the `If-Match` guard: push even if the file changed on
    /// the agent since the manifest was read (last-write-wins).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct RmArgs {
    /// Invite token granting access (the access anchor, LLD-19 п. 9.5).
    #[arg(long)]
    pub invite: String,
    /// Hub base URL (default https://xr-hub.zoobr.top).
    #[arg(long)]
    pub hub: Option<String>,
    /// Which share to delete from, by its share_id or name.
    #[arg(long)]
    pub share: String,
    /// Path inside the share to remove.
    pub rel: String,
    /// Reach the agent over https (default http).
    #[arg(long)]
    pub https: bool,
}

/// Upload a local file into a writable share (the `push` subcommand).
pub fn push(args: PushArgs) -> Result<()> {
    let hub = args.hub.clone().unwrap_or_else(|| HUB_DEFAULT.to_string());
    let share = select_share(&hub, &args.invite, &args.share)?;
    ensure_writable_grant(&share)?;

    let local = Path::new(&args.file);
    if !local.is_file() {
        bail!("файл не найден: {}", args.file);
    }
    let rel = match &args.to {
        Some(t) => t.clone(),
        None => local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .context("у файла нет имени, задай --to <путь в шаре>")?,
    };

    let scheme = if args.https { "https" } else { "http" };
    let base = format!("{scheme}://{}:{}/{}", share.addr, share.port, share.share_id);

    // On overwrite, pin the version we saw so a newer upload is not clobbered
    // silently. --force skips the fetch and the guard.
    let if_match = if args.force {
        None
    } else {
        current_hash(&base, &share, &rel)?
    };
    if if_match.is_some() {
        println!("  перезапись {rel}: If-Match по текущей версии (--force чтобы обойти)");
    }

    let sha = sha256_file(local).with_context(|| format!("хеш файла {}", args.file))?;
    let status = put_file(&base, &share, &rel, local, if_match.as_deref(), &sha)?;
    match status {
        201 => println!("готово, создан файл: {} -> {}/{rel}", args.file, share.name),
        204 => println!("готово, файл перезаписан: {} -> {}/{rel}", args.file, share.name),
        other => println!("готово, агент ответил {other}"),
    }
    Ok(())
}

/// Remove a file from a writable share (the `rm` subcommand).
pub fn rm(args: RmArgs) -> Result<()> {
    let hub = args.hub.clone().unwrap_or_else(|| HUB_DEFAULT.to_string());
    let share = select_share(&hub, &args.invite, &args.share)?;
    ensure_writable_grant(&share)?;

    let scheme = if args.https { "https" } else { "http" };
    let base = format!("{scheme}://{}:{}/{}", share.addr, share.port, share.share_id);
    let status = delete_file(&base, &share, &args.rel)?;
    match status {
        204 => println!("готово, файл удалён: {}/{}", share.name, args.rel),
        other => println!("готово, агент ответил {other}"),
    }
    Ok(())
}

/// Fetch the invite's shares and pick the one named by `--share` (id or name).
fn select_share(hub: &str, invite: &str, want: &str) -> Result<InviteShareDto> {
    let url = format!("{}/api/v1/invite/{}/shares", hub.trim_end_matches('/'), invite);
    let shares: Vec<InviteShareDto> = get_json(&url, None).context("список шар по инвайту")?;
    shares
        .into_iter()
        .find(|s| s.share_id == want || s.name == want)
        .with_context(|| format!("шара «{want}» не найдена на этом инвайте"))
}

/// Refuse before touching the network if the grant's token has no write scope:
/// this invite is read-only for the share (no write binding on the hub).
fn ensure_writable_grant(share: &InviteShareDto) -> Result<()> {
    let token = crate::auth::decode_token_blob(&share.token)
        .context("токен гранта не декодируется")?;
    if !scope_contains(&token.scope, SCOPE_WRITE) {
        bail!(
            "нет права записи: инвайт даёт шару «{}» только на чтение \
             (нужна write-привязка на хабе: xr-share share --writable)",
            share.name
        );
    }
    Ok(())
}

/// The current hash of `rel` in the share's manifest, or `None` if the file is
/// not there yet (a fresh upload needs no `If-Match`) or the agent has not hashed
/// it (cold cache, empty sha256, nothing to match against).
fn current_hash(base: &str, share: &InviteShareDto, rel: &str) -> Result<Option<String>> {
    let manifest = fetch_manifest_verified(&format!("{base}/manifest"), share)
        .with_context(|| format!("манифест шары «{}»", share.name))?;
    Ok(manifest
        .entries
        .into_iter()
        .find(|e| e.path == rel)
        .map(|e| e.sha256)
        .filter(|s| !s.is_empty()))
}

/// PUT the local file, streaming it (constant memory), returning the HTTP status
/// or a human-readable error for the write-path codes (LLD-28 п. 2.3).
fn put_file(
    base: &str,
    share: &InviteShareDto,
    rel: &str,
    local: &Path,
    if_match: Option<&str>,
    sha: &str,
) -> Result<u16> {
    let url = format!("{base}/file/{}", encode_path(rel));
    let file = File::open(local).with_context(|| format!("открытие {}", local.display()))?;
    let mut req = ureq::put(&url)
        .set("Authorization", &format!("Bearer {}", share.token))
        .set("X-Xr-Sha256", sha)
        .timeout(Duration::from_secs(300));
    if let Some(m) = if_match {
        req = req.set("If-Match", m);
    }
    match req.send(file) {
        Ok(r) => Ok(r.status()),
        Err(ureq::Error::Status(code, _)) => bail!("{}", put_error(code)),
        Err(e) => bail!("сеть при заливке: {e}"),
    }
}

/// DELETE `rel` from the share.
fn delete_file(base: &str, share: &InviteShareDto, rel: &str) -> Result<u16> {
    let url = format!("{base}/file/{}", encode_path(rel));
    match ureq::delete(&url)
        .set("Authorization", &format!("Bearer {}", share.token))
        .timeout(Duration::from_secs(60))
        .call()
    {
        Ok(r) => Ok(r.status()),
        Err(ureq::Error::Status(404, _)) => bail!("файла нет на агенте: {rel}"),
        Err(ureq::Error::Status(code, _)) => bail!("{}", put_error(code)),
        Err(e) => bail!("сеть при удалении: {e}"),
    }
}

/// Human-readable message for a write-path status code.
fn put_error(code: u16) -> String {
    match code {
        401 => "агент отклонил токен (нет доступа)".into(),
        403 => "запись запрещена: нет права записи, шара read-only или путь отклонён".into(),
        409 => "цель это каталог".into(),
        412 => "конфликт версий: файл на агенте изменился, перезалей с --force".into(),
        413 => "файл больше лимита агента (max_file_mb)".into(),
        422 => "sha256 не сошёлся при заливке".into(),
        507 => "на агенте кончилось место".into(),
        other => format!("агент ответил HTTP {other}"),
    }
}

/// Streaming SHA-256 of a file, lowercase hex (constant memory, 64 KiB chunks).
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}
