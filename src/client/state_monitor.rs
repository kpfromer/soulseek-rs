use super::{ClientInner, ClientState};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

/// Events sent from ConnectedWorker → state_monitor.
pub enum WorkerEvent {
    ServerDisconnected,
    LoginSucceeded,
    DownloadSlotFreed,
}

/// Tiny background task: the only place that mutates `ClientInner.state`.
pub async fn state_monitor(
    mut event_rx: UnboundedReceiver<WorkerEvent>,
    inner: Arc<Mutex<ClientInner>>,
) {
    while let Some(event) = event_rx.recv().await {
        match event {
            WorkerEvent::ServerDisconnected => {
                inner.lock().unwrap_or_else(|e| e.into_inner()).state = ClientState::Disconnected;
            }
            WorkerEvent::LoginSucceeded => {
                inner.lock().unwrap_or_else(|e| e.into_inner()).state = ClientState::Connected;
            }
            WorkerEvent::DownloadSlotFreed => {
                // Download slot management is handled by ConnectedWorker; nothing to do here.
            }
        }
    }
}
