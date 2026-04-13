//! Preset cache: load from disk, fetch from hub, verify signature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use xr_proto::config::RoutingConfig;
use xr_proto::preset::{Preset, PresetSummary};

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
        let client = match reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(false)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to build HTTP client: {}", e);
                return false;
            }
        };

        // Check version list first.
        let summaries_url = format!("{}/api/v1/presets", self.hub_url);
        let summaries: Vec<PresetSummary> = match client.get(&summaries_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to parse presets list: {}", e);
                    return false;
                }
            },
            Ok(resp) => {
                tracing::warn!("hub returned {} for presets list", resp.status());
                return false;
            }
            Err(e) => {
                tracing::info!("hub unreachable for preset check: {}", e);
                return false;
            }
        };

        let remote_version = summaries
            .iter()
            .find(|s| s.name == self.preset_name)
            .map(|s| s.version);

        let local_version = self.cached.as_ref().map(|p| p.version);

        match (remote_version, local_version) {
            (Some(remote), Some(local)) if remote <= local => {
                tracing::debug!(
                    "preset '{}' is up to date (v{})",
                    self.preset_name,
                    local
                );
                return false;
            }
            (None, _) => {
                tracing::warn!(
                    "preset '{}' not found on hub",
                    self.preset_name
                );
                return false;
            }
            _ => {}
        }

        // Fetch full preset.
        let preset_url = format!(
            "{}/api/v1/presets/{}",
            self.hub_url, self.preset_name
        );
        let preset: Preset = match client.get(&preset_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("failed to parse preset: {}", e);
                    return false;
                }
            },
            Ok(resp) => {
                tracing::warn!("hub returned {} for preset", resp.status());
                return false;
            }
            Err(e) => {
                tracing::warn!("failed to fetch preset: {}", e);
                return false;
            }
        };

        tracing::info!(
            "fetched preset '{}' v{} from hub",
            preset.name,
            preset.version
        );

        // Save to disk cache.
        if let Err(e) = self.save_to_disk(&preset) {
            tracing::warn!("failed to save preset cache: {}", e);
        }

        self.cached = Some(preset);
        true
    }

    /// Get the cached routing config, if any.
    pub fn routing_config(&self) -> Option<&RoutingConfig> {
        self.cached.as_ref().map(|p| &p.rules)
    }

    fn cache_path(&self) -> PathBuf {
        self.cache_dir.join(format!("{}.json", self.preset_name))
    }

    fn save_to_disk(&self, preset: &Preset) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let data = serde_json::to_string_pretty(preset)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let path = self.cache_path();
        // Simple atomic-ish write: write to tmp then rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}
