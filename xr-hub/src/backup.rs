//! `xr-hub backup` и `xr-hub restore` (XR-224): состояние хаба уезжает архивом
//! на второй VPS, оттуда же разворачивается на чистой машине.
//!
//! В архив едут конфиг, ключ подписи и содержимое data_dir, кроме раздачи
//! дистрибутивов (`releases/`, `setup-dist/`): её воспроизводит машина сборки,
//! ежедневно гонять её десятки мегабайт незачем. Первым файлом лежит
//! `MANIFEST.json` с отпечатком публичной половины ключа подписи и
//! счётчиками пресетов, инвайтов и шар; приватной половины в манифесте нет.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ARCHIVE_PREFIX: &str = "xr-hub-backup-";
pub const ARCHIVE_SUFFIX: &str = ".tar.gz";

pub const MANIFEST_NAME: &str = "MANIFEST.json";
pub const CONFIG_NAME: &str = "config.toml";
pub const KEY_NAME: &str = "signing.key";

/// Каталоги data_dir, которые в архив не едут.
const SKIP_DIRS: [&str; 2] = ["releases", "setup-dist"];

/// Что лежит в архиве помимо файлов состояния.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub created_at: String,
    pub host: String,
    pub hub_version: String,
    /// Публичная половина ключа подписи, base64. Приватная сюда не пишется.
    pub signing_public_key: Option<String>,
    /// sha256 публичной половины, по нему сверяют корень доверия.
    pub signing_fingerprint: Option<String>,
    pub presets: usize,
    pub invites: usize,
    pub shares: usize,
}

/// Откуда собирается архив.
pub struct Sources {
    pub config: PathBuf,
    pub data_dir: PathBuf,
    pub signing_key: Option<PathBuf>,
}

/// Куда раскладывается архив. Пустые поля берутся из восстановленного конфига.
pub struct Targets {
    pub config: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub signing_key: Option<PathBuf>,
}

#[derive(Debug)]
pub struct RestoreReport {
    pub manifest: Manifest,
    pub config: PathBuf,
    pub data_dir: PathBuf,
    pub signing_key: Option<PathBuf>,
    /// Сколько файлов состояния (без конфига и ключа) разложено.
    pub data_files: usize,
    pub key_state: KeyState,
}

/// Что случилось с ключом подписи на месте назначения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Ключа там не было, восстановление на чистую машину.
    Fresh,
    /// Лежал тот же самый ключ, корень доверия не менялся.
    Same,
    /// Лежал другой ключ и заменён по --force.
    Replaced,
}

/// Отпечаток публичной половины: sha256 в нижнем регистре.
pub fn fingerprint(public_key: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(public_key);
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Публичная половина (base64) и её отпечаток по содержимому файла ключа.
pub fn key_identity(key_file: &str) -> Result<(String, String)> {
    let ctx = crate::signing::SigningContext::from_file(key_file)?;
    let public = ctx.verifying_key();
    let b64 = base64::engine::general_purpose::STANDARD.encode(public.as_bytes());
    Ok((b64, fingerprint(public.as_bytes())))
}

/// Имя архива с UTC-меткой: сортировка по имени совпадает с хронологией,
/// на ней стоит и ротация.
pub fn archive_name(stamp: &str) -> String {
    format!("{ARCHIVE_PREFIX}{stamp}{ARCHIVE_SUFFIX}")
}

/// Собрать архив в память. Размер состояния хаба это килобайты, поток на
/// диск тут не нужен, зато собранное можно проверить до записи.
pub fn build_archive(
    src: &Sources,
    created_at: &str,
    host: &str,
    mtime: u64,
) -> Result<(Vec<u8>, Manifest)> {
    let config = std::fs::read(&src.config)
        .with_context(|| format!("чтение конфига {}", src.config.display()))?;

    let (key_bytes, public_key, key_fingerprint) = match &src.signing_key {
        Some(path) => {
            let bytes = std::fs::read(path).with_context(|| {
                format!(
                    "чтение ключа подписи {}: он объявлен в конфиге, но недоступен",
                    path.display()
                )
            })?;
            let (b64, fp) = key_identity(&path.to_string_lossy())
                .with_context(|| format!("разбор ключа подписи {}", path.display()))?;
            (Some(bytes), Some(b64), Some(fp))
        }
        None => (None, None, None),
    };

    let skip: Vec<PathBuf> = [Some(&src.config), src.signing_key.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let data = collect_data(&src.data_dir, &skip)?;

    let manifest = Manifest {
        created_at: created_at.to_string(),
        host: host.to_string(),
        hub_version: env!("CARGO_PKG_VERSION").to_string(),
        signing_public_key: public_key,
        signing_fingerprint: key_fingerprint,
        presets: count_records(&data, "presets"),
        invites: count_records(&data, "invites"),
        shares: count_records(&data, "shares"),
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest).context("сериализация манифеста")?;

    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    append(&mut tar, MANIFEST_NAME, &manifest_json, mtime)?;
    append(&mut tar, CONFIG_NAME, &config, mtime)?;
    if let Some(bytes) = &key_bytes {
        append(&mut tar, KEY_NAME, bytes, mtime)?;
    }
    for (name, bytes) in &data {
        append(&mut tar, name, bytes, mtime)?;
    }
    let bytes = tar
        .into_inner()
        .context("закрытие tar")?
        .finish()
        .context("сжатие архива")?;

    Ok((bytes, manifest))
}

/// Разложить архив по целевым путям. Существующий ключ подписи с другим
/// отпечатком без `force` не трогается: молча подменённый корень доверия
/// хуже отсутствия бэкапа.
pub fn restore(archive: &[u8], targets: &Targets, force: bool, stamp: &str) -> Result<RestoreReport> {
    let entries = read_archive(archive)?;

    let manifest: Manifest = entries
        .get(MANIFEST_NAME)
        .context("в архиве нет MANIFEST.json, это не бэкап хаба")
        .and_then(|b| serde_json::from_slice(b).context("разбор MANIFEST.json"))?;
    let config = entries
        .get(CONFIG_NAME)
        .context("в архиве нет config.toml")?
        .clone();
    let config_text = String::from_utf8(config).context("config.toml не UTF-8")?;

    let from_config = parse_paths(&config_text)?;
    let data_dir = targets
        .data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(&from_config.0));
    let archived_key = entries.get(KEY_NAME);
    let signing_key = match (&targets.signing_key, archived_key) {
        (_, None) => None,
        (Some(path), _) => Some(path.clone()),
        // Восстановление в свой data_dir (проверка бэкапа, standby) не должно
        // целить ключом в чужой путь из конфига: кладём ключ рядом с данными.
        (None, _) if targets.data_dir.is_some() => Some(data_dir.join(KEY_NAME)),
        (None, _) => from_config.1.clone().map(PathBuf::from),
    };

    let mut key_state = KeyState::Fresh;
    if let (Some(target), Some(bytes)) = (&signing_key, archived_key) {
        key_state = check_key_collision(target, bytes, force)?;
    }

    // Пути в конфиге ведут на исходную машину; при восстановлении в другое
    // место правим две строки, чтобы комплект был самодостаточным.
    let config_text = if targets.data_dir.is_some() || targets.signing_key.is_some() {
        retarget_config(
            &config_text,
            &data_dir.to_string_lossy(),
            signing_key.as_ref().map(|p| p.to_string_lossy().into_owned()),
        )
    } else {
        config_text
    };

    backup_existing(&targets.config, stamp)?;
    write_secret(&targets.config, config_text.as_bytes())?;
    if let (Some(target), Some(bytes)) = (&signing_key, archived_key) {
        backup_existing(target, stamp)?;
        write_secret(target, bytes)?;
    }

    let mut data_files = 0;
    for (name, bytes) in &entries {
        if name == MANIFEST_NAME || name == CONFIG_NAME || name == KEY_NAME {
            continue;
        }
        write_secret(&data_dir.join(name), bytes)?;
        data_files += 1;
    }

    Ok(RestoreReport {
        manifest,
        config: targets.config.clone(),
        data_dir,
        signing_key,
        data_files,
        key_state,
    })
}

/// Оставить в каталоге `keep` свежих архивов, остальные удалить. `keep = 0`
/// отключает ротацию: пусть лучше накопится, чем удалится последнее.
pub fn prune(dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    if keep == 0 || !dir.exists() {
        return Ok(Vec::new());
    }
    let mut archives: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("чтение {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(ARCHIVE_PREFIX) && n.ends_with(ARCHIVE_SUFFIX))
        })
        .collect();
    archives.sort();

    let mut removed = Vec::new();
    let extra = archives.len().saturating_sub(keep);
    for path in archives.into_iter().take(extra) {
        std::fs::remove_file(&path).with_context(|| format!("удаление {}", path.display()))?;
        removed.push(path);
    }
    Ok(removed)
}

fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8], mtime: u64) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(mtime);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)
        .with_context(|| format!("запись {name} в архив"))?;
    Ok(())
}

/// Содержимое data_dir относительными путями, без раздачи дистрибутивов и
/// без файлов, которые уже уехали в архив отдельно (ключ лежит внутри
/// data_dir на живом хабе).
fn collect_data(data_dir: &Path, skip: &[PathBuf]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    if !data_dir.exists() {
        return Ok(out);
    }
    walk(data_dir, data_dir, skip, &mut out)?;
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    skip: &[PathBuf],
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("чтение {}", dir.display()))? {
        let path = entry?.path();
        let rel = path
            .strip_prefix(root)
            .expect("обход не выходит за data_dir")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            if SKIP_DIRS.contains(&rel.as_str()) {
                continue;
            }
            walk(root, &path, skip, out)?;
            continue;
        }
        if path.canonicalize().is_ok_and(|c| skip.contains(&c)) {
            continue;
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("чтение {}", path.display()))?;
        out.insert(rel, bytes);
    }
    Ok(())
}

fn count_records(data: &BTreeMap<String, Vec<u8>>, dir: &str) -> usize {
    let prefix = format!("{dir}/");
    data.keys()
        .filter(|k| k.starts_with(&prefix) && k.ends_with(".json"))
        .count()
}

fn read_archive(archive: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut out = BTreeMap::new();
    for entry in tar.entries().context("чтение архива")? {
        let mut entry = entry.context("чтение записи архива")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("имя записи архива")?.into_owned();
        let name = safe_name(&path)?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("чтение {name} из архива"))?;
        out.insert(name, bytes);
    }
    if out.is_empty() {
        bail!("архив пуст");
    }
    Ok(out)
}

/// Имя записи, которому можно доверить запись на диск: только относительный
/// путь вниз. Архив приезжает по сети, `..` в имени записал бы файл куда
/// угодно от рута.
fn safe_name(path: &Path) -> Result<String> {
    use std::path::Component;
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("подозрительный путь в архиве: {}", path.display()),
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

/// data_dir и путь ключа подписи из текста конфига.
fn parse_paths(config_text: &str) -> Result<(String, Option<String>)> {
    let value: toml::Value = config_text.parse().context("разбор config.toml из архива")?;
    let data_dir = value
        .get("server")
        .and_then(|s| s.get("data_dir"))
        .and_then(|d| d.as_str())
        .unwrap_or("/var/lib/xr-hub")
        .to_string();
    let key = value
        .get("signing")
        .and_then(|s| s.get("private_key"))
        .and_then(|k| k.as_str())
        .map(str::to_string);
    Ok((data_dir, key))
}

/// Переписать в конфиге пути состояния, не трогая остальной файл (правка
/// хирургическая по той же причине, что и в password_reset).
pub fn retarget_config(text: &str, data_dir: &str, signing_key: Option<String>) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if is_assignment(trimmed, "data_dir") {
            *line = format!("{indent}data_dir = \"{data_dir}\"");
        } else if let Some(key) = &signing_key {
            if is_assignment(trimmed, "private_key") {
                *line = format!("{indent}private_key = \"{key}\"");
            }
        }
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn is_assignment(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Чужой ключ на месте назначения без force это отказ; отсутствие ключа это
/// штатное восстановление на чистой машине.
fn check_key_collision(target: &Path, archived: &[u8], force: bool) -> Result<KeyState> {
    if !target.exists() {
        return Ok(KeyState::Fresh);
    }
    let existing = key_identity(&target.to_string_lossy())
        .with_context(|| format!("разбор существующего ключа {}", target.display()))?;
    let incoming = decode_key(archived)?;
    if existing.1 == incoming {
        return Ok(KeyState::Same);
    }
    if force {
        println!(
            "  внимание: {} заменяется ключом с другим отпечатком (--force)",
            target.display()
        );
        println!("    было:  {}", existing.1);
        println!("    стало: {incoming}");
        return Ok(KeyState::Replaced);
    }
    bail!(
        "в {} лежит другой ключ подписи (отпечаток {}, в архиве {}): восстановление \
         подменило бы корень доверия всего флота. Убедись, что архив от этого хаба, \
         и повтори с --force",
        target.display(),
        existing.1,
        incoming
    )
}

fn decode_key(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("ключ подписи в архиве не UTF-8")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .context("ключ подписи в архиве не base64")?;
    let key: [u8; 32] = raw
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("ключ подписи должен быть 32 байта, а не {}", v.len()))?;
    let public = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key();
    Ok(fingerprint(public.as_bytes()))
}

/// Каталог под архивы и состояние: 0700. На умолчательном umask (юнит
/// xr-hub его не задаёт) вышло бы 0755, и листинг бэкапов читал бы любой
/// локальный пользователь, хотя файлы внутри и лежат с 600. Приёмник
/// xr-hub-backup-receive.sh ставит для того же umask 077.
fn create_dir_secure(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("создание {}", dir.display()))
}

/// Копия затираемого секрета рядом: если архив оказался не тем, откатиться
/// есть куда (так же страхуется cert-receive.sh).
fn backup_existing(path: &Path, stamp: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bak.{stamp}"));
    let dest = path.with_file_name(name);
    std::fs::copy(path, &dest)
        .with_context(|| format!("копия {} перед заменой", path.display()))?;
    Ok(())
}

/// Атомарная запись секрета: сначала temp рядом, потом rename. Права 600
/// выставляются при создании, файл ни мгновения не лежит по umask.
pub fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        create_dir_secure(parent)?;
    }
    let tmp = path.with_extension("xr-backup.new");
    std::fs::remove_file(&tmp).ok();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("создание {}", tmp.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("запись {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("замена {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xr-hub-backup-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn gen_key() -> String {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
    }

    /// Живая раскладка хаба: конфиг рядом, ключ подписи внутри data_dir,
    /// записи состояния и раздача дистрибутивов.
    fn hub_layout(dir: &Path, key_b64: &str) -> Sources {
        let data_dir = dir.join("var/lib/xr-hub");
        for sub in ["presets", "invites", "shares", "releases", "setup-dist"] {
            std::fs::create_dir_all(data_dir.join(sub)).unwrap();
        }
        std::fs::write(data_dir.join("presets/russia.json"), r#"{"name":"russia"}"#).unwrap();
        std::fs::write(data_dir.join("presets/lite.json"), r#"{"name":"lite"}"#).unwrap();
        std::fs::write(data_dir.join("invites/tok1.json"), r#"{"token":"tok1"}"#).unwrap();
        std::fs::write(data_dir.join("shares/sh1.json"), r#"{"share_id":"sh1"}"#).unwrap();
        std::fs::write(data_dir.join("shares/sh2.json"), r#"{"share_id":"sh2"}"#).unwrap();
        std::fs::write(data_dir.join("releases/0.9.0.apk"), vec![0u8; 4096]).unwrap();
        std::fs::write(data_dir.join("setup-dist/xr-server-linux-x86_64"), b"bin").unwrap();

        let key_path = data_dir.join("signing.key");
        std::fs::write(&key_path, key_b64).unwrap();

        let config = dir.join("etc/xr-hub/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            format!(
                "[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"{}\"\n\n\
                 [[admin.users]]\nusername = \"admin\"\npassword_hash = \"$argon2id$x\"\n\n\
                 [signing]\nprivate_key = \"{}\"\n",
                data_dir.display(),
                key_path.display()
            ),
        )
        .unwrap();

        Sources {
            config,
            data_dir,
            signing_key: Some(key_path),
        }
    }

    fn names(archive: &[u8]) -> Vec<String> {
        read_archive(archive).unwrap().into_keys().collect()
    }

    #[test]
    fn archive_carries_state_without_the_dist_share() {
        let dir = tmpdir("build");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, manifest) =
            build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 1_753_000_000).unwrap();

        let names = names(&bytes);
        for expected in [
            MANIFEST_NAME,
            CONFIG_NAME,
            KEY_NAME,
            "presets/russia.json",
            "invites/tok1.json",
            "shares/sh1.json",
        ] {
            assert!(names.contains(&expected.to_string()), "нет {expected} в {names:?}");
        }
        assert!(
            !names.iter().any(|n| n.starts_with("releases/") || n.starts_with("setup-dist/")),
            "раздача дистрибутивов в бэкап не едет: {names:?}"
        );
        // Ключ лежит внутри data_dir, но в архиве он ровно один раз.
        assert_eq!(names.iter().filter(|n| n.ends_with("signing.key")).count(), 1);

        assert_eq!((manifest.presets, manifest.invites, manifest.shares), (2, 1, 2));
        assert_eq!(manifest.host, "hub-test");
        assert_eq!(manifest.created_at, "2026-07-31T04:17:00Z");
        assert_eq!(
            manifest.signing_fingerprint.as_deref(),
            Some(key_identity(&src.signing_key.unwrap().to_string_lossy()).unwrap().1.as_str())
        );

        // Приватная половина в манифест не попадает.
        let entries = read_archive(&bytes).unwrap();
        let manifest_text = String::from_utf8(entries[MANIFEST_NAME].clone()).unwrap();
        assert!(!manifest_text.contains(&key), "приватный ключ в манифесте");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_roundtrip_lands_state_with_600() {
        let dir = tmpdir("roundtrip");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, _) = build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 0).unwrap();

        let fresh = tmpdir("roundtrip-dest");
        let targets = Targets {
            config: fresh.join("etc/config.toml"),
            data_dir: Some(fresh.join("data")),
            signing_key: None,
        };
        let report = restore(&bytes, &targets, false, "1").unwrap();

        assert_eq!(report.data_files, 5);
        assert_eq!(report.key_state, KeyState::Fresh);
        assert_eq!(report.signing_key.as_deref(), Some(fresh.join("data/signing.key").as_path()));
        assert_eq!(
            std::fs::read_to_string(fresh.join("data/signing.key")).unwrap(),
            key
        );
        assert_eq!(
            std::fs::read_to_string(fresh.join("data/presets/russia.json")).unwrap(),
            r#"{"name":"russia"}"#
        );
        for path in [fresh.join("etc/config.toml"), fresh.join("data/signing.key")] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "секрет обязан лечь с 600: {}",
                path.display()
            );
        }
        for dir in [fresh.join("data"), fresh.join("data/presets"), fresh.join("etc")] {
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "каталог состояния не должен листаться посторонними: {}",
                dir.display()
            );
        }

        // Конфиг перенацелен: хаб с ним поднимется из нового каталога.
        let restored = std::fs::read_to_string(fresh.join("etc/config.toml")).unwrap();
        let value: toml::Value = restored.parse().unwrap();
        assert_eq!(
            value["server"]["data_dir"].as_str(),
            Some(fresh.join("data").to_string_lossy().as_ref())
        );
        assert_eq!(
            value["signing"]["private_key"].as_str(),
            Some(fresh.join("data/signing.key").to_string_lossy().as_ref())
        );
        assert_eq!(value["server"]["bind"].as_str(), Some("127.0.0.1:8080"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&fresh).ok();
    }

    #[test]
    fn restore_without_overrides_follows_the_config() {
        let dir = tmpdir("as-is");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, _) = build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 0).unwrap();
        let archived_config = read_archive(&bytes).unwrap()[CONFIG_NAME].clone();

        std::fs::remove_dir_all(&src.data_dir).unwrap();
        std::fs::remove_file(&src.config).unwrap();
        let targets = Targets {
            config: src.config.clone(),
            data_dir: None,
            signing_key: None,
        };
        let report = restore(&bytes, &targets, false, "1").unwrap();
        assert_eq!(report.data_dir, src.data_dir, "цели берутся из конфига архива");
        assert_eq!(report.signing_key, src.signing_key);
        assert!(src.data_dir.join("presets/russia.json").exists());
        assert_eq!(
            std::fs::read_to_string(&src.config).unwrap(),
            String::from_utf8(archived_config).unwrap(),
            "без переопределений конфиг ложится как был"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_refuses_to_swap_the_root_of_trust() {
        let dir = tmpdir("collision");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, _) = build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 0).unwrap();

        let dest = tmpdir("collision-dest");
        let data_dir = dest.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let alien = gen_key();
        std::fs::write(data_dir.join("signing.key"), &alien).unwrap();

        let targets = Targets {
            config: dest.join("config.toml"),
            data_dir: Some(data_dir.clone()),
            signing_key: None,
        };
        let err = restore(&bytes, &targets, false, "1").unwrap_err().to_string();
        assert!(err.contains("другой ключ подписи"), "невнятный отказ: {err}");
        assert_eq!(
            std::fs::read_to_string(data_dir.join("signing.key")).unwrap(),
            alien,
            "отказ обязан оставить ключ на месте"
        );

        let report = restore(&bytes, &targets, true, "42").unwrap();
        assert_eq!(report.key_state, KeyState::Replaced);
        assert_eq!(std::fs::read_to_string(data_dir.join("signing.key")).unwrap(), key);
        assert_eq!(
            std::fs::read_to_string(data_dir.join("signing.key.bak.42")).unwrap(),
            alien,
            "затёртый ключ обязан остаться копией рядом"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn restore_over_the_same_key_needs_no_force() {
        let dir = tmpdir("same-key");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, _) = build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 0).unwrap();

        let dest = tmpdir("same-key-dest");
        let data_dir = dest.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("signing.key"), &key).unwrap();
        let targets = Targets {
            config: dest.join("config.toml"),
            data_dir: Some(data_dir),
            signing_key: None,
        };
        let report = restore(&bytes, &targets, false, "1").unwrap();
        assert_eq!(report.key_state, KeyState::Same, "свой же ключ не повод отказывать");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn archive_entries_may_not_escape_the_target() {
        // Архив приезжает по сети, `..` в имени записал бы файл куда угодно
        // от рута. Builder такое имя не пропускает, поэтому собираем блоки
        // руками, как это сделал бы отправитель с чужим tar.
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o600);
        header.set_mtime(0);
        header.as_old_mut().name[..12].copy_from_slice(b"../evil.toml");
        header.set_cksum();
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(b"pwn");
        raw.resize(512 * 4, 0);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let bytes = gz.finish().unwrap();

        let err = read_archive(&bytes).unwrap_err().to_string();
        assert!(err.contains("подозрительный путь"), "прошёл побег из каталога: {err}");
    }

    #[test]
    fn manifest_is_required() {
        let dir = tmpdir("no-manifest");
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        append(&mut tar, CONFIG_NAME, b"[server]\n", 0).unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        let targets = Targets {
            config: dir.join("config.toml"),
            data_dir: Some(dir.join("data")),
            signing_key: None,
        };
        let err = restore(&bytes, &targets, false, "1").unwrap_err().to_string();
        assert!(err.contains("MANIFEST.json"), "невнятный отказ: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_archive_is_rejected_and_nothing_is_laid_out() {
        let dir = tmpdir("broken");
        let key = gen_key();
        let src = hub_layout(&dir, &key);
        let (bytes, _) = build_archive(&src, "2026-07-31T04:17:00Z", "hub-test", 0).unwrap();

        let dest = tmpdir("broken-dest");
        let targets = Targets {
            config: dest.join("config.toml"),
            data_dir: Some(dest.join("data")),
            signing_key: None,
        };

        // Оборванная передача: половина gzip-потока.
        let cut = &bytes[..bytes.len() / 2];
        assert!(read_archive(cut).is_err(), "усечённый архив не архив");
        assert!(restore(cut, &targets, false, "1").is_err());
        assert!(
            !targets.config.exists() && !dest.join("data").exists(),
            "разбор падает до записи, полусостояния на диске не остаётся"
        );

        // Мусор вместо архива и пустой поток.
        assert!(read_archive(b"not an archive at all").is_err());
        assert!(read_archive(&[]).is_err(), "пустой поток не архив");

        // Целый gzip, но внутри не tar.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all("просто текст, а не блоки tar".as_bytes()).unwrap();
        assert!(read_archive(&gz.finish().unwrap()).is_err());

        // Битый MANIFEST.json: архив читается, а разбор манифеста нет.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append(&mut tar, MANIFEST_NAME, "{ не json".as_bytes(), 0).unwrap();
            append(&mut tar, CONFIG_NAME, b"[server]\n", 0).unwrap();
            tar.finish().unwrap();
        }
        let err = restore(&gz.finish().unwrap(), &targets, false, "1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("MANIFEST.json"), "невнятный отказ: {err}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn prune_keeps_the_freshest() {
        let dir = tmpdir("prune");
        let stamps = [
            "20260728T041700Z",
            "20260729T041700Z",
            "20260730T041700Z",
            "20260731T041700Z",
        ];
        for stamp in stamps {
            std::fs::write(dir.join(archive_name(stamp)), b"x").unwrap();
        }
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();

        let removed = prune(&dir, 2).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!dir.join(archive_name(stamps[0])).exists());
        assert!(!dir.join(archive_name(stamps[1])).exists());
        assert!(dir.join(archive_name(stamps[2])).exists());
        assert!(dir.join(archive_name(stamps[3])).exists());
        assert!(dir.join("readme.txt").exists(), "чужие файлы ротация не трогает");

        assert!(prune(&dir, 2).unwrap().is_empty(), "повторная ротация вхолостую");
        assert!(prune(&dir, 0).unwrap().is_empty(), "keep=0 отключает ротацию");
        assert!(dir.join(archive_name(stamps[2])).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retarget_touches_only_the_two_paths() {
        let text = "# конфиг\n[server]\nbind = \"127.0.0.1:8080\"\ndata_dir = \"/var/lib/xr-hub\"\n\n\
                    [signing]\n# private_key = \"/decoy\"\nprivate_key = \"/var/lib/xr-hub/signing.key\"\n";
        let out = retarget_config(text, "/tmp/x/data", Some("/tmp/x/data/signing.key".into()));
        let value: toml::Value = out.parse().unwrap();
        assert_eq!(value["server"]["data_dir"].as_str(), Some("/tmp/x/data"));
        assert_eq!(
            value["signing"]["private_key"].as_str(),
            Some("/tmp/x/data/signing.key")
        );
        assert_eq!(value["server"]["bind"].as_str(), Some("127.0.0.1:8080"));
        assert!(out.starts_with("# конфиг"), "комментарии остаются на месте");
        assert!(out.contains("# private_key = \"/decoy\""));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn fingerprint_belongs_to_the_public_half() {
        let dir = tmpdir("fp");
        let path = dir.join("signing.key");
        let private = gen_key();
        std::fs::write(&path, &private).unwrap();
        let (public, fp) = key_identity(&path.to_string_lossy()).unwrap();

        assert_ne!(public, private, "в манифест идёт публичная половина");
        let raw = base64::engine::general_purpose::STANDARD.decode(&public).unwrap();
        assert_eq!(raw.len(), 32);
        assert_eq!(fp, fingerprint(&raw));
        assert_eq!(fp.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }
}
