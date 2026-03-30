use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::state_monitor::WorkerEvent;
use crate::actor::server_actor::ServerMessage;
use crate::client::inner::PendingDownload;
use crate::client::{ClientContext, ClientOperation};
use crate::path::SoulseekPath;
use crate::token::{DownloadToken, SearchToken};
use crate::types::DownloadStatus;
use crate::types::{Download, Search};
use crate::{debug, error, info, trace, warn};
use crate::peer::{ConnectionType, DownloadPeer, Peer};

/// Owns the incoming-operations loop for a live connection.
/// Handles all `ClientOperation` messages from actors (server, peers).
/// State-transition events are forwarded to `state_monitor` via `event_tx`.
pub struct ConnectedWorker {
    pub own_username: String,
    /// Sender half — cloned into spawned closures so they can send back operations.
    pub op_tx: UnboundedSender<ClientOperation>,
    pub op_rx: UnboundedReceiver<ClientOperation>,
    pub event_tx: UnboundedSender<WorkerEvent>,
    pub context: Arc<ClientContext>,
    pub cancellation_token: CancellationToken,
    /// Sender to the ServerActor dispatcher. Populated by `SetServerSender` on first connect.
    pub server_sender: Option<UnboundedSender<ServerMessage>>,
    // Download concurrency queue — the single source of truth for pending downloads.
    pub logged_in: bool,
    pub pending: VecDeque<PendingDownload>,
    pub max_concurrent: Option<u32>,
    pub active_downloads: u32,
    /// All known downloads (queued or in-flight).
    pub downloads: HashMap<DownloadToken, Download>,
    /// All active searches keyed by token.
    pub searches: HashMap<SearchToken, Search>,
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
                    Err(crate::error::SoulseekRs::DownloadCancelled) => DownloadStatus::Cancelled,
                    Err(crate::error::SoulseekRs::DownloadTimedOut) => DownloadStatus::TimedOut,
                    Err(ref e) => {
                        error!("Download failed: {}", e);
                        DownloadStatus::Failed
                    }
                };
                if let Some(download) = self.downloads.get(&token) {
                    let _ = download.sender.send(status.clone());
                }
                if let Some(download) = self.downloads.get_mut(&token) {
                    download.status = status;
                }
                self.active_downloads = self.active_downloads.saturating_sub(1);
                self.try_dequeue_next();
            }
            ClientOperation::RequestDownload(pd) => {
                // Insert immediately so it's visible to queries even while queued.
                self.downloads.insert(pd.token, pd.to_download());
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
                let downloads = self.downloads.clone();
                tokio::spawn(async move {
                    Self::connect_to_peer(peer, context, own_username, None, op_tx, downloads);
                });
            }
            ClientOperation::SearchResult(search_result) => {
                trace!("[worker] SearchResult {:?}", search_result);
                if let Some(search) = self.searches.get_mut(&search_result.token) {
                    search.results.push(search_result);
                }
            }
            ClientOperation::PeerDisconnected(username, maybe_error) => {
                if let Some(handle) = self.context.peer_registry.remove_peer(&username) {
                    let _ = handle.stop();
                }
                if let Some(error) = maybe_error {
                    warn!(
                        "[worker] Peer {} disconnected with error: {:?}",
                        username, error
                    );
                    self.process_failed_uploads(&username, None);
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
                    self.downloads.clone(),
                );
            }
            ClientOperation::DownloadFromPeer(token, peer, _allowed) => {
                let maybe_download = self.downloads.get(&token).cloned();
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
                                    use crate::peer::download_peer::DownloadError;
                                    match e {
                                        DownloadError::Cancelled => {
                                            crate::error::SoulseekRs::DownloadCancelled
                                        }
                                        DownloadError::NoProgressTimeout => {
                                            crate::error::SoulseekRs::DownloadTimedOut
                                        }
                                        other => {
                                            error!(
                                                "Failed to download '{}' from {}:{} (token: {}): {}",
                                                download.filename, peer.host, peer.port,
                                                download.token, other
                                            );
                                            crate::error::SoulseekRs::InvalidMessage(
                                                other.to_string(),
                                            )
                                        }
                                    }
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
                let peer_exists = self.context.peer_registry.contains(&new_peer.username);

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
                    self.downloads.clone(),
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

                let peer_exists = self.context.peer_registry.contains(&username);

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
                    let downloads = self.downloads.clone();
                    tokio::spawn(async move {
                        Self::connect_to_peer(peer, context, own_username, None, op_tx, downloads);
                    });
                }
            }
            ClientOperation::UpdateDownloadTokens(transfer, username) => {
                let download_to_update = self.downloads.values().find_map(|d| {
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
                    self.downloads.insert(transfer.token, Download {
                        username: username.clone(),
                        filename: transfer.filename,
                        token: transfer.token,
                        size: transfer.size,
                        download_directory: download.download_directory,
                        status: download.status.clone(),
                        sender: download.sender.clone(),
                        cancel: download.cancel.clone(),
                        progress_timeout: download.progress_timeout,
                    });
                    self.downloads.remove(&old_token);
                }
            }
            ClientOperation::UploadFailed(username, filename) => {
                self.process_failed_uploads(&username, Some(&filename));
            }
            ClientOperation::PierceFirewallPreTokenFailed => {
                self.active_downloads = self.active_downloads.saturating_sub(1);
                self.try_dequeue_next();
            }
            ClientOperation::SetServerSender(sender) => {
                self.server_sender = Some(sender);
                debug!("[worker] Server sender initialized");
            }
            ClientOperation::InitiateSearch(token, query) => {
                self.searches.insert(token, Search { token, query, results: vec![] });
            }
            ClientOperation::QueryDownloadByToken(token, tx) => {
                let _ = tx.send(self.downloads.get(&token).cloned());
            }
            ClientOperation::QueryDownloads(tx) => {
                let _ = tx.send(self.downloads.values().cloned().collect());
            }
            ClientOperation::QuerySearchResults(query, tx) => {
                let _ = tx.send(
                    self.searches
                        .values()
                        .find(|s| s.query == query)
                        .map(|s| s.results.clone())
                        .unwrap_or_default(),
                );
            }
        }
    }

    /// Initiate a download: ensure it's in the downloads map, queue upload with peer registry,
    /// and increment active_downloads counter.
    fn try_initiate(&mut self, pd: PendingDownload) {
        // Insert/update so pre-pending items are also visible.
        self.downloads.insert(pd.token, pd.to_download());
        let _ = self.context.peer_registry.queue_upload(&pd.username, pd.filename.clone());
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

    fn process_failed_uploads(&mut self, username: &str, filename: Option<&SoulseekPath>) {
        let failed_tokens: Vec<_> = self
            .downloads
            .values()
            .filter(|d| d.username == username && filename.is_none_or(|f| d.filename == *f))
            .map(|d| {
                let _ = d.sender.send(DownloadStatus::Failed);
                d.token
            })
            .collect();
        let count = failed_tokens.len();
        for token in failed_tokens {
            self.downloads.remove(&token);
        }
        self.active_downloads = self.active_downloads.saturating_sub(count as u32);
        if count > 0 {
            self.try_dequeue_next();
        }
    }

    fn connect_to_peer(
        peer: Peer,
        context: Arc<ClientContext>,
        own_username: String,
        stream: Option<std::net::TcpStream>,
        op_tx: UnboundedSender<ClientOperation>,
        downloads: HashMap<DownloadToken, Download>,
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
                match context.peer_registry.register_peer(peer_clone, tokio_stream, None) {
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
                    let resolve = move |token: DownloadToken| downloads.get(&token).cloned();
                    match download_peer.download_pierced(resolve, stream) {
                        Ok((download, path)) => {
                            trace!("[worker] downloaded {} bytes {:?}", path, download.size);
                            let _ = op_tx
                                .send(ClientOperation::DownloadCompleted(download.token, Ok(path)));
                        }
                        Err((
                            Some(token),
                            crate::peer::download_peer::DownloadError::Cancelled,
                        )) => {
                            trace!("[worker] pierced download cancelled");
                            let _ = op_tx.send(ClientOperation::DownloadCompleted(
                                token,
                                Err(crate::error::SoulseekRs::DownloadCancelled),
                            ));
                        }
                        Err((
                            Some(token),
                            crate::peer::download_peer::DownloadError::NoProgressTimeout,
                        )) => {
                            trace!("[worker] pierced download timed out");
                            let _ = op_tx.send(ClientOperation::DownloadCompleted(
                                token,
                                Err(crate::error::SoulseekRs::DownloadTimedOut),
                            ));
                        }
                        Err((Some(token), e)) => {
                            error!("[worker] pierced download failed: {}", e);
                            let _ = op_tx.send(ClientOperation::DownloadCompleted(
                                token,
                                Err(crate::error::SoulseekRs::InvalidMessage(e.to_string())),
                            ));
                        }
                        Err((None, e)) => {
                            warn!("[worker] pierce-firewall pre-token failure: {}", e);
                            let _ = op_tx.send(ClientOperation::PierceFirewallPreTokenFailed);
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
