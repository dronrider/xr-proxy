//! Preset cache: load from disk, fetch from hub, verify signature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use xr_proto::config::RoutingConfig;
use xr_proto::preset::{decode_verifying_key, verify_preset, Preset, PresetSummary};
use xr_proto::user_rule::UserRule;

/// Исход [`PresetCache::refresh`]: обновились до новой версии либо локальная
/// уже актуальна. Ошибки (сеть, 404, битый JSON) идут отдельной веткой Err.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Updated(u64),
    UpToDate(u64),
}

/// Сколько хаб держит висящий запрос ожидания. Потолок на его стороне 60
/// секунд, запас нужен промежуточным прокси, которые режут долгие ответы.
pub const WAIT_HOLD: Duration = Duration::from_secs(55);

/// Насколько клиентский таймаут больше удержания на хабе.
const WAIT_TIMEOUT_MARGIN: Duration = Duration::from_secs(15);

/// С чего начинается пауза деградированного режима, дальше удвоение до
/// `fallback_interval`.
const DEGRADED_BACKOFF_START: Duration = Duration::from_secs(5);

/// Caches a single preset locally and fetches updates from the hub.
pub struct PresetCache {
    cache_dir: PathBuf,
    hub_url: String,
    preset_name: String,
    cached: Option<Preset>,
    /// Публичный ключ проверки подписи (XR-207) в том виде, в каком его
    /// назвал конфиг или профиль. Разбирается на каждую проверку: проверки
    /// редки (новая версия пресета, чтение кэша), а хранение сырой строки
    /// держит кэш и конфиг в одинаковых терминах.
    trusted_public_key: Option<String>,
}

impl PresetCache {
    pub fn new(
        cache_dir: &Path,
        hub_url: &str,
        preset_name: &str,
        trusted_public_key: Option<&str>,
    ) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            hub_url: hub_url.trim_end_matches('/').to_string(),
            preset_name: preset_name.to_string(),
            cached: None,
            trusted_public_key: trusted_public_key
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string),
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
                    if let Err(e) =
                        verify_trusted(&preset, self.trusted_public_key.as_deref())
                    {
                        tracing::warn!(
                            "cached preset {} rejected: {} (kept on disk, not applied)",
                            path.display(),
                            e
                        );
                        return None;
                    }
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

        if let Err(e) = verify_trusted(&preset, self.trusted_public_key.as_deref()) {
            tracing::warn!(
                "preset '{}' v{} from hub rejected: {}",
                preset.name,
                preset.version,
                e
            );
            return Err(e);
        }

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

    /// Ожидание изменения пресета на хабе (LLD-37): запрос висит на ручке
    /// `wait` со своей версией и возвращается либо новым пресетом, либо
    /// `UpToDate` по истечении удержания. Без кэша уходит `version=0`, такой
    /// версии не бывает, и хаб сразу отдаёт текущий пресет: бутстрап
    /// достаётся тем же вызовом.
    ///
    /// Клиентский таймаут заведомо больше удержания, иначе клиент рвёт
    /// собственный висящий запрос и обновление не доезжает никогда.
    pub async fn wait_for_update(&mut self, hold: Duration) -> Result<RefreshOutcome, String> {
        let local_version = self.cached.as_ref().map(|p| p.version).unwrap_or(0);
        let client = reqwest::Client::builder()
            .timeout(hold + WAIT_TIMEOUT_MARGIN)
            .danger_accept_invalid_certs(false)
            .build()
            .map_err(|e| format!("http client: {}", e))?;
        let url = format!(
            "{}/api/v1/presets/{}/wait?version={}&timeout_secs={}",
            self.hub_url,
            self.preset_name,
            local_version,
            hold.as_secs()
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(RefreshOutcome::UpToDate(local_version));
        }
        if !resp.status().is_success() {
            return Err(format!("http_{}", resp.status().as_u16()));
        }
        let preset: Preset = resp
            .json()
            .await
            .map_err(|e| format!("bad preset: {}", e))?;

        if let Err(e) = verify_trusted(&preset, self.trusted_public_key.as_deref()) {
            tracing::warn!(
                "preset '{}' v{} from hub rejected: {}",
                preset.name,
                preset.version,
                e
            );
            return Err(e);
        }

        tracing::info!(
            "preset '{}' v{} received from hub (long-poll)",
            preset.name,
            preset.version
        );

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

/// Фоновая доставка правил с хаба, одна на всех потребителей (LLD-37):
/// её крутят и `xr-client`, и `VpnEngine`, раньше у каждого был свой почти
/// дословно совпадающий цикл. `on_rules` зовётся на каждой новой версии,
/// вызывающий собирает merged-роутер и подменяет свой `RwLock`.
///
/// Пока хаб отвечает, правило доезжает за секунды. Отказ уводит в
/// деградированный режим: пауза с backoff от [`DEGRADED_BACKOFF_START`] до
/// `fallback_interval`, на каждом пробуждении обычный опрос (обновления
/// продолжают доезжать с прежней скоростью) и новая попытка встать на
/// ожидание. `shutdown` завершает цикл: движок передаёт свой сигнал
/// остановки, у xr-client цикл живёт до конца процесса.
pub async fn watch_loop(
    mut cache: PresetCache,
    fallback_interval: Duration,
    shutdown: impl std::future::Future<Output = ()>,
    mut on_rules: impl FnMut(&RoutingConfig),
) {
    let work = async {
        let mut backoff = Duration::ZERO;
        loop {
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
                if cache.fetch_if_stale(Duration::from_secs(5)).await {
                    if let Some(rules) = cache.routing_config() {
                        on_rules(rules);
                    }
                }
            }
            match cache.wait_for_update(WAIT_HOLD).await {
                Ok(RefreshOutcome::Updated(version)) => {
                    backoff = Duration::ZERO;
                    if let Some(rules) = cache.routing_config() {
                        on_rules(rules);
                    }
                    tracing::info!("preset updated to v{} without restart", version);
                }
                Ok(RefreshOutcome::UpToDate(_)) => {
                    backoff = Duration::ZERO;
                }
                Err(e) => {
                    backoff = next_backoff(backoff, fallback_interval);
                    tracing::warn!(
                        "preset wait failed ({}), falling back to polling every {}s",
                        e,
                        backoff.as_secs()
                    );
                }
            }
        }
    };
    tokio::select! {
        _ = work => {}
        _ = shutdown => {}
    }
}

fn next_backoff(current: Duration, cap: Duration) -> Duration {
    if current.is_zero() {
        DEGRADED_BACKOFF_START.min(cap)
    } else {
        (current * 2).min(cap)
    }
}

/// Прогнать пресет через проверку подписи (XR-207). Общий фильтр всех точек
/// приёма: сетевой фетч, ожидание изменения, дисковый кэш и прогрев
/// онбордингом. Ключ не задан, пресет принимается как раньше; ключ задан,
/// пресет без подписи или с неверной подписью отбраковывается с причиной.
/// Неразборная строка ключа тоже отбраковывает пресет: опечатка в конфиге
/// не должна молча выключать проверку.
pub(crate) fn verify_trusted(
    preset: &Preset,
    trusted_public_key: Option<&str>,
) -> Result<(), String> {
    let Some(key_b64) = trusted_public_key.map(str::trim).filter(|k| !k.is_empty()) else {
        return Ok(());
    };
    let key: VerifyingKey =
        decode_verifying_key(key_b64).map_err(|e| format!("trusted key: {e}"))?;
    match verify_preset(preset, &key) {
        Ok(true) => Ok(()),
        Ok(false) => Err("signature: preset is unsigned or the signature is wrong".into()),
        Err(e) => Err(format!("signature: {e}")),
    }
}

/// Предупреждение на старт без ключа проверки (XR-207): пресет хаба тогда
/// доверяется целиком, и подменённый хаб разворачивает трафик напрямую.
/// Печатается один раз стартом клиента, а не каждой конструкцией кэша.
pub fn warn_if_unverified(trusted_public_key: Option<&str>) {
    let blank = trusted_public_key.map(str::trim).unwrap_or("").is_empty();
    if blank {
        tracing::warn!(
            "hub presets are not signature-verified: set trusted_public_key \
             (the public half of the hub [signing] key) to reject tampered presets"
        );
    }
}

/// Прочитать кэш пресета для экрана правил: `<cache_dir>/<name>.json`.
/// `None`, когда файла нет или он не читается. Пресет проходит ту же
/// проверку подписи, что и в [`PresetCache`] (XR-207): карточка и превью
/// не должны показывать то, что движок применять откажется.
///
/// Кэш пишет только ядро (apply инвайта, фоновый рефреш, кнопка «Обновить
/// сейчас»), и раньше приложение лезло в него своим разбором, то есть знало
/// внутренний формат чужого файла. Теперь наружу отдаётся ровно то, что нужно
/// карточке, и формат кэша остаётся делом ядра (XR-271).
pub fn read_cached(
    cache_dir: &Path,
    name: &str,
    trusted_public_key: Option<&str>,
) -> Option<Preset> {
    if name.trim().is_empty() {
        return None;
    }
    let path = cache_dir.join(format!("{}.json", name));
    let data = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Preset>(&data) {
        Ok(preset) => match verify_trusted(&preset, trusted_public_key) {
            Ok(()) => Some(preset),
            Err(e) => {
                tracing::warn!("cached preset {} rejected: {}", path.display(), e);
                None
            }
        },
        Err(e) => {
            tracing::warn!("unreadable preset cache {}: {}", path.display(), e);
            None
        }
    }
}

/// Тот же кэш, свёрнутый в JSON для обвязки платформы: имя, версия, дата,
/// действие по умолчанию и группы правил.
pub fn cached_preset_json(
    cache_dir: &Path,
    name: &str,
    trusted_public_key: Option<&str>,
) -> Option<String> {
    let preset = read_cached(cache_dir, name, trusted_public_key)?;
    let rules: Vec<serde_json::Value> = preset
        .rules
        .rules
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name.clone().unwrap_or_default(),
                "action": r.action,
                "domains": r.domains,
                "ip_ranges": r.ip_ranges,
                "geoip": r.geoip,
            })
        })
        .collect();
    Some(
        serde_json::json!({
            "name": preset.name,
            "version": preset.version,
            "updated_at": preset.updated_at,
            "default_action": preset.rules.default_action,
            "rules": rules,
        })
        .to_string(),
    )
}

/// Читаемый блок `[routing]` из того же материала, из которого движок строит
/// merged-роутер: мои правила сверху, пресет хаба ниже, действие по умолчанию.
/// Результат готов ко вставке в `configs/client.toml` роутера, в этом смысл
/// кнопки `{ }` на экране правил.
///
/// Пустое `default_action` берёт общий дефолт клиента: экран правил не должен
/// держать свою копию этого значения (XR-271).
pub fn merged_toml(user_rules: &[UserRule], default_action: &str, preset: Option<&Preset>) -> String {
    let default_action = if default_action.trim().is_empty() {
        crate::client_config::DEFAULT_ROUTING_ACTION
    } else {
        default_action.trim()
    };
    let preset_tag = preset.map(|p| format!("{} v{}", p.name, p.version));

    let mut out = String::new();
    out.push_str("# Generated by XR Proxy\n");
    out.push_str(&format!(
        "# Merged: user overrides + preset {}\n\n",
        preset_tag.clone().unwrap_or_else(|| "(не подключён)".into())
    ));
    out.push_str("[routing]\n");
    out.push_str(&format!("default_action = {}\n", toml_string(default_action)));

    if !user_rules.is_empty() {
        out.push_str("\n# --- User overrides ---\n");
        for rule in user_rules {
            out.push_str("[[routing.rules]]\n");
            out.push_str(&format!("action = {}\n", toml_string(&rule.action)));
            // Правило пользователя это либо домен, либо подсеть; строка с
            // косой чертой это подсеть, так же её читает и классификатор.
            let key = if rule.pattern.contains('/') {
                "ip_ranges"
            } else {
                "domains"
            };
            out.push_str(&format!("{} = [{}]\n", key, toml_string(&rule.pattern)));
        }
    }

    if let Some(preset) = preset {
        if !preset.rules.rules.is_empty() {
            out.push_str(&format!(
                "\n# --- Preset {} ---\n",
                preset_tag.unwrap_or_default()
            ));
            for rule in &preset.rules.rules {
                out.push_str("[[routing.rules]]\n");
                // Имя группы (XR-117) идёт первой строкой правила: по нему в
                // выгруженном конфиге видно, о чём эти двадцать доменов.
                if let Some(name) = rule.name.as_ref().filter(|n| !n.trim().is_empty()) {
                    out.push_str(&format!("name = {}\n", toml_string(name)));
                }
                out.push_str(&format!("action = {}\n", toml_string(&rule.action)));
                if !rule.domains.is_empty() {
                    out.push_str(&toml_array("domains", &rule.domains));
                }
                if !rule.ip_ranges.is_empty() {
                    out.push_str(&toml_array("ip_ranges", &rule.ip_ranges));
                }
                if !rule.geoip.is_empty() {
                    out.push_str(&toml_array("geoip", &rule.geoip));
                }
            }
        }
    }
    out
}

/// Многострочный массив, чтобы длинные списки доменов пресета читались.
fn toml_array(key: &str, values: &[String]) -> String {
    if values.len() <= 3 {
        let inner: Vec<String> = values.iter().map(|v| toml_string(v)).collect();
        format!("{} = [{}]\n", key, inner.join(", "))
    } else {
        let mut out = format!("{} = [\n", key);
        for v in values {
            out.push_str(&format!("  {},\n", toml_string(v)));
        }
        out.push_str("]\n");
        out
    }
}

/// Строка в TOML-кавычках. Название группы владелец пресета набирает свободным
/// текстом, и кавычка в нём оборвала бы значение: превью для того и печатается,
/// чтобы его скопировали в конфиг роутера, а битый TOML туда не вставить.
fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
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
    /// Мок-хаба хватает на несколько запросов подряд: `respond` выбирает ответ
    /// по пути запроса, а `None` оставляет соединение висеть, изображая
    /// удержание на ручке ожидания. Путь каждого запроса уходит в канал, по
    /// нему тесты сверяют, какую версию клиент принёс с собой.
    fn serve_hub<F>(respond: F) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>)
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let respond = std::sync::Arc::new(respond);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let respond = respond.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = sock.read(&mut buf).await else { return };
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let _ = tx.send(path.clone());
                    match respond(&path) {
                        Some(resp) => {
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        None => {
                            // Сокет держим живым, иначе клиент увидит обрыв
                            // вместо честного ожидания.
                            std::future::pending::<()>().await;
                        }
                    }
                });
            }
        });
        (format!("http://{}", addr), rx)
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    const PRESET_V3: &str = r#"{"name":"russia","version":3,"updated_at":"2026-08-03T00:00:00Z","description":"","rules":{"default_action":"direct","rules":[{"action":"proxy","domains":["example.com"]}]}}"#;

    // Новая версия приезжает тем же запросом, каким её ждали: лишнего похода
    // за телом нет, кэш ложится на диск, а следующее ожидание уносит уже
    // свежую версию (иначе хаб отдавал бы одно и то же по кругу).
    #[tokio::test]
    async fn wait_for_update_applies_preset_and_carries_version() {
        let dir = tempfile::tempdir().unwrap();
        let (url, mut requests) = serve_hub(|path| {
            if path.contains("version=0") {
                Some(json_response(PRESET_V3))
            } else {
                Some("HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_string())
            }
        });

        let mut cache = PresetCache::new(dir.path(), &url, "russia", None);
        let first = cache.wait_for_update(Duration::from_secs(1)).await.unwrap();
        let second = cache.wait_for_update(Duration::from_secs(1)).await.unwrap();

        assert_eq!(first, RefreshOutcome::Updated(3));
        assert_eq!(second, RefreshOutcome::UpToDate(3));
        assert_eq!(
            cache.routing_config().unwrap().rules[0].domains,
            vec!["example.com".to_string()]
        );

        let on_disk = std::fs::read_to_string(dir.path().join("russia.json")).unwrap();
        let saved: Preset = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(saved.version, 3);

        let first_path = requests.recv().await.unwrap();
        let second_path = requests.recv().await.unwrap();
        assert!(first_path.contains("version=0"), "первый запрос: {first_path}");
        assert!(second_path.contains("version=3"), "второй запрос: {second_path}");
        assert!(first_path.contains("timeout_secs=1"), "первый запрос: {first_path}");
    }

    // Старый хаб ручки ожидания не знает и отдаёт страницу админки. Разбор
    // падает, и цикл уходит в деградированный опрос: специальной детекции
    // версии хаба не заводим.
    #[tokio::test]
    async fn wait_for_update_rejects_spa_stub() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _requests) = serve_hub(|_| {
            Some(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                "<title>xr-hub Admin</title>".len(),
                "<title>xr-hub Admin</title>"
            ))
        });

        let mut cache = PresetCache::new(dir.path(), &url, "russia", None);
        let err = cache
            .wait_for_update(Duration::from_secs(1))
            .await
            .unwrap_err();

        assert!(err.starts_with("bad preset"), "неожиданная ошибка: {err}");
    }

    #[tokio::test]
    async fn wait_for_update_maps_http_error() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _requests) = serve_hub(|_| {
            Some("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
        });

        let mut cache = PresetCache::new(dir.path(), &url, "russia", None);
        let err = cache
            .wait_for_update(Duration::from_secs(1))
            .await
            .unwrap_err();

        assert_eq!(err, "http_500");
    }

    // Цикл ждёт события, а не тикает по часам: первый же ответ хаба доводит
    // правила до вызывающего, дальше запрос висит, а сигнал остановки
    // завершает задачу.
    #[tokio::test]
    async fn watch_loop_delivers_rules_and_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _requests) = serve_hub(|path| {
            if path.contains("version=0") {
                Some(json_response(PRESET_V3))
            } else {
                None
            }
        });

        let cache = PresetCache::new(dir.path(), &url, "russia", None);
        let (rules_tx, mut rules_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            watch_loop(
                cache,
                Duration::from_secs(300),
                async {
                    let _ = stop_rx.await;
                },
                move |rules| {
                    let _ = rules_tx.send(rules.rules[0].domains.clone());
                },
            )
            .await;
        });

        let domains = tokio::time::timeout(Duration::from_secs(5), rules_rx.recv())
            .await
            .expect("правила не доехали за отведённое время")
            .unwrap();
        assert_eq!(domains, vec!["example.com".to_string()]);

        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("цикл не завершился по сигналу остановки")
            .unwrap();
    }

    // Хаб без ручки ожидания (или просто отвечающий отказом) не должен
    // останавливать доставку: цикл выжидает backoff и продолжает прежним
    // опросом. Интервал в тесте маленький, потолок backoff считается от него,
    // поэтому ждать нечего и часы не нужны.
    #[tokio::test]
    async fn watch_loop_falls_back_to_polling_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _requests) = serve_hub(|path| {
            if path.contains("/wait") {
                Some("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
            } else if path == "/api/v1/presets" {
                Some(json_response(
                    r#"[{"name":"russia","version":3,"updated_at":"2026-08-03T00:00:00Z","rules_count":1}]"#,
                ))
            } else if path == "/api/v1/presets/russia" {
                Some(json_response(PRESET_V3))
            } else {
                None
            }
        });

        let cache = PresetCache::new(dir.path(), &url, "russia", None);
        let (rules_tx, mut rules_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            watch_loop(
                cache,
                Duration::from_millis(20),
                async {
                    let _ = stop_rx.await;
                },
                move |rules| {
                    let _ = rules_tx.send(rules.rules[0].domains.clone());
                },
            )
            .await;
        });

        let domains = tokio::time::timeout(Duration::from_secs(5), rules_rx.recv())
            .await
            .expect("деградированный опрос не привёз правила")
            .unwrap();
        assert_eq!(domains, vec!["example.com".to_string()]);

        stop_tx.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[test]
    fn backoff_doubles_and_stops_at_interval() {
        let cap = Duration::from_secs(300);
        let first = next_backoff(Duration::ZERO, cap);
        assert_eq!(first, DEGRADED_BACKOFF_START);
        assert_eq!(next_backoff(first, cap), Duration::from_secs(10));
        assert_eq!(next_backoff(Duration::from_secs(200), cap), cap);
        // Потолок ниже стартовой паузы (короткий refresh_interval_secs в
        // конфиге) не должен делать паузу длиннее самого потолка.
        assert_eq!(
            next_backoff(Duration::ZERO, Duration::from_secs(2)),
            Duration::from_secs(2)
        );
    }

    fn sample_preset() -> Preset {
        serde_json::from_str(
            r#"{"name":"russia","version":7,"updated_at":"2026-01-01T00:00:00+00:00",
                "rules":{"default_action":"direct","rules":[
                  {"name":"Видео","action":"proxy","domains":["youtube.com","*.youtube.com"]},
                  {"action":"proxy","domains":["a.example","b.example","c.example","d.example"],
                   "ip_ranges":["203.0.113.0/24"],"geoip":["RU"]}
                ]}}"#,
        )
        .unwrap()
    }

    fn rule(action: &str, pattern: &str) -> UserRule {
        UserRule {
            action: action.into(),
            pattern: pattern.into(),
        }
    }

    /// Превью собирается в том же порядке, в каком движок строит роутер: мои
    /// правила выше пресета, домены и подсети расходятся по своим ключам.
    #[test]
    fn merged_toml_puts_user_rules_above_preset() {
        let preset = sample_preset();
        let toml = merged_toml(
            &[rule("direct", "bank.example"), rule("proxy", "10.0.0.0/8")],
            "direct",
            Some(&preset),
        );
        let user_at = toml.find("# --- User overrides ---").expect("нет блока моих правил");
        let preset_at = toml.find("# --- Preset russia v7 ---").expect("нет блока пресета");
        assert!(user_at < preset_at, "пресет выше моих правил:\n{toml}");
        assert!(toml.contains("domains = [\"bank.example\"]"), "{toml}");
        assert!(toml.contains("ip_ranges = [\"10.0.0.0/8\"]"), "{toml}");
        assert!(toml.contains("default_action = \"direct\""), "{toml}");
        assert!(toml.contains("name = \"Видео\""), "{toml}");
        // Длинный список доменов разворачивается в многострочный массив.
        assert!(toml.contains("domains = [\n  \"a.example\",\n"), "{toml}");
        assert!(toml.contains("geoip = [\"RU\"]"), "{toml}");
        assert!(toml.parse::<toml::Value>().is_ok(), "невалидный TOML:\n{toml}");
    }

    /// Пустое действие по умолчанию берётся из общего дефолта клиента: у
    /// экрана правил своей копии этого значения больше нет.
    #[test]
    fn merged_toml_defaults_the_action() {
        let toml = merged_toml(&[], "", None);
        assert!(
            toml.contains(&format!(
                "default_action = \"{}\"",
                crate::client_config::DEFAULT_ROUTING_ACTION
            )),
            "{toml}"
        );
        assert!(toml.contains("(не подключён)"), "{toml}");
        assert!(!toml.contains("[[routing.rules]]"), "{toml}");
    }

    /// Кавычка в названии группы экранируется: превью для того и печатается,
    /// чтобы его вставили в конфиг роутера, а битый TOML туда не вставить.
    #[test]
    fn merged_toml_escapes_group_names() {
        let mut preset = sample_preset();
        preset.rules.rules[0].name = Some("Видео \"и\" музыка".into());
        let toml = merged_toml(&[], "direct", Some(&preset));
        assert!(toml.contains(r#"name = "Видео \"и\" музыка""#), "{toml}");
        assert!(toml.parse::<toml::Value>().is_ok(), "невалидный TOML:\n{toml}");
    }

    /// Карточка пресета читает кэш через ядро: имя, версия, дата и группы.
    /// Ни файла, ни его формата обвязка платформы больше не знает.
    #[test]
    fn cached_preset_json_reads_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        PresetCache::write_to_disk(dir.path(), &sample_preset()).unwrap();

        let json = cached_preset_json(dir.path(), "russia", None).expect("кэш не прочитан");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "russia");
        assert_eq!(value["version"], 7);
        assert_eq!(value["default_action"], "direct");
        assert_eq!(value["rules"].as_array().unwrap().len(), 2);
        assert_eq!(value["rules"][0]["name"], "Видео");
        // Правило без названия группы отдаётся с пустым именем, а не без ключа.
        assert_eq!(value["rules"][1]["name"], "");
        assert_eq!(value["rules"][1]["geoip"][0], "RU");
    }

    /// Пустое имя, отсутствующий и битый файл это «карточки нет», а не паника.
    #[test]
    fn cached_preset_json_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cached_preset_json(dir.path(), "", None).is_none());
        assert!(cached_preset_json(dir.path(), "russia", None).is_none());
        std::fs::write(dir.path().join("broken.json"), "{ не json").unwrap();
        assert!(cached_preset_json(dir.path(), "broken", None).is_none());
    }

}
