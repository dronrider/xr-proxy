use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{watch, RwLock};
use xr_proto::preset::{Invite, Preset};
use xr_proto::share::{ExposeRecord, ShareRecord};

use crate::config::HubConfig;
use crate::signing::SigningContext;
use crate::storage;

pub struct AppState {
    pub presets: RwLock<HashMap<String, Preset>>,
    pub invites: RwLock<HashMap<String, Invite>>,
    pub shares: RwLock<HashMap<String, ShareRecord>>, // share_id -> record (LLD-19)
    /// Публикации локальных сервисов: имя -> запись (LLD-38 п. 2.1). Имя одно
    /// на хаб, потому что оно же поддомен браузерного входа.
    pub exposes: RwLock<HashMap<String, ExposeRecord>>,
    /// Admin-сессии (XR-194): TTL, logout и лимит на оператора держит сам
    /// стор, поэтому карта не растёт бесконечно и утёкший Bearer гасится.
    pub sessions: crate::sessions::SessionStore,
    pub config: HubConfig,
    pub signing: Option<SigningContext>,
    /// Поколение пресетов (LLD-37): любая правка через админку двигает его, и
    /// на этом просыпаются клиенты, висящие на ручке ожидания. Канал один на
    /// все пресеты: разбудить лишних дешевле, чем вести канал на имя, чужой
    /// подписчик сверит свою версию и перевзведётся.
    pub preset_gen: watch::Sender<u64>,
    /// Счётчик неверных паролей на ручке браузерного входа (LLD-38 п. 3.2):
    /// перебор гасится задержкой на стороне хаба, а не только на фронте.
    pub web_attempts: crate::api::web::PasswordAttempts,
    /// Лимит попыток входа на источник для `/auth/login` (XR-195): шторм
    /// логинов упирается в отказ без argon2, а не занимает проверкой пароля.
    pub login_attempts: crate::api::auth::LoginAttempts,
    /// Готовность (XR-230): поднимается в конце [`hydrate`], когда инвайты,
    /// шары и ключ подписи уже на месте. Слушатель хаб поднимает после
    /// hydrate, поэтому наружу неготовность видна только закрытым портом,
    /// но мониторингу ручка `/readyz` обязана отвечать той же правдой, а не
    /// «процесс вообще жив».
    pub ready: AtomicBool,
}

impl AppState {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// Load state from disk and build AppState.
pub fn hydrate(config: HubConfig) -> Result<Arc<AppState>> {
    let data_dir = Path::new(&config.server.data_dir);

    // Ensure data directories exist.
    std::fs::create_dir_all(data_dir.join("presets"))?;
    std::fs::create_dir_all(data_dir.join("invites"))?;
    std::fs::create_dir_all(data_dir.join("shares"))?;
    std::fs::create_dir_all(data_dir.join("expose"))?;

    let presets = storage::load_all_presets(data_dir)?;
    let invites = storage::load_all_invites(data_dir)?;
    let shares = storage::load_all_shares(data_dir)?;
    let exposes = storage::load_all_exposes(data_dir)?;

    tracing::info!(
        "loaded {} presets, {} invites, {} shares, {} publications from {}",
        presets.len(),
        invites.len(),
        shares.len(),
        exposes.len(),
        data_dir.display()
    );

    let signing = config
        .signing
        .as_ref()
        .map(|s| SigningContext::from_file(&s.private_key))
        .transpose()?;

    let login_attempts = crate::api::auth::LoginAttempts::new(
        config.admin.login_max_attempts,
        config.admin.login_window_secs.saturating_mul(1000),
    );

    let state = Arc::new(AppState {
        presets: RwLock::new(presets),
        invites: RwLock::new(invites),
        shares: RwLock::new(shares),
        exposes: RwLock::new(exposes),
        sessions: crate::sessions::SessionStore::new(
            Duration::from_secs(config.admin.session_ttl_secs),
            config.admin.max_sessions_per_user,
        ),
        config,
        signing,
        preset_gen: watch::Sender::new(0),
        web_attempts: Default::default(),
        login_attempts,
        ready: AtomicBool::new(false),
    });
    state.ready.store(true, Ordering::Release);
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    // XR-230: флаг готовности поднимает сам hydrate, а не старт сервера,
    // поэтому состояние «поднято» и «слушатель открыт» не разъезжаются.
    #[test]
    fn hydrate_ends_ready() {
        let dir = std::env::temp_dir().join(format!("xr-hub-ready-{}", std::process::id()));
        let toml = format!("[server]\ndata_dir = \"{}\"\n[admin]\nusers = []\n", dir.display());
        let config: HubConfig = toml::from_str(&toml).unwrap();

        let state = hydrate(config).unwrap();

        assert!(state.is_ready());
        let _ = std::fs::remove_dir_all(dir);
    }
}
