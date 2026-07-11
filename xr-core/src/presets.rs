//! Preset cache: load from disk, fetch from hub, verify signature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use xr_proto::config::RoutingConfig;
use xr_proto::preset::{Preset, PresetSummary};

/// Исход [`PresetCache::refresh`]: обновились до новой версии либо локальная
/// уже актуальна. Ошибки (сеть, 404, битый JSON) идут отдельной веткой Err.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Updated(u64),
    UpToDate(u64),
}

/// Caches a single preset locally and fetches updates from the hub.
pub struct PresetCache {
    cache_dir: PathBuf,
    hub_url: String,
    preset_name: String,
    cached: Option<Preset>,
}

impl PresetCache {
    pub fn new(cache_dir: &Path, hub_url: &str, preset_name: &str) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            hub_url: hub_url.trim_end_matches('/').to_string(),
            preset_name: preset_name.to_string(),
            cached: None,
        }
    }

    /// Load cached preset from disk.
    pub fn load_from_disk(&mut self) -> Option<&Preset> {
        let path = self.cache_path();
        if !path.exists() {
            return None;
        }
        match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<Preset>(&data) {
                Ok(preset) => {
                    tracing::info!(
                        "loaded cached preset '{}' v{} from {}",
                        preset.name,
                        preset.version,
                        path.display()
                    );
                    self.cached = Some(preset);
                    self.cached.as_ref()
                }
                Err(e) => {
                    tracing::warn!("failed to parse cached preset {}: {}", path.display(), e);
                    None
                }
            },
            Err(e) => {
                tracing::warn!("failed to read cached preset {}: {}", path.display(), e);
                None
            }
        }
    }

    /// Fetch preset from hub if newer version available.
    /// Returns true if cache was updated.
    pub async fn fetch_if_stale(&mut self, timeout: Duration) -> bool {
        match self.refresh(timeout).await {
            Ok(RefreshOutcome::Updated(_)) => true,
            Ok(RefreshOutcome::UpToDate(v)) => {
                tracing::debug!("preset '{}' is up to date (v{})", self.preset_name, v);
                false
            }
            Err(e) => {
                tracing::warn!("preset '{}' refresh failed: {}", self.preset_name, e);
                false
            }
        }
    }

    /// То же, что [`fetch_if_stale`], но с внятным исходом: ручная кнопка
    /// «Обновить сейчас» должна отличать «актуален» от «хаб недоступен»,
    /// bool этого не умеет.
    pub async fn refresh(&mut self, timeout: Duration) -> Result<RefreshOutcome, String> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(false)
            .build()
            .map_err(|e| format!("http client: {}", e))?;

        // Check version list first.
        let summaries_url = format!("{}/api/v1/presets", self.hub_url);
        let resp = client
            .get(&summaries_url)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("http_{}", resp.status().as_u16()));
        }
        let summaries: Vec<PresetSummary> = resp
            .json()
            .await
            .map_err(|e| format!("bad presets list: {}", e))?;

        let remote_version = summaries
            .iter()
            .find(|s| s.name == self.preset_name)
            .map(|s| s.version);

        let local_version = self.cached.as_ref().map(|p| p.version);

        match (remote_version, local_version) {
            (Some(remote), Some(local)) if remote <= local => {
                return Ok(RefreshOutcome::UpToDate(local));
            }
            (None, _) => {
                return Err("not_found".into());
            }
            _ => {}
        }

        // Fetch full preset.
        let preset_url = format!(
            "{}/api/v1/presets/{}",
            self.hub_url, self.preset_name
        );
        let resp = client
            .get(&preset_url)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("http_{}", resp.status().as_u16()));
        }
        let preset: Preset = resp
            .json()
            .await
            .map_err(|e| format!("bad preset: {}", e))?;

        tracing::info!(
            "fetched preset '{}' v{} from hub",
            preset.name,
            preset.version
        );

        // Save to disk cache.
        if let Err(e) = self.save_to_disk(&preset) {
            tracing::warn!("failed to save preset cache: {}", e);
        }

        let version = preset.version;
        self.cached = Some(preset);
        Ok(RefreshOutcome::Updated(version))
    }

    /// Get the cached routing config, if any.
    pub fn routing_config(&self) -> Option<&RoutingConfig> {
        self.cached.as_ref().map(|p| &p.rules)
    }

    fn cache_path(&self) -> PathBuf {
        self.cache_dir.join(format!("{}.json", self.preset_name))
    }

    fn save_to_disk(&self, preset: &Preset) -> std::io::Result<()> {
        Self::write_to_disk(&self.cache_dir, preset)
    }

    /// Atomically write `preset` to `<cache_dir>/<preset.name>.json`.
    /// Public so onboarding (one-shot, no live `PresetCache`) can pre-warm
    /// the cache for the engine that will pick it up on first Connect.
    pub fn write_to_disk(cache_dir: &Path, preset: &Preset) -> std::io::Result<()> {
        std::fs::create_dir_all(cache_dir)?;
        let data = serde_json::to_string_pretty(preset)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let path = cache_dir.join(format!("{}.json", preset.name));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}
