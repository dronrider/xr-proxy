use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use xr_proto::preset::{Invite, Preset};
use xr_proto::share::ShareRecord;

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
    std::fs::create_dir_all(&dir)?;
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
    std::fs::create_dir_all(&dir)?;
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
    std::fs::create_dir_all(&dir)?;
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

/// Atomic write: write to temp file in same dir, then rename.
fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = target.parent().unwrap();
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.persist(target)?;
    Ok(())
}
