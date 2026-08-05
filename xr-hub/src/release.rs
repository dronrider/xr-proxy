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

/// Arguments for [`verify_release`], mirroring the CLI flags.
#[derive(Default)]
pub struct VerifyReleaseArgs {
    pub signed: Option<String>,
    pub manifest: Option<String>,
    pub sig: Option<String>,
    pub pubkey: Option<String>,
    pub key: Option<String>,
    pub expect_version: Option<String>,
    pub expect_version_code: Option<u64>,
    pub expect_sha256: Option<String>,
}

/// Check a release manifest the way the phone checks it: detached ed25519
/// signature over the exact manifest bytes, verified with the pinned public
/// key (XR-109).
///
/// Берёт либо ответ `/api/v1/app/latest` целиком (`--signed`, файл или `-`),
/// либо пару файлов из каталога подписи (`--manifest`/`--sig`). Ключ задаётся
/// публичной половиной (`--pubkey`) или приватным файлом (`--key`), из
/// которого публичная выводится: так один прогон отвечает и на вопрос
/// «сойдётся ли подпись», и на вопрос «тем ли ключом мы собираемся
/// подписывать» (подпись живого манифеста своим ключом это единственная
/// доступная проверка, что ключ не подменён).
pub fn verify_release(args: VerifyReleaseArgs) -> Result<()> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let (manifest_json, sig_b64) = match (&args.signed, &args.manifest, &args.sig) {
        (Some(path), None, None) => {
            let raw = read_source(path)?;
            let signed: xr_proto::app_update::SignedManifest =
                serde_json::from_str(&raw).context("parsing signed manifest JSON")?;
            (signed.manifest, signed.signature)
        }
        (None, Some(manifest), Some(sig)) => (read_source(manifest)?, read_source(sig)?),
        _ => anyhow::bail!("provide either --signed <file|-> or both --manifest and --sig"),
    };

    let pub_b64 = match (&args.pubkey, &args.key) {
        (Some(p), _) => p.trim().to_string(),
        (None, Some(key)) => {
            let ctx = SigningContext::from_file(key)
                .with_context(|| format!("loading release key from {key}"))?;
            base64::engine::general_purpose::STANDARD.encode(ctx.verifying_key().as_bytes())
        }
        (None, None) => anyhow::bail!("provide --pubkey <base64> or --key <private key file>"),
    };

    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(pub_b64.trim())
        .context("decoding release public key base64")?
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("public key must be 32 bytes, got {}", v.len()))?;
    let verifying = VerifyingKey::from_bytes(&key_bytes).context("bad release public key")?;

    let sig_bytes: [u8; 64] = base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .context("decoding signature base64")?
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("signature must be 64 bytes, got {}", v.len()))?;
    verifying
        .verify(manifest_json.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow::anyhow!("signature does not match this key"))?;

    let manifest: AppManifest =
        serde_json::from_str(&manifest_json).context("parsing manifest JSON")?;

    if let Some(want) = &args.expect_version {
        if &manifest.version_name != want {
            anyhow::bail!(
                "version_name is {}, expected {want}",
                manifest.version_name
            );
        }
    }
    if let Some(want) = args.expect_version_code {
        if manifest.version_code != want {
            anyhow::bail!("version_code is {}, expected {want}", manifest.version_code);
        }
    }
    if let Some(want) = &args.expect_sha256 {
        if !manifest.apk_sha256.eq_ignore_ascii_case(want.trim()) {
            anyhow::bail!("apk_sha256 is {}, expected {want}", manifest.apk_sha256);
        }
    }

    println!(
        "OK {} (code {}), sha256 {}, key {}",
        manifest.version_name, manifest.version_code, manifest.apk_sha256, pub_b64
    );
    Ok(())
}

/// `-` это stdin, всё остальное путь к файлу.
fn read_source(path: &str) -> Result<String> {
    if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
    }
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

/// Проверка релиза (XR-109): подпись сходится ровно на тех байтах, что уехали
/// на хаб, а мимо ключа или мимо ожидаемой версии прогон падает.
#[cfg(test)]
mod verify_tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    struct Signed {
        dir: tempfile::TempDir,
        key_file: String,
        pubkey: String,
    }

    /// Подписанный релиз 0.5.0/code5 в отдельном каталоге: приватный ключ,
    /// manifest.json и manifest.sig ровно теми же путями, что пишет
    /// `sign-release`.
    fn signed_release() -> Signed {
        let dir = tempfile::tempdir().expect("временный каталог");
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let priv_b64 = base64::engine::general_purpose::STANDARD.encode(signing.to_bytes());
        let pubkey =
            base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes());
        let key_file = dir.path().join("release.key");
        std::fs::write(&key_file, &priv_b64).expect("ключ");

        let apk = dir.path().join("app-release.apk");
        std::fs::write(&apk, b"not really an apk").expect("apk");
        sign_release(SignReleaseArgs {
            apk: apk.display().to_string(),
            version: "0.5.0".into(),
            version_code: 5,
            key: key_file.display().to_string(),
            base_url: Some("https://hub.example".into()),
            apk_url: None,
            min_sdk: 29,
            notes: String::new(),
            released_at: "2026-08-05".into(),
            out: Some(dir.path().display().to_string()),
        })
        .expect("подпись релиза");

        Signed {
            key_file: key_file.display().to_string(),
            pubkey,
            dir,
        }
    }

    impl Signed {
        fn at(&self, name: &str) -> String {
            self.dir.path().join(name).display().to_string()
        }

        /// Ответ `/api/v1/app/latest`: манифест строкой плюс подпись.
        fn served(&self) -> String {
            let manifest = std::fs::read_to_string(self.at("manifest.json")).unwrap();
            let signature = std::fs::read_to_string(self.at("manifest.sig")).unwrap();
            serde_json::json!({ "manifest": manifest, "signature": signature }).to_string()
        }

        fn write_served(&self, name: &str, body: &str) -> String {
            let path = self.dir.path().join(name);
            std::fs::write(&path, body).expect("ответ хаба");
            path.display().to_string()
        }

        fn args(&self) -> VerifyReleaseArgs {
            VerifyReleaseArgs {
                manifest: Some(self.at("manifest.json")),
                sig: Some(self.at("manifest.sig")),
                pubkey: Some(self.pubkey.clone()),
                ..Default::default()
            }
        }
    }

    #[test]
    fn local_pair_verifies_with_its_pubkey() {
        let r = signed_release();
        verify_release(r.args()).expect("подпись своей парой файлов");
    }

    /// Публичная половина выводится из приватного ключа: так префлайт узнаёт,
    /// что подписывать собрались тем же ключом, каким подписан живой манифест.
    #[test]
    fn private_key_gives_the_same_pubkey() {
        let r = signed_release();
        verify_release(VerifyReleaseArgs {
            pubkey: None,
            key: Some(r.key_file.clone()),
            ..r.args()
        })
        .expect("подпись по приватному ключу");
    }

    #[test]
    fn served_answer_verifies() {
        let r = signed_release();
        let served = r.write_served("latest.json", &r.served());
        verify_release(VerifyReleaseArgs {
            signed: Some(served),
            manifest: None,
            sig: None,
            expect_version: Some("0.5.0".into()),
            expect_version_code: Some(5),
            ..r.args()
        })
        .expect("ответ хаба");
    }

    /// Хаб отдал манифест с подкрученной версией при старой подписи: ровно то,
    /// от чего проверка и заведена.
    #[test]
    fn tampered_manifest_fails() {
        let r = signed_release();
        let body = r.served().replace("0.5.0", "0.6.0");
        let served = r.write_served("tampered.json", &body);
        let err = verify_release(VerifyReleaseArgs {
            signed: Some(served),
            manifest: None,
            sig: None,
            ..r.args()
        })
        .expect_err("подделанный манифест");
        assert!(err.to_string().contains("signature does not match"), "{err}");
    }

    #[test]
    fn wrong_pubkey_fails() {
        let r = signed_release();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let other_pub =
            base64::engine::general_purpose::STANDARD.encode(other.verifying_key().as_bytes());
        let err = verify_release(VerifyReleaseArgs {
            pubkey: Some(other_pub),
            ..r.args()
        })
        .expect_err("чужой ключ");
        assert!(err.to_string().contains("signature does not match"), "{err}");
    }

    /// Заливка прошла, но хаб отдаёт прежний релиз: ловится ожидаемым кодом.
    #[test]
    fn version_code_mismatch_fails() {
        let r = signed_release();
        let err = verify_release(VerifyReleaseArgs {
            expect_version_code: Some(6),
            ..r.args()
        })
        .expect_err("не та версия");
        assert!(err.to_string().contains("version_code is 5"), "{err}");
    }

    #[test]
    fn sha256_mismatch_fails() {
        let r = signed_release();
        let err = verify_release(VerifyReleaseArgs {
            expect_sha256: Some("deadbeef".into()),
            ..r.args()
        })
        .expect_err("не тот apk");
        assert!(err.to_string().contains("apk_sha256 is"), "{err}");
    }

    #[test]
    fn source_and_key_are_required() {
        let r = signed_release();
        let no_source = verify_release(VerifyReleaseArgs {
            pubkey: Some(r.pubkey.clone()),
            ..Default::default()
        })
        .expect_err("без источника манифеста");
        assert!(no_source.to_string().contains("--signed"), "{no_source}");

        let no_key = verify_release(VerifyReleaseArgs {
            pubkey: None,
            ..r.args()
        })
        .expect_err("без ключа");
        assert!(no_key.to_string().contains("--pubkey"), "{no_key}");
    }
}
