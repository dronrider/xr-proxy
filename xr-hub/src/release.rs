//! `xr-hub sign-release` / `gen-release-key` — offline release signing
//! (LLD-12 §3.4, §4).
//!
//! These run wherever the **private release key** lives — the owner's laptop,
//! NOT the VPS. `sign-release` computes the APK's SHA-256, builds the version
//! manifest, signs the manifest's raw bytes with the release key, and writes
//! `manifest.json` + `manifest.sig` (plus a `<version>.apk` copy) into an
//! output directory. The owner then uploads those three files to the hub's
//! `releases/` directory; the hub serves them verbatim (it has no key).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::Signer;
use sha2::{Digest, Sha256};
use xr_proto::app_update::AppManifest;

use crate::signing::SigningContext;

/// Arguments for [`sign_release`], mirroring the CLI flags.
pub struct SignReleaseArgs {
    pub apk: String,
    pub version: String,
    pub version_code: u64,
    pub key: String,
    pub base_url: Option<String>,
    pub apk_url: Option<String>,
    pub min_sdk: u32,
    pub notes: String,
    pub released_at: String,
    pub out: Option<String>,
}

/// Build, sign, and stage a release. Writes `manifest.json`, `manifest.sig`,
/// and `<version>.apk` into the output directory (default: the APK's dir).
pub fn sign_release(args: SignReleaseArgs) -> Result<()> {
    let apk_path = Path::new(&args.apk);
    if !apk_path.is_file() {
        anyhow::bail!("APK not found: {}", apk_path.display());
    }

    // SHA-256 + size over the exact bytes we will serve.
    let apk_sha256 = sha256_file(apk_path)
        .with_context(|| format!("hashing {}", apk_path.display()))?;
    let size_bytes = std::fs::metadata(apk_path)?.len();

    // Download URL: explicit override, else derived from --base-url.
    let apk_url = match (&args.apk_url, &args.base_url) {
        (Some(u), _) => u.clone(),
        (None, Some(base)) => format!(
            "{}/api/v1/app/download/{}",
            base.trim_end_matches('/'),
            args.version
        ),
        (None, None) => {
            anyhow::bail!("provide --apk-url or --base-url to set the download link")
        }
    };

    let manifest = AppManifest {
        version_code: args.version_code,
        version_name: args.version.clone(),
        min_sdk: args.min_sdk,
        apk_url,
        apk_sha256,
        size_bytes,
        release_notes: args.notes,
        released_at: args.released_at,
    };

    // The signed bytes ARE the served bytes — pretty JSON for the owner's
    // eyes, but written verbatim and signed verbatim (the client verifies the
    // detached signature over exactly this string).
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("serializing manifest")?;

    let ctx = SigningContext::from_file(&args.key)
        .with_context(|| format!("loading release key from {}", args.key))?;
    let signature = ctx.signing_key.sign(manifest_json.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());

    let out_dir: PathBuf = match &args.out {
        Some(d) => PathBuf::from(d),
        None => apk_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating output dir {}", out_dir.display()))?;

    atomic_write(&out_dir.join("manifest.json"), manifest_json.as_bytes())?;
    atomic_write(&out_dir.join("manifest.sig"), sig_b64.as_bytes())?;

    // Stage the APK under the name the download endpoint serves.
    let target_apk = out_dir.join(format!("{}.apk", args.version));
    if target_apk != apk_path {
        std::fs::copy(apk_path, &target_apk)
            .with_context(|| format!("copying APK to {}", target_apk.display()))?;
    }

    println!("Release {} (code {}) signed.", args.version, args.version_code);
    println!("  {}", out_dir.join("manifest.json").display());
    println!("  {}", out_dir.join("manifest.sig").display());
    println!("  {}", target_apk.display());
    println!();
    println!("Upload these three files to the hub's releases/ dir, then they are");
    println!("served at /api/v1/app/latest and /api/v1/app/download/{}.", args.version);
    Ok(())
}

/// Generate a fresh ed25519 release keypair and print both halves (base64).
/// Keep the private key OFFLINE; put the public key in the app build
/// (`gradle.properties` → `xrReleasePublicKey`).
pub fn gen_release_key() {
    use ed25519_dalek::SigningKey;
    let signing = SigningKey::generate(&mut rand::thread_rng());
    let priv_b64 = base64::engine::general_purpose::STANDARD.encode(signing.to_bytes());
    let pub_b64 =
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes());
    println!("# ed25519 release keypair (LLD-12). KEEP THE PRIVATE KEY OFFLINE.");
    println!("# Private — pass to `xr-hub sign-release --key <file>` (store in a file, chmod 600):");
    println!("{priv_b64}");
    println!("# Public — set as gradle property xrReleasePublicKey (compiled into the app):");
    println!("{pub_b64}");
}

/// Streaming SHA-256 → lowercase hex (matches `xr_core::update`).
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
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
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(hex, "{:02x}", b);
    }
    Ok(hex)
}

/// Atomic write: temp file in the same dir + rename.
fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.persist(target)
        .with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}
