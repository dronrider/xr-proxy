//! VPN connection state management.

use std::sync::Arc;
use tokio::sync::watch;

/// VPN connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VpnState {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error(String),
}

impl std::fmt::Display for VpnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VpnState::Disconnected => write!(f, "Disconnected"),
            VpnState::Connecting => write!(f, "Connecting"),
            VpnState::Connected => write!(f, "Connected"),
            VpnState::Disconnecting => write!(f, "Disconnecting"),
            VpnState::Error(e) => write!(f, "Error: {}", e),
        }
    }
}

/// Observable state holder — allows UI to subscribe to state changes.
#[derive(Clone)]
pub struct StateHandle {
    tx: Arc<watch::Sender<VpnState>>,
    rx: watch::Receiver<VpnState>,
}

impl StateHandle {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(VpnState::Disconnected);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    pub fn set(&self, state: VpnState) {
        tracing::info!("VPN state: {}", state);
        let _ = self.tx.send(state);
    }

    pub fn get(&self) -> VpnState {
        self.rx.borrow().clone()
    }

    /// Wait for the next state change.
    pub async fn changed(&mut self) -> VpnState {
        let _ = self.rx.changed().await;
        self.rx.borrow().clone()
    }
}
