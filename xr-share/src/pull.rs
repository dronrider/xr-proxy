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
use xr_proto::share::{
    parse_agent_pubkey, verify_share_manifest, ShareManifest, MANIFEST_SIGNED_AT_HEADER,
    MANIFEST_SIG_HEADER,
};

pub(crate) const HUB_DEFAULT: &str = "https://xr-hub.zoobr.top";

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
    /// Non-interactive selection: a manifest path per occurrence (repeatable).
    /// A value that matches no path exactly is treated as a comma-separated
    /// list (the legacy form), so names containing commas are selectable too.
    #[arg(long)]
    pub select: Vec<String>,
    /// Reach agents over https (default http; the distributed agent serves HTTP).
    #[arg(long)]
    pub https: bool,
    /// Limit to one share by its share_id or name.
    #[arg(long)]
    pub share: Option<String>,
}

/// One share on an invite, as the hub returns it (`GET /invite/{t}/shares`).
/// Shared by the `pull` receiver and the `push`/`rm` harness (LLD-28).
#[derive(Deserialize)]
pub(crate) struct InviteShareDto {
    pub(crate) share_id: String,
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) port: u16,
    /// Base64 identity key of the agent; the manifest signature is verified
    /// against it (XR-046). Tolerated absent for an older hub, then the
    /// manifest is accepted unverified.
    #[serde(default)]
    pub(crate) agent_pubkey: String,
    pub(crate) token: String,
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
        let manifest = fetch_manifest_verified(&format!("{base}/manifest"), s)
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

/// Which manifest paths to download: `--all`, `--select`, or interactive.
fn choose(manifest: &ShareManifest, args: &PullArgs, share_name: &str) -> Result<HashSet<String>> {
    if args.all {
        return Ok(manifest.entries.iter().map(|e| e.path.clone()).collect());
    }
    if !args.select.is_empty() {
        let set = select_paths(&args.select, manifest);
        // A piece that matched nothing is either a typo or another share of the
        // same invite; say so instead of silently downloading zero files
        // (XR-106: the old behaviour was a bare "Готово: 0 файлов").
        for miss in set.iter().filter(|p| !manifest.entries.iter().any(|e| &e.path == *p)) {
            eprintln!("[{share_name}] предупреждение: не найдено в шаре: {miss}");
        }
        return Ok(set);
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

/// Resolve `--select` values against the manifest. A value equal to an entry
/// path is taken whole, even with commas inside (XR-106: a comma is a legal
/// filename character); only otherwise it is split as the legacy
/// comma-separated list. Unmatched pieces are kept, the caller reports them.
fn select_paths(values: &[String], manifest: &ShareManifest) -> HashSet<String> {
    let mut set = HashSet::new();
    for v in values {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if manifest.entries.iter().any(|e| e.path == v) {
            set.insert(v.to_string());
        } else {
            set.extend(
                v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            );
        }
    }
    set
}

/// GET the share manifest, verifying the agent's signature headers against the
/// `agent_pubkey` pinned in the grant (XR-046). The data-path is plain HTTP, so
/// without this check a MITM could rewrite a file and its hash together and the
/// SHA-256 verification below would confirm the substitution. Fail-closed when
/// a key is pinned: a missing signature (old agent or stripped headers) is a
/// refusal with a pointer at updating the agent.
pub(crate) fn fetch_manifest_verified(url: &str, share: &InviteShareDto) -> Result<ShareManifest> {
    let resp = match ureq::get(url)
        .set("Authorization", &format!("Bearer {}", share.token))
        .timeout(Duration::from_secs(30))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            bail!("HTTP {code}: {}", r.into_string().unwrap_or_default())
        }
        Err(e) => bail!("сеть: {e}"),
    };
    let sig = resp.header(MANIFEST_SIG_HEADER).map(str::to_string);
    let signed_at = resp
        .header(MANIFEST_SIGNED_AT_HEADER)
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.into_string().context("чтение ответа")?;

    if !share.agent_pubkey.is_empty() {
        let key = parse_agent_pubkey(&share.agent_pubkey)
            .map_err(|e| anyhow::anyhow!("agent_pubkey из гранта: {e}"))?;
        let (Some(sig), Some(signed_at)) = (sig, signed_at) else {
            bail!("агент не подписал манифест: обнови xr-share на стороне агента");
        };
        verify_share_manifest(&sig, &key, &share.share_id, signed_at, body.as_bytes())
            .map_err(|e| anyhow::anyhow!("подпись манифеста не сошлась ({e}): возможна подмена по пути"))?;
    }
    serde_json::from_str(&body).context("разбор JSON манифеста")
}

/// GET a JSON body, optionally with a bearer token. Maps a 4xx/5xx to a clear
/// error instead of a panic.
pub(crate) fn get_json<T: DeserializeOwned>(url: &str, token: Option<&str>) -> Result<T> {
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
pub(crate) fn encode_path(p: &str) -> String {
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
    use xr_proto::share::ShareManifestEntry;

    fn manifest_of(paths: &[&str]) -> ShareManifest {
        ShareManifest {
            entries: paths
                .iter()
                .map(|p| ShareManifestEntry {
                    path: p.to_string(),
                    size: 1,
                    mtime: 0,
                    sha256: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn select_keeps_comma_name_when_exact() {
        // XR-106: a filename containing a comma must be selectable whole; the
        // pre-fix code always split on commas and matched nothing.
        let m = manifest_of(&["a [5 2026, RUS].torrent", "b.txt"]);
        let set = select_paths(&["a [5 2026, RUS].torrent".to_string()], &m);
        assert_eq!(set.len(), 1);
        assert!(set.contains("a [5 2026, RUS].torrent"));
    }

    #[test]
    fn select_legacy_comma_list_still_splits() {
        // The documented list form keeps working when nothing matches exactly.
        let m = manifest_of(&["a.txt", "b.txt"]);
        let set = select_paths(&["a.txt, b.txt".to_string()], &m);
        assert_eq!(set.len(), 2);
        assert!(set.contains("a.txt") && set.contains("b.txt"));
    }

    #[test]
    fn select_repeats_and_skips_empty() {
        let m = manifest_of(&["a.txt", "b, c.txt"]);
        let set = select_paths(&["a.txt".to_string(), "b, c.txt".to_string(), " ".to_string()], &m);
        assert_eq!(set.len(), 2);
        assert!(set.contains("b, c.txt"));
        // A typo stays in the set (the caller warns about it, and the download
        // filter drops it), it must not silently vanish.
        let set = select_paths(&["nope.txt".to_string()], &m);
        assert!(set.contains("nope.txt"));
    }

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
