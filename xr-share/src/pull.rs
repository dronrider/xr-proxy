//! Desktop receiver: `xr-share pull --invite <token>` (LLD-19 §9.6, XR-031).
//!
//! Authenticates by an invite, lists the shares attached to it, lets the operator
//! pick a subset of files (a whole folder or individual files), and downloads
//! them with SHA-256 verification. Self-contained on `ureq` so the agent binary
//! still cross-compiles to Windows (depending on `xr-core` would pull
//! reqwest/aws-lc, which does not build for `windows-gnu`). The Android receiver
//! uses `xr-core` over JNI; the pure diff there is shared, the transport is not.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use xr_proto::share::ShareManifest;

const HUB_DEFAULT: &str = "https://xr-hub.zoobr.top";

#[derive(Args)]
pub struct PullArgs {
    /// Invite token granting access (the access anchor, §9.5).
    #[arg(long)]
    pub invite: String,
    /// Hub base URL (default https://xr-hub.zoobr.top).
    #[arg(long)]
    pub hub: Option<String>,
    /// Destination directory (default ./xr-share-pull).
    #[arg(long)]
    pub dest: Option<String>,
    /// Take everything without prompting.
    #[arg(long)]
    pub all: bool,
    /// Non-interactive selection: comma-separated manifest paths.
    #[arg(long)]
    pub select: Option<String>,
    /// Reach agents over https (default http; the distributed agent serves HTTP).
    #[arg(long)]
    pub https: bool,
    /// Limit to one share by its share_id or name.
    #[arg(long)]
    pub share: Option<String>,
}

#[derive(Deserialize)]
struct InviteShareDto {
    share_id: String,
    name: String,
    addr: String,
    port: u16,
    token: String,
}

pub fn pull(args: PullArgs) -> Result<()> {
    let hub = args.hub.clone().unwrap_or_else(|| HUB_DEFAULT.to_string());
    let url = format!("{}/api/v1/invite/{}/shares", hub.trim_end_matches('/'), args.invite);
    let shares: Vec<InviteShareDto> = get_json(&url, None).context("список шар по инвайту")?;
    if shares.is_empty() {
        println!("На этом инвайте нет привязанных шар.");
        return Ok(());
    }

    let scheme = if args.https { "https" } else { "http" };
    let dest_root = PathBuf::from(args.dest.clone().unwrap_or_else(|| "xr-share-pull".into()));
    let mut total = 0usize;

    for s in &shares {
        if let Some(f) = &args.share {
            if &s.share_id != f && &s.name != f {
                continue;
            }
        }
        let base = format!("{scheme}://{}:{}/{}", s.addr, s.port, s.share_id);
        let manifest: ShareManifest = get_json(&format!("{base}/manifest"), Some(&s.token))
            .with_context(|| format!("манифест шары «{}»", s.name))?;
        if manifest.entries.is_empty() {
            println!("[{}] пусто", s.name);
            continue;
        }

        let chosen = choose(&manifest, &args, &s.name)?;
        if chosen.is_empty() {
            println!("[{}] ничего не выбрано", s.name);
            continue;
        }

        let share_dir = dest_root.join(sanitize(&s.name));
        for entry in manifest.entries.iter().filter(|e| chosen.contains(&e.path)) {
            let dest = safe_join(&share_dir, &entry.path)
                .with_context(|| format!("небезопасный путь в манифесте: {}", entry.path))?;
            download_verify(
                &format!("{base}/file/{}", encode_path(&entry.path)),
                &s.token,
                &dest,
                &entry.sha256,
            )
            .with_context(|| format!("скачивание {}", entry.path))?;
            println!("  ✓ {}", entry.path);
            total += 1;
        }
    }
    println!("Готово: {total} файл(ов) в {}", dest_root.display());
    Ok(())
}

/// Which manifest paths to download: `--all`, `--select a,b`, or interactive.
fn choose(manifest: &ShareManifest, args: &PullArgs, share_name: &str) -> Result<HashSet<String>> {
    if args.all {
        return Ok(manifest.entries.iter().map(|e| e.path.clone()).collect());
    }
    if let Some(sel) = &args.select {
        return Ok(sel
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect());
    }
    // Interactive: numbered list, blank/"all" selects everything.
    println!("\nШара «{share_name}»: отметь файлы (номера через пробел или запятую, Enter = все):");
    for (i, e) in manifest.entries.iter().enumerate() {
        println!("  [{}] {} ({} б)", i + 1, e.path, e.size);
    }
    print!("> ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() || line.eq_ignore_ascii_case("all") {
        return Ok(manifest.entries.iter().map(|e| e.path.clone()).collect());
    }
    let mut set = HashSet::new();
    for tok in line.split(|c: char| c == ' ' || c == ',' || c == '\t').filter(|s| !s.is_empty()) {
        let idx: usize = tok.parse().map_err(|_| anyhow::anyhow!("не число: {tok}"))?;
        let e = manifest
            .entries
            .get(idx.wrapping_sub(1))
            .ok_or_else(|| anyhow::anyhow!("нет файла #{idx}"))?;
        set.insert(e.path.clone());
    }
    Ok(set)
}

/// GET a JSON body, optionally with a bearer token. Maps a 4xx/5xx to a clear
/// error instead of a panic.
fn get_json<T: DeserializeOwned>(url: &str, token: Option<&str>) -> Result<T> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(30));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let body = match req.call() {
        Ok(r) => r.into_string().context("чтение ответа")?,
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {code}: {}", r.into_string().unwrap_or_default())
        }
        Err(e) => bail!("сеть: {e}"),
    };
    serde_json::from_str(&body).context("разбор JSON")
}

/// Stream a file to `dest`, verifying SHA-256, publishing atomically only on a
/// match (a truncated transfer never appears as a real file).
fn download_verify(url: &str, token: &str, dest: &Path, want_sha: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("создание {}", parent.display()))?;
    }
    let resp = match ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(300))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => bail!("HTTP {code}"),
        Err(e) => bail!("сеть: {e}"),
    };

    let part = dest.with_extension("xrpull-part");
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part).with_context(|| format!("создание {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    drop(file);

    let got = hex_lower(&hasher.finalize());
    if !got.eq_ignore_ascii_case(want_sha) {
        let _ = std::fs::remove_file(&part);
        bail!("sha256 не совпал (ждали {want_sha}, получили {got})");
    }
    std::fs::rename(&part, dest).with_context(|| format!("переименование в {}", dest.display()))?;
    Ok(())
}

/// Join a manifest-relative path under `root`, refusing traversal. The manifest
/// is not trusted to dictate where we write (mirrors `xr-core::safe_dest`).
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        bail!("traversing path");
    }
    let mut out = root.to_path_buf();
    let mut components = 0usize;
    for comp in rel.split('/') {
        match comp {
            "" | "." => continue,
            ".." => bail!("traversing path"),
            other => {
                let p = Path::new(other);
                if p.is_absolute() || p.components().count() != 1 {
                    bail!("traversing path");
                }
                out.push(other);
                components += 1;
            }
        }
    }
    if components == 0 {
        bail!("empty path");
    }
    Ok(out)
}

/// Make a share name safe as a single directory component.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' || c == ':' { '_' } else { c })
        .collect();
    let s = s.trim().trim_matches('.').trim();
    if s.is_empty() { "share".to_string() } else { s.to_string() }
}

/// Percent-encode a share path for the URL, preserving `/` separators.
fn encode_path(p: &str) -> String {
    p.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_blocks_traversal() {
        let root = Path::new("/tmp/dl");
        assert_eq!(safe_join(root, "a.txt").unwrap(), root.join("a.txt"));
        assert_eq!(safe_join(root, "sub/b.bin").unwrap(), root.join("sub/b.bin"));
        for bad in ["../e", "..", "a/../../b", "/etc/passwd", "", ".", "x\\y", "x\0y"] {
            assert!(safe_join(root, bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize("Photos"), "Photos");
        assert_eq!(sanitize("a/b:c"), "a_b_c");
        assert_eq!(sanitize("  ..  "), "share");
    }
}
