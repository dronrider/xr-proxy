use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use xr_proto::preset::{Invite, Preset};
use xr_proto::share::ShareRecord;

use crate::config::HubConfig;
use crate::signing::SigningContext;
use crate::storage;

pub struct AppState {
    pub presets: RwLock<HashMap<String, Preset>>,
    pub invites: RwLock<HashMap<String, Invite>>,
    pub shares: RwLock<HashMap<String, ShareRecord>>, // share_id → record (LLD-19)
    pub sessions: RwLock<HashMap<String, String>>, // session_token → username
    pub config: HubConfig,
    pub signing: Option<SigningContext>,
}

/// Load state from disk and build AppState.
pub fn hydrate(config: HubConfig) -> Result<Arc<AppState>> {
    let data_dir = Path::new(&config.server.data_dir);

    // Ensure data directories exist.
    std::fs::create_dir_all(data_dir.join("presets"))?;
    std::fs::create_dir_all(data_dir.join("invites"))?;
    std::fs::create_dir_all(data_dir.join("shares"))?;

    let presets = storage::load_all_presets(data_dir)?;
    let invites = storage::load_all_invites(data_dir)?;
    let shares = storage::load_all_shares(data_dir)?;

    tracing::info!(
        "loaded {} presets, {} invites, {} shares from {}",
        presets.len(),
        invites.len(),
        shares.len(),
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
        sessions: RwLock::new(HashMap::new()),
        config,
        signing,
    }))
}
