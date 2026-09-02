use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{mpsc, watch};

use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;

use crate::session::persistence::PersistenceMsg;

pub(crate) struct NotificationSender {
    pub gateway: GatewaySender,
    /// When false, notifications are persisted but NOT forwarded to the client.
    /// Opened by `MvpAgent::load_session` when the client explicitly loads the session.
    pub gateway_enabled: Arc<AtomicBool>,
    pub persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
    pub disk_full: watch::Receiver<bool>,
}

impl NotificationSender {
    pub(crate) fn is_disk_full(&self) -> bool {
        *self.disk_full.borrow()
    }
}

#[cfg(test)]
pub(crate) fn idle_disk_full_rx() -> watch::Receiver<bool> {
    watch::channel(false).1
}
