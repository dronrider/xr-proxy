use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
    pub sessions: RwLock<HashMap<String, String>>, // session_token → username
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

    Ok(Arc::new(AppState {
        presets: RwLock::new(presets),
        invites: RwLock::new(invites),
        shares: RwLock::new(shares),
        exposes: RwLock::new(exposes),
        sessions: RwLock::new(HashMap::new()),
        config,
        signing,
        preset_gen: watch::Sender::new(0),
        web_attempts: Default::default(),
    }))
}
