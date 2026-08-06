use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use xr_proto::preset::{Invite, Preset};
use xr_proto::share::{ExposeRecord, ShareRecord};

/// Load all presets from `<data_dir>/presets/`.
pub fn load_all_presets(data_dir: &Path) -> Result<HashMap<String, Preset>> {
    let dir = data_dir.join("presets");
    let mut map = HashMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    for entry in std::fs::read_dir(&dir).context("reading presets dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let preset: Preset = serde_json::from_str(&data)
            .with_context(|| format!("parsing {}", path.display()))?;
        map.insert(preset.name.clone(), preset);
    }
    Ok(map)
}

/// Save preset atomically via temp file + rename.
pub fn save_preset(data_dir: &Path, preset: &Preset) -> Result<()> {
    let dir = data_dir.join("presets");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(format!("{}.json", preset.name));
    let data = serde_json::to_string_pretty(preset)?;
    atomic_write(&target, data.as_bytes())
}

/// Delete preset file.
pub fn delete_preset_file(data_dir: &Path, name: &str) -> Result<()> {
    let path = data_dir.join("presets").join(format!("{name}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Load all invites from `<data_dir>/invites/`.
pub fn load_all_invites(data_dir: &Path) -> Result<HashMap<String, Invite>> {
    let dir = data_dir.join("invites");
    let mut map = HashMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    for entry in std::fs::read_dir(&dir).context("reading invites dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let invite: Invite = serde_json::from_str(&data)
            .with_context(|| format!("parsing {}", path.display()))?;
        map.insert(invite.token.clone(), invite);
    }
    Ok(map)
}

/// Save invite atomically.
pub fn save_invite(data_dir: &Path, invite: &Invite) -> Result<()> {
    let dir = data_dir.join("invites");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(format!("{}.json", invite.token));
    let data = serde_json::to_string_pretty(invite)?;
    atomic_write(&target, data.as_bytes())
}

/// Delete invite file.
#[allow(dead_code)]
pub fn delete_invite_file(data_dir: &Path, token: &str) -> Result<()> {
    let path = data_dir.join("invites").join(format!("{token}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Load all shares from `<data_dir>/shares/` (LLD-19). The hub stores **only**
/// the address+identity record — never any file bytes or listing.
pub fn load_all_shares(data_dir: &Path) -> Result<HashMap<String, ShareRecord>> {
    let dir = data_dir.join("shares");
    let mut map = HashMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    for entry in std::fs::read_dir(&dir).context("reading shares dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let share: ShareRecord = serde_json::from_str(&data)
            .with_context(|| format!("parsing {}", path.display()))?;
        map.insert(share.share_id.clone(), share);
    }
    Ok(map)
}

/// Save share atomically via temp file + rename.
pub fn save_share(data_dir: &Path, share: &ShareRecord) -> Result<()> {
    let dir = data_dir.join("shares");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(format!("{}.json", share.share_id));
    let data = serde_json::to_string_pretty(share)?;
    atomic_write(&target, data.as_bytes())
}

/// Delete a share's record file.
pub fn delete_share_file(data_dir: &Path, share_id: &str) -> Result<()> {
    let path = data_dir.join("shares").join(format!("{share_id}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Прочитать публикации из `<data_dir>/expose/` (LLD-38 п. 2.1). Как и у шар,
/// хаб держит только адресную запись: имя, агент, момент заведения. Апстрима в
/// ней нет и быть не может, он остаётся в конфиге агента.
pub fn load_all_exposes(data_dir: &Path) -> Result<HashMap<String, ExposeRecord>> {
    let dir = data_dir.join("expose");
    let mut map = HashMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    for entry in std::fs::read_dir(&dir).context("reading expose dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let rec: ExposeRecord = serde_json::from_str(&data)
            .with_context(|| format!("parsing {}", path.display()))?;
        map.insert(rec.name.clone(), rec);
    }
    Ok(map)
}

/// Сохранить публикацию атомарно. Имя уже проверено как DNS-метка, поэтому в
/// имя файла оно едет как есть: ни слеша, ни точки в метке не бывает.
pub fn save_expose(data_dir: &Path, rec: &ExposeRecord) -> Result<()> {
    let dir = data_dir.join("expose");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(format!("{}.json", rec.name));
    let data = serde_json::to_string_pretty(rec)?;
    atomic_write(&target, data.as_bytes())
}

/// Убрать файл публикации.
pub fn delete_expose_file(data_dir: &Path, name: &str) -> Result<()> {
    let path = data_dir.join("expose").join(format!("{name}.json"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Atomic write: write to temp file in same dir, then rename. Каждая io-ошибка
/// несёт путь: разбирать отказ записи по голому «Not a directory (os error 20)»
/// в логе не по чему, а такой отказ гасит потребление инвайта (XR-211).
fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = target.parent().unwrap();
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(data)
        .with_context(|| format!("writing {}", target.display()))?;
    tmp.persist(target)
        .with_context(|| format!("renaming into {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(token: &str) -> Invite {
        Invite {
            token: token.into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            expires_at: "2099-01-01T00:00:00+00:00".into(),
            consumed_at: None,
            claimed_by_ip: None,
            claim_id: None,
            one_time: true,
            comment: String::new(),
            payload: xr_proto::preset::InvitePayload {
                server_address: "203.0.113.10".into(),
                server_port: 8443,
                obfuscation_key: String::new(),
                modifier: "positional_xor_rotate".into(),
                salt: 0,
                preset: String::new(),
                hub_url: String::new(),
                servers: Vec::new(),
            },
            share_ids: Vec::new(),
            write_share_ids: Vec::new(),
        }
    }

    // XR-211, замечание ревью: неудачную запись инвайта хаб пишет в лог, и по
    // ней разбирают отказ на живой машине. Без контекста туда уходило голое
    // «Not a directory (os error 20)», по которому не понять ни какой каталог
    // не открылся, ни у какого инвайта сгорело потребление.
    #[test]
    fn failed_save_names_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::write(&data_dir, b"not a directory").unwrap();

        let err = save_invite(&data_dir, &invite("token-1")).expect_err("запись обязана упасть");
        let text = format!("{err:#}");
        assert!(
            text.contains(data_dir.join("invites").to_str().unwrap()),
            "в ошибке нет пути каталога инвайтов: {text}"
        );
    }

    // Тот же разбор нужен и когда каталог на месте, а спотыкается запись файла.
    #[test]
    fn failed_write_names_the_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let invites = dir.path().join("invites");
        // На месте файла инвайта каталог: временный файл рядом создаётся и
        // пишется, а переименование поверх каталога не проходит.
        std::fs::create_dir_all(invites.join("token-1.json")).unwrap();

        let err = save_invite(dir.path(), &invite("token-1")).expect_err("запись обязана упасть");
        let text = format!("{err:#}");
        assert!(
            text.contains(invites.join("token-1.json").to_str().unwrap()),
            "в ошибке нет пути файла инвайта: {text}"
        );
    }
}
