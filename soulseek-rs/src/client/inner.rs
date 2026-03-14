use std::collections::VecDeque;

use crate::DownloadStatus;
use crate::actor::server_actor::ServerMessage;
use crate::actor::{ActorHandle, ActorSystem};
use crate::client::ClientContext;
use crate::client::ClientOperation;
use crate::path::SoulseekPath;
use crate::search_rate_limiter::SlidingRateLimiter;
use crate::token::DownloadToken;
use crate::types::Download;
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
    pub filename: SoulseekPath,
    pub username: String,
    pub size: u64,
    pub download_directory: String,
    pub token: DownloadToken,
    pub status_sender: UnboundedSender<DownloadStatus>,
}

impl PendingDownload {
    pub fn to_download(&self) -> Download {
        Download {
            username: self.username.clone(),
            filename: self.filename.clone(),
            token: self.token,
            size: self.size,
            download_directory: self.download_directory.clone(),
            status: DownloadStatus::Queued,
            sender: self.status_sender.clone(),
        }
    }
}

/// Holds all live-connection resources. Created on connect, persists across disconnects
/// (ServerActor handles auto-reconnect). Only cleared when a new connect() is initiated.
pub struct ActiveConnection {
    pub server_handle: ActorHandle<ServerMessage>,
    pub context: Arc<RwLock<ClientContext>>,
    /// Sender to the ConnectedWorker operations channel.
    pub op_tx: UnboundedSender<ClientOperation>,
    /// Actor system — used for shutdown.
    pub actor_system: Arc<ActorSystem>,
}

/// All mutable state behind a single lock.
pub struct ClientInner {
    pub state: ClientState,
    /// Present after the first successful connect(); persists across disconnects.
    pub active: Option<ActiveConnection>,
    /// Downloads queued before connect() is ever called; seeded into the worker on first connect.
    pub pending_downloads: VecDeque<PendingDownload>,
    pub search_limiter: Option<SlidingRateLimiter>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.actor_system.shutdown();
        }
    }
}
