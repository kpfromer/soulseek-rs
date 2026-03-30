use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::download_slot::DownloadSlot;
use crate::actor::server_actor::ServerCommand;
use crate::actor::ActorHandle;
use crate::client::inner::{ClientInner, ClientState, PendingDownload};
use crate::client::{ClientContext, ClientOperation};
use crate::path::SoulseekPath;
use crate::token::{DownloadToken, PeerTransferToken, SearchToken};
use crate::types::DownloadStatus;
use crate::types::{Download, Search};
use crate::{debug, error, info, trace, warn};
use crate::peer::download_peer::spawn_direct_download;
use crate::peer::{ConnectionType, DownloadPeer, Peer};

/// Owns the incoming-operations loop for a live connection.
/// Handles all `ClientOperation` messages from actors (server, peers).
pub struct ConnectedWorker {
    pub own_username: String,
    /// Sender half — cloned into spawned closures so they can send back operations.
    pub op_tx: UnboundedSender<ClientOperation>,
    pub op_rx: UnboundedReceiver<ClientOperation>,
    /// Shared client state — worker mutates `.state` directly on connect/disconnect.
    pub inner: Arc<Mutex<ClientInner>>,
    pub context: Arc<ClientContext>,
    pub cancellation_token: CancellationToken,
    /// Handle to ServerActor — used to send commands (PierceFirewall, GetPeerAddress).
    pub server_handle: ActorHandle<ServerCommand>,
    // Download concurrency queue — the single source of truth for pending downloads.
    pub logged_in: bool,
    pub pending: VecDeque<PendingDownload>,
    pub max_concurrent: Option<u32>,
    /// Holds one [`DownloadSlot`] per in-flight download. `active_slots.len()` is the active count.
    /// Removing an entry drops the slot, freeing the concurrency slot automatically.
    pub active_slots: HashMap<DownloadToken, DownloadSlot>,
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
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).state =
                    ClientState::Disconnected;
            }
            ClientOperation::LoginSucceeded => {
                self.logged_in = true;
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).state =
                    ClientState::Connected;
                self.drain_pending_queue();
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
                self.active_slots.remove(&token);
                self.try_dequeue_next();
            }
            ClientOperation::RequestDownload(pd) => {
                // Insert immediately so it's visible to queries even while queued.
                self.downloads.insert(pd.token, pd.to_download());
                // Notify caller immediately that the download was accepted.
                let _ = pd.status_sender.send(DownloadStatus::Queued);
                if self.logged_in && self.max_concurrent.is_none_or(|max| (self.active_slots.len() as u32) < max) {
                    self.try_initiate(pd);
                } else {
                    self.pending.push_back(pd);
                }
            }
            ClientOperation::ConnectToPeer(peer) => {
                let connector = self.peer_connector();
                let downloads = self.downloads.clone();
                tokio::spawn(async move {
                    match peer.connection_type {
                        ConnectionType::P => connector.connect_p(peer, None),
                        ConnectionType::F => connector.connect_f(peer, downloads, None),
                        ConnectionType::D => error!("ConnectionType::D not implemented"),
                    }
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
                if let Some(ref error) = maybe_error {
                    warn!(
                        "[worker] Peer {} disconnected with error: {:?}",
                        username, error
                    );
                }
                // Fail any queued downloads for this peer regardless of whether the disconnect
                // was clean or due to an error — the peer is gone and will never send a
                // TransferRequest.
                self.process_failed_uploads(&username, None);
            }
            ClientOperation::PierceFireWall(peer) => {
                debug!("Piercing firewall for peer: {:?}", peer);
                if let Some(token) = peer.token {
                    if let Err(e) = self.server_handle.send(ServerCommand::PierceFirewall(token)) {
                        error!("Failed to send PierceFirewall message: {}", e);
                    }
                } else {
                    error!("No token available for PierceFirewall");
                }
                self.peer_connector().connect_f(peer, self.downloads.clone(), None);
            }
            ClientOperation::DownloadFromPeer(peer_transfer_token, peer, _allowed) => {
                let maybe_download = self.downloads.values()
                    .find(|d| d.peer_token == Some(peer_transfer_token))
                    .cloned();
                let own_username = self.own_username.clone();
                let op_tx = self.op_tx.clone();

                trace!(
                    "[worker] DownloadFromPeer peer_token: {} peer: {:?}",
                    peer_transfer_token, peer
                );

                match maybe_download {
                    Some(download) => {
                        spawn_direct_download(download, peer.host, peer.port, peer_transfer_token.0, own_username, None, op_tx);
                    }
                    None => {
                        error!("Can't find download with peer_token {:?}", peer_transfer_token);
                    }
                }
            }
            ClientOperation::NewPeer(new_peer) => {
                let peer_exists = self.context.peer_registry.contains(&new_peer.username);

                if peer_exists {
                    debug!("Already connected to {}", new_peer.username);
                } else {
                    self.server_handle
                        .send(ServerCommand::GetPeerAddress(new_peer.username.clone()))
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

                let connector = self.peer_connector();
                let downloads = self.downloads.clone();
                match peer.connection_type {
                    ConnectionType::P => connector.connect_p(peer, Some(new_peer.tcp_stream)),
                    ConnectionType::F => connector.connect_f(peer, downloads, Some(new_peer.tcp_stream)),
                    ConnectionType::D => error!("ConnectionType::D not implemented"),
                }
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
                    let connector = self.peer_connector();
                    tokio::spawn(async move { connector.connect_p(peer, None) });
                }
            }
            ClientOperation::UpdateDownloadTokens(transfer, username) => {
                if let Some(download) = self.downloads.values_mut().find(|d| {
                    d.username == username && d.filename == transfer.filename
                }) {
                    trace!(
                        "[worker] UpdateDownloadTokens: {} peer_token={} size={}",
                        download.token, transfer.token, transfer.size
                    );
                    download.peer_token = Some(transfer.token);
                    download.size = transfer.size;
                }
            }
            ClientOperation::UploadFailed(username, filename) => {
                self.process_failed_uploads(&username, Some(&filename));
            }
            ClientOperation::InitiateSearch(token, query) => {
                self.searches.insert(token, Search { token, query, results: vec![] });
            }
            ClientOperation::QueryDownloadByToken(peer_token, tx) => {
                let download = self.downloads.values()
                    .find(|d| d.peer_token == Some(peer_token))
                    .cloned();
                let _ = tx.send(download);
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
            ClientOperation::CancelDownload(token) => {
                // Remove from pending queue (not yet started).
                self.pending.retain(|pd| pd.token != token);
                // Remove from active downloads; notify caller and free the slot.
                if let Some(download) = self.downloads.remove(&token) {
                    let _ = download.sender.send(DownloadStatus::Cancelled);
                    if self.active_slots.remove(&token).is_some() {
                        self.try_dequeue_next();
                    }
                }
            }
        }
    }

    /// Initiate a download: ensure it's in the downloads map, queue upload with peer registry,
    /// and acquire a concurrency slot.
    fn try_initiate(&mut self, pd: PendingDownload) {
        // Insert/update so pre-pending items are also visible.
        self.downloads.insert(pd.token, pd.to_download());
        let _ = self.context.peer_registry.queue_upload(&pd.username, pd.filename.clone());
        self.active_slots.insert(pd.token, DownloadSlot);
    }

    /// Drain pending queue up to the concurrency limit.
    fn drain_pending_queue(&mut self) {
        loop {
            if self.max_concurrent.is_some_and(|max| (self.active_slots.len() as u32) >= max) {
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
        if self.max_concurrent.is_some_and(|max| (self.active_slots.len() as u32) >= max) {
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
        let any_failed = !failed_tokens.is_empty();
        for token in failed_tokens {
            self.downloads.remove(&token);
            self.active_slots.remove(&token); // slot drops → counter decremented
        }
        if any_failed {
            self.try_dequeue_next();
        }
    }

    fn peer_connector(&self) -> PeerConnector {
        PeerConnector {
            context: self.context.clone(),
            own_username: self.own_username.clone(),
            op_tx: self.op_tx.clone(),
        }
    }
}

/// Encapsulates the shared context needed to initiate a peer connection,
/// eliminating repetitive parameter passing across the four call sites.
pub(super) struct PeerConnector {
    context: Arc<ClientContext>,
    own_username: String,
    op_tx: UnboundedSender<ClientOperation>,
}

impl PeerConnector {
    /// Register a P-type (messaging) peer connection.
    pub fn connect_p(&self, peer: Peer, stream: Option<std::net::TcpStream>) {
        let username = peer.username.clone();
        trace!(
            "[worker] connecting P-type to {}, token {:?}",
            username, peer.token
        );
        let tokio_stream = stream.and_then(|s| {
            s.set_nonblocking(true).ok();
            tokio::net::TcpStream::from_std(s).ok()
        });
        if let Err(e) = self.context.peer_registry.register_peer(peer, tokio_stream, None) {
            trace!("Failed to spawn peer actor for {:?}: {:?}", username, e);
        }
    }

    /// Initiate an F-type (file transfer / pierce-firewall) download connection.
    ///
    /// `downloads` is a snapshot of the current download map used to resolve the
    /// wire token sent by the peer. `stream` is `Some` when the peer is already
    /// connected (inbound); `None` when we need to dial out.
    pub fn connect_f(
        &self,
        peer: Peer,
        downloads: HashMap<DownloadToken, Download>,
        stream: Option<std::net::TcpStream>,
    ) {
        trace!(
            "[worker] downloading F-type from: {}, {:?}",
            peer.username, peer.token
        );
        // Build a peer-token-keyed snapshot for the resolve closure.
        let peer_downloads: HashMap<PeerTransferToken, Download> = downloads
            .values()
            .filter_map(|d| d.peer_token.map(|pt| (pt, d.clone())))
            .collect();

        // Capture the initiating DownloadToken (by username) for pre-token failure reporting.
        let initiating_token = downloads
            .values()
            .find(|d| d.username == peer.username)
            .map(|d| d.token);

        let own_username = self.own_username.clone();
        let op_tx = self.op_tx.clone();
        let download_peer = DownloadPeer::new(
            peer.username,
            peer.host,
            peer.port,
            peer.token.unwrap().0,
            own_username,
        );
        tokio::task::spawn_blocking(move || {
            let peer_downloads_for_resolve = peer_downloads.clone();
            let resolve = move |token: PeerTransferToken| peer_downloads_for_resolve.get(&token).cloned();
            match download_peer.download_pierced(resolve, stream) {
                Ok((download, path)) => {
                    trace!("[worker] pierced download complete: {}", path);
                    // download.token is our DownloadToken — always stable
                    let _ = op_tx
                        .send(ClientOperation::DownloadCompleted(download.token, Ok(path)));
                }
                Err((Some(peer_token), crate::peer::download_peer::DownloadError::Cancelled)) => {
                    trace!("[worker] pierced download cancelled");
                    let our_token = peer_downloads.get(&peer_token).map(|d| d.token);
                    if let Some(token) = our_token.or(initiating_token) {
                        let _ = op_tx.send(ClientOperation::DownloadCompleted(
                            token,
                            Err(crate::error::SoulseekRs::DownloadCancelled),
                        ));
                    }
                }
                Err((Some(peer_token), crate::peer::download_peer::DownloadError::NoProgressTimeout)) => {
                    trace!("[worker] pierced download timed out");
                    let our_token = peer_downloads.get(&peer_token).map(|d| d.token);
                    if let Some(token) = our_token.or(initiating_token) {
                        let _ = op_tx.send(ClientOperation::DownloadCompleted(
                            token,
                            Err(crate::error::SoulseekRs::DownloadTimedOut),
                        ));
                    }
                }
                Err((Some(peer_token), e)) => {
                    error!("[worker] pierced download failed: {}", e);
                    let our_token = peer_downloads.get(&peer_token).map(|d| d.token);
                    if let Some(token) = our_token.or(initiating_token) {
                        let _ = op_tx.send(ClientOperation::DownloadCompleted(
                            token,
                            Err(crate::error::SoulseekRs::InvalidMessage(e.to_string())),
                        ));
                    }
                }
                Err((None, e)) => {
                    warn!("[worker] pierce-firewall pre-token failure: {}", e);
                    if let Some(token) = initiating_token {
                        let _ = op_tx.send(ClientOperation::DownloadCompleted(
                            token,
                            Err(crate::error::SoulseekRs::InvalidMessage(e.to_string())),
                        ));
                    }
                }
            }
        });
    }
}
