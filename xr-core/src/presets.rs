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
        // Version list first (общий с листингом пикера выбора пресета).
        let summaries = list_presets(&self.hub_url, timeout).await?;

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
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(false)
            .build()
            .map_err(|e| format!("http client: {}", e))?;
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

/// Список пресетов с хаба (сводки без правил). Отдельно от [`PresetCache`]:
/// листинг не привязан к одному пресету, его дёргает пикер выбора пресета
/// (XR-119). Ошибки те же, что у [`PresetCache::refresh`]: сеть, http-код,
/// битый JSON.
pub async fn list_presets(hub_url: &str, timeout: Duration) -> Result<Vec<PresetSummary>, String> {
    let hub_url = hub_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .danger_accept_invalid_certs(false)
        .build()
        .map_err(|e| format!("http client: {}", e))?;
    let url = format!("{}/api/v1/presets", hub_url);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }
    resp.json::<Vec<PresetSummary>>()
        .await
        .map_err(|e| format!("bad presets list: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Отдаёт один HTTP-ответ на первое соединение и закрывается.
    async fn serve_once(body: &'static str, status_line: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn list_presets_parses_summaries() {
        let body = r#"[{"name":"russia","version":12,"updated_at":"2026-07-20T00:00:00Z","rules_count":148},{"name":"turkey","version":7,"updated_at":"2026-07-01T00:00:00Z","rules_count":96}]"#;
        let url = serve_once(body, "HTTP/1.1 200 OK").await;
        let out = list_presets(&url, Duration::from_secs(5)).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "russia");
        assert_eq!(out[0].version, 12);
        assert_eq!(out[0].rules_count, 148);
        assert_eq!(out[1].name, "turkey");
    }

    #[tokio::test]
    async fn list_presets_maps_http_error() {
        let url = serve_once("", "HTTP/1.1 500 Internal Server Error").await;
        let err = list_presets(&url, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(err, "http_500");
    }
}
