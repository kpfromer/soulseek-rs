use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::RwLock;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::state_monitor::WorkerEvent;
use crate::actor::server_actor::ServerMessage;
use crate::client::inner::PendingDownload;
use crate::client::{ClientContext, ClientOperation};
use crate::path::SoulseekPath;
use crate::types::DownloadStatus;
use crate::{debug, error, info, trace, warn};
use crate::{
    peer::{ConnectionType, DownloadPeer, Peer},
    types::Download,
};

/// Owns the incoming-operations loop for a live connection.
/// Handles all `ClientOperation` messages from actors (server, peers).
/// State-transition events are forwarded to `state_monitor` via `event_tx`.
pub struct ConnectedWorker {
    pub own_username: String,
    /// Sender half — cloned into spawned closures so they can send back operations.
    pub op_tx: UnboundedSender<ClientOperation>,
    pub op_rx: UnboundedReceiver<ClientOperation>,
    pub event_tx: UnboundedSender<WorkerEvent>,
    pub context: Arc<RwLock<ClientContext>>,
    pub cancellation_token: CancellationToken,
    /// Sender to the ServerActor dispatcher. Populated by `SetServerSender` on first connect.
    pub server_sender: Option<UnboundedSender<ServerMessage>>,
    // Download concurrency queue — the single source of truth for pending downloads.
    pub logged_in: bool,
    pub pending: VecDeque<PendingDownload>,
    pub max_concurrent: Option<u32>,
    pub active_downloads: u32,
}

impl ConnectedWorker {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    trace!("[worker] Shutdown signal received");
                    break;
                }
                op = self.op_rx.recv() => {
                    match op {
                        Some(op) => self.handle_operation(op).await,
                        None => {
                            error!("[worker] Channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn handle_operation(&mut self, op: ClientOperation) {
        match op {
            ClientOperation::ServerDisconnected => {
                self.logged_in = false;
                if let Err(e) = self.event_tx.send(WorkerEvent::ServerDisconnected) {
                    error!("[worker] Failed to forward ServerDisconnected: {}", e);
                }
            }
            ClientOperation::LoginSucceeded => {
                self.logged_in = true;
                self.drain_pending_queue();
                if let Err(e) = self.event_tx.send(WorkerEvent::LoginSucceeded) {
                    error!("[worker] Failed to forward LoginSucceeded: {}", e);
                }
            }
            ClientOperation::DownloadCompleted(token, result) => {
                let status = match result {
                    Ok(ref path) => {
                        info!("Successfully downloaded to {}", path);
                        DownloadStatus::Completed
                    }
                    Err(ref e) => {
                        error!("Download failed: {}", e);
                        DownloadStatus::Failed
                    }
                };
                {
                    let mut ctx = self.context.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(download) = ctx.get_download_by_token(token) {
                        let _ = download.sender.send(status.clone());
                    }
                    ctx.update_download_with_status(token, status);
                }
                self.active_downloads = self.active_downloads.saturating_sub(1);
                self.try_dequeue_next();
            }
            ClientOperation::RequestDownload(pd) => {
                if self.logged_in && self.max_concurrent.is_none_or(|max| self.active_downloads < max) {
                    self.try_initiate(pd);
                } else {
                    self.pending.push_back(pd);
                }
            }
            ClientOperation::ConnectToPeer(peer) => {
                let context = self.context.clone();
                let own_username = self.own_username.clone();
                let op_tx = self.op_tx.clone();
                tokio::spawn(async move {
                    Self::connect_to_peer(peer, context, own_username, None, op_tx);
                });
            }
            ClientOperation::SearchResult(search_result) => {
                trace!("[worker] SearchResult {:?}", search_result);
                let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());
                let result_token = search_result.token;
                for search in context.searches.values_mut() {
                    if search.token == result_token {
                        search.results.push(search_result);
                        break;
                    }
                }
            }
            ClientOperation::PeerDisconnected(username, maybe_error) => {
                {
                    let context = self.context.read().unwrap_or_else(|e| e.into_inner());
                    if let Some(handle) = context.peer_registry.remove_peer(&username) {
                        let _ = handle.stop();
                    }
                }
                if let Some(error) = maybe_error {
                    warn!(
                        "[worker] Peer {} disconnected with error: {:?}",
                        username, error
                    );
                    Self::process_failed_uploads(self.context.clone(), &username, None);
                }
            }
            ClientOperation::PierceFireWall(peer) => {
                debug!("Piercing firewall for peer: {:?}", peer);
                if let Some(ref ss) = self.server_sender {
                    if let Some(token) = peer.token {
                        if let Err(e) = ss.send(ServerMessage::PierceFirewall(token)) {
                            error!("Failed to send PierceFirewall message: {}", e);
                        }
                    } else {
                        error!("No token available for PierceFirewall");
                    }
                } else {
                    error!("No server sender available for PierceFirewall");
                }
                Self::connect_to_peer(
                    peer,
                    self.context.clone(),
                    self.own_username.clone(),
                    None,
                    self.op_tx.clone(),
                );
            }
            ClientOperation::DownloadFromPeer(token, peer, _allowed) => {
                let maybe_download = {
                    let ctx = self.context.read().unwrap_or_else(|e| e.into_inner());
                    ctx.get_download_by_token(token).cloned()
                };
                let own_username = self.own_username.clone();
                let op_tx = self.op_tx.clone();

                trace!(
                    "[worker] DownloadFromPeer token: {} peer: {:?}",
                    token, peer
                );

                match maybe_download {
                    Some(download) => {
                        tokio::task::spawn_blocking(move || {
                            let download_peer = DownloadPeer::new(
                                download.username.clone(),
                                peer.host.clone(),
                                peer.port,
                                token.0,
                                own_username,
                            );
                            let result = download_peer
                                .download_direct(download.clone(), None)
                                .map(|(_, path)| path)
                                .map_err(|e| {
                                    error!(
                                        "Failed to download '{}' from {}:{} (token: {}): {}",
                                        download.filename, peer.host, peer.port, download.token, e
                                    );
                                    crate::error::SoulseekRs::InvalidMessage(e.to_string())
                                });
                            let _ = op_tx.send(ClientOperation::DownloadCompleted(token, result));
                        });
                    }
                    None => {
                        error!("Can't find download with token {:?}", token);
                    }
                }
            }
            ClientOperation::NewPeer(new_peer) => {
                let peer_exists = self
                    .context
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .peer_registry
                    .contains(&new_peer.username);

                if peer_exists {
                    debug!("Already connected to {}", new_peer.username);
                } else if let Some(ref server_sender) = self.server_sender {
                    server_sender
                        .send(ServerMessage::GetPeerAddress(new_peer.username.clone()))
                        .unwrap_or_else(|e| {
                            error!("[worker] Failed to send GetPeerAddress: {}", e)
                        });
                }

                let addr = new_peer.tcp_stream.peer_addr().unwrap();
                let host = addr.ip().to_string();
                let port: u32 = addr.port().into();

                let peer = Peer {
                    username: new_peer.username.clone(),
                    connection_type: new_peer.connection_type,
                    host,
                    port,
                    token: Some(new_peer.token),
                    privileged: None,
                    obfuscated_port: None,
                    unknown: None,
                };

                Self::connect_to_peer(
                    peer,
                    self.context.clone(),
                    self.own_username.clone(),
                    Some(new_peer.tcp_stream),
                    self.op_tx.clone(),
                );
            }
            ClientOperation::GetPeerAddressResponse {
                username,
                host,
                port,
                obfuscation_type,
                obfuscated_port,
            } => {
                debug!(
                    "Received peer address for {}: {}:{} (obf_type: {}, obf_port: {})",
                    username, host, port, obfuscation_type, obfuscated_port
                );

                let peer_exists = self
                    .context
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .peer_registry
                    .contains(&username);

                if !peer_exists {
                    let peer = Peer::new(
                        username,
                        ConnectionType::P,
                        host,
                        port,
                        None,
                        0,
                        obfuscation_type.try_into().unwrap(),
                        obfuscated_port.try_into().unwrap(),
                    );
                    let context = self.context.clone();
                    let own_username = self.own_username.clone();
                    let op_tx = self.op_tx.clone();
                    tokio::spawn(async move {
                        Self::connect_to_peer(peer, context, own_username, None, op_tx);
                    });
                }
            }
            ClientOperation::UpdateDownloadTokens(transfer, username) => {
                let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());

                let download_to_update = context.get_downloads().into_iter().find_map(|d| {
                    if d.username == username && d.filename == transfer.filename {
                        Some((d.token, d.clone()))
                    } else {
                        None
                    }
                });

                if let Some((old_token, download)) = download_to_update {
                    trace!(
                        "[worker] UpdateDownloadTokens found {old_token}, transfer: {:?}",
                        transfer
                    );
                    context.add_download(Download {
                        username: username.clone(),
                        filename: transfer.filename,
                        token: transfer.token,
                        size: transfer.size,
                        download_directory: download.download_directory,
                        status: download.status.clone(),
                        sender: download.sender.clone(),
                    });
                    context.remove_download(old_token);
                }
            }
            ClientOperation::UploadFailed(username, filename) => {
                Self::process_failed_uploads(self.context.clone(), &username, Some(&filename));
            }
            ClientOperation::SetServerSender(sender) => {
                self.server_sender = Some(sender);
                debug!("[worker] Server sender initialized");
            }
        }
    }

    /// Initiate a download: add to context and queue upload with the peer registry.
    fn try_initiate(&mut self, pd: PendingDownload) {
        let mut ctx = self.context.write().unwrap_or_else(|e| e.into_inner());
        ctx.add_download(pd.to_download());
        let _ = ctx.peer_registry.queue_upload(&pd.username, pd.filename.clone());
        self.active_downloads += 1;
    }

    /// Drain pending queue up to the concurrency limit.
    fn drain_pending_queue(&mut self) {
        loop {
            if self.max_concurrent.is_some_and(|max| self.active_downloads >= max) {
                break;
            }
            match self.pending.pop_front() {
                Some(pd) => self.try_initiate(pd),
                None => break,
            }
        }
    }

    /// Dequeue and initiate the next pending download if a slot is available.
    fn try_dequeue_next(&mut self) {
        if !self.logged_in {
            return;
        }
        if self.max_concurrent.is_some_and(|max| self.active_downloads >= max) {
            return;
        }
        if let Some(pd) = self.pending.pop_front() {
            self.try_initiate(pd);
        }
    }

    fn process_failed_uploads(
        context: Arc<RwLock<ClientContext>>,
        username: &str,
        filename: Option<&SoulseekPath>,
    ) {
        let mut ctx = context.write().unwrap_or_else(|e| e.into_inner());
        let failed_tokens: Vec<_> = ctx
            .downloads
            .values()
            .filter(|d| d.username == username && filename.is_none_or(|f| d.filename == *f))
            .map(|d| {
                let _ = d.sender.send(DownloadStatus::Failed);
                d.token
            })
            .collect();
        for token in failed_tokens {
            ctx.remove_download(token);
        }
    }

    fn connect_to_peer(
        peer: Peer,
        context: Arc<RwLock<ClientContext>>,
        own_username: String,
        stream: Option<std::net::TcpStream>,
        op_tx: UnboundedSender<ClientOperation>,
    ) {
        let peer_clone = peer.clone();
        trace!(
            "[worker] connecting to {}, with connection_type: {}, and token {:?}",
            peer.username, peer.connection_type, peer.token
        );
        match peer.connection_type {
            ConnectionType::P => {
                let username = peer.username.clone();
                let tokio_stream = stream.and_then(|s| {
                    s.set_nonblocking(true).ok();
                    tokio::net::TcpStream::from_std(s).ok()
                });
                let ctx = context.read().unwrap_or_else(|e| e.into_inner());
                match ctx.peer_registry.register_peer(peer_clone, tokio_stream, None) {
                    Ok(_) => (),
                    Err(e) => {
                        trace!("Failed to spawn peer actor for {:?}: {:?}", username, e);
                    }
                }
            }
            ConnectionType::F => {
                trace!(
                    "[worker] downloading from: {}, {:?}",
                    peer.username, peer.token
                );
                let download_peer = DownloadPeer::new(
                    peer.username,
                    peer.host,
                    peer.port,
                    peer.token.unwrap().0,
                    own_username,
                );
                tokio::task::spawn_blocking(move || {
                    let resolve = {
                        let ctx = context.clone();
                        move |token| {
                            ctx.read()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_download_by_token(token)
                                .cloned()
                        }
                    };
                    let result = download_peer
                        .download_pierced(resolve, stream)
                        .map(|(download, path)| {
                            trace!("[worker] downloaded {} bytes {:?}", path, download.size);
                            (download.token, path)
                        });
                    match result {
                        Ok((token, path)) => {
                            let _ = op_tx.send(ClientOperation::DownloadCompleted(token, Ok(path)));
                        }
                        Err(e) => {
                            trace!("[worker] failed to download: {}", e);
                            // No token available from pierce failure — slot freed without context update.
                        }
                    }
                });
            }
            ConnectionType::D => {
                error!("ConnectionType::D not implemented")
            }
        }
    }
}
