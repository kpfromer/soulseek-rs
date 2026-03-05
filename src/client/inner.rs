use std::collections::VecDeque;

use crate::DownloadStatus;
use crate::actor::ActorHandle;
use crate::actor::server_actor::ServerMessage;
use crate::client::ClientContext;
use crate::client::DownloadConcurrencyLimiter;
use crate::search_rate_limiter::SlidingRateLimiter;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::mpsc::UnboundedSender;

pub enum ClientState {
    Disconnected,
    Connecting,
    Connected,
}

#[allow(dead_code)]
pub struct PendingDownload {
    pub filename: String,
    pub username: String,
    pub size: u64,
    pub download_directory: String,
    pub token: u32,
    pub status_sender: UnboundedSender<DownloadStatus>,
}

/// Holds all live-connection resources. Created on connect, persists across disconnects
/// (ServerActor handles auto-reconnect). Only cleared when a new connect() is initiated.
pub struct ActiveConnection {
    pub server_handle: ActorHandle<ServerMessage>,
    pub context: Arc<RwLock<ClientContext>>,
}

/// All mutable state behind a single lock.
pub struct ClientInner {
    pub state: ClientState,
    /// Present after the first successful connect(); persists across disconnects.
    pub active: Option<ActiveConnection>,
    /// Downloads queued while disconnected; replayed on LoginSucceeded.
    pub pending_downloads: VecDeque<PendingDownload>,
    pub search_limiter: Option<SlidingRateLimiter>,
    pub download_limiter: Option<DownloadConcurrencyLimiter>,
}
