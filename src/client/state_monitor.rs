use super::{ClientInner, ClientState, PendingDownload};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

/// Events sent from ConnectedWorker → state_monitor.
pub enum WorkerEvent {
    ServerDisconnected,
    LoginSucceeded,
    DownloadSlotFreed,
}

/// Tiny background task: the only place that mutates `ClientInner.state`.
/// Also handles pending-download replay and concurrency-limiter dequeue.
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
                let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                guard.state = ClientState::Connected;

                let pending: Vec<PendingDownload> = guard.pending_downloads.drain(..).collect();
                // Clone Arc so we don't hold a borrow of guard.active while mutating guard.download_limiter
                let ctx_arc = guard.active.as_ref().map(|a| a.context.clone());

                if !pending.is_empty() {
                    if let Some(ctx_arc) = ctx_arc {
                        let ctx = ctx_arc.read().unwrap_or_else(|e| e.into_inner());
                        if let Some(ref registry) = ctx.peer_registry {
                            for pd in pending {
                                if let Some(ref mut lim) = guard.download_limiter {
                                    if lim.active < lim.max_concurrent {
                                        lim.active += 1;
                                        let _ = registry.queue_upload(&pd.username, pd.filename);
                                    } else {
                                        lim.queue.push_back(pd);
                                    }
                                } else {
                                    let _ = registry.queue_upload(&pd.username, pd.filename);
                                }
                            }
                        }
                    }
                }
            }
            WorkerEvent::DownloadSlotFreed => {
                let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                // Separate lim mutations from guard.active borrow to avoid conflict
                let (next_pd, ctx_arc) = {
                    let next = if let Some(ref mut lim) = guard.download_limiter {
                        lim.active = lim.active.saturating_sub(1);
                        if let Some(pd) = lim.queue.pop_front() {
                            lim.active += 1;
                            Some(pd)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    // lim borrow ends; clone Arc before guard.active borrow ends too
                    let ctx_arc = guard.active.as_ref().map(|a| a.context.clone());
                    (next, ctx_arc)
                };
                if let (Some(pd), Some(ctx_arc)) = (next_pd, ctx_arc) {
                    let ctx = ctx_arc.read().unwrap_or_else(|e| e.into_inner());
                    if let Some(ref registry) = ctx.peer_registry {
                        let _ = registry.queue_upload(&pd.username, pd.filename);
                    }
                }
            }
        }
    }
}
