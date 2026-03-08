use super::state_monitor::WorkerEvent;
use crate::actor::server_actor::ServerMessage;
use crate::client::{ClientContext, ClientOperation};
use crate::types::DownloadStatus;
use crate::{
    peer::{ConnectionType, DownloadPeer, Peer},
    types::Download,
};
use crate::{debug, error, info, trace, warn};
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Owns the incoming-operations loop for a live connection.
/// Handles all `ClientOperation` messages from actors (server, peers).
/// State-transition events are forwarded to `state_monitor` via `event_tx`.
pub struct ConnectedWorker {
    pub own_username: String,
    pub op_rx: UnboundedReceiver<ClientOperation>,
    pub event_tx: UnboundedSender<WorkerEvent>,
    pub context: Arc<RwLock<ClientContext>>,
    pub cancellation_token: CancellationToken,
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
                if let Err(e) = self.event_tx.send(WorkerEvent::ServerDisconnected) {
                    error!("[worker] Failed to forward ServerDisconnected: {}", e);
                }
            }
            ClientOperation::LoginSucceeded => {
                if let Err(e) = self.event_tx.send(WorkerEvent::LoginSucceeded) {
                    error!("[worker] Failed to forward LoginSucceeded: {}", e);
                }
            }
            ClientOperation::DownloadSlotFreed => {
                if let Err(e) = self.event_tx.send(WorkerEvent::DownloadSlotFreed) {
                    error!("[worker] Failed to forward DownloadSlotFreed: {}", e);
                }
            }
            ClientOperation::ConnectToPeer(peer) => {
                let context = self.context.clone();
                let own_username = self.own_username.clone();
                tokio::spawn(async move {
                    Self::connect_to_peer(peer, context, own_username, None);
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
                    if let Some(ref registry) = context.peer_registry
                        && let Some(handle) = registry.remove_peer(&username)
                    {
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
                Self::pierce_firewall(peer, self.context.clone(), self.own_username.clone());
            }
            ClientOperation::DownloadFromPeer(token, peer, allowed) => {
                let maybe_download = {
                    let ctx = self.context.read().unwrap_or_else(|e| e.into_inner());
                    ctx.get_download_by_token(token).cloned()
                };
                let own_username = self.own_username.clone();
                let context = self.context.clone();

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
                                token,
                                allowed,
                                own_username,
                            );
                            let filename: Option<&str> = download.filename.split('\\').next_back();
                            match filename {
                                Some(filename) => {
                                    match download_peer.download_file(
                                        context.clone(),
                                        Some(download.clone()),
                                        None,
                                    ) {
                                        Ok((download, filename)) => {
                                            let _ = download.sender.send(DownloadStatus::Completed);
                                            context
                                                .write()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .update_download_with_status(
                                                    download.token,
                                                    DownloadStatus::Completed,
                                                );
                                            info!(
                                                "Successfully downloaded {} bytes to {}",
                                                download.size, filename
                                            );
                                        }
                                        Err(e) => {
                                            let _ = download.sender.send(DownloadStatus::Failed);
                                            context
                                                .write()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .update_download_with_status(
                                                    download.token,
                                                    DownloadStatus::Failed,
                                                );
                                            error!(
                                                "Failed to download file '{}' from {}:{} (token: {}) - Error: {}",
                                                filename, peer.host, peer.port, download.token, e
                                            );
                                        }
                                    }
                                }
                                None => error!(
                                    "Cant find filename to save download: {:?}",
                                    download.filename
                                ),
                            }

                            // Signal download slot freed (for concurrency limiter)
                            if let Ok(ctx) = context.read() {
                                if let Some(ref sender) = ctx.sender {
                                    let _ = sender.send(ClientOperation::DownloadSlotFreed);
                                }
                            }
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
                    .as_ref()
                    .map(|r| r.contains(&new_peer.username))
                    .unwrap_or(false);

                if peer_exists {
                    debug!("Already connected to {}", new_peer.username);
                } else {
                    let ctx = self.context.read().unwrap_or_else(|e| e.into_inner());
                    if let Some(ref server_sender) = ctx.server_sender {
                        server_sender
                            .send(ServerMessage::GetPeerAddress(new_peer.username.clone()))
                            .unwrap_or_else(|e| {
                                error!("[worker] Failed to send GetPeerAddress: {}", e)
                            });
                    }
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
                    .as_ref()
                    .map(|r| r.contains(&username))
                    .unwrap_or(false);

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
                    tokio::spawn(async move {
                        Self::connect_to_peer(peer, context, own_username, None);
                    });
                }
            }
            ClientOperation::UpdateDownloadTokens(transfer, username) => {
                let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());

                let download_to_update = context.get_downloads().iter().find_map(|d| {
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
                self.context
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .server_sender = Some(sender);
                debug!("[worker] Server sender initialized");
            }
        }
    }

    fn process_failed_uploads(
        context: Arc<RwLock<ClientContext>>,
        username: &str,
        filename: Option<&str>,
    ) {
        let failed_tokens: Vec<_> = {
            let ctx = context.read().unwrap_or_else(|e| e.into_inner());
            ctx.get_downloads()
                .iter()
                .filter(|d| d.username == username && filename.is_none_or(|f| d.filename == *f))
                .map(|d| {
                    let _ = d.sender.send(DownloadStatus::Failed);
                    d.token
                })
                .collect()
        };

        for token in failed_tokens {
            let mut ctx = context.write().unwrap_or_else(|e| e.into_inner());
            ctx.update_download_with_status(token, DownloadStatus::Failed);
            ctx.remove_download(token);
        }
    }

    fn connect_to_peer(
        peer: Peer,
        context: Arc<RwLock<ClientContext>>,
        own_username: String,
        stream: Option<std::net::TcpStream>,
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
                if let Some(ref registry) = ctx.peer_registry {
                    match registry.register_peer(peer_clone, tokio_stream, None) {
                        Ok(_) => (),
                        Err(e) => {
                            trace!("Failed to spawn peer actor for {:?}: {:?}", username, e);
                        }
                    }
                } else {
                    trace!("PeerRegistry not initialized");
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
                    peer.token.unwrap(),
                    false,
                    own_username,
                );
                tokio::task::spawn_blocking(move || {
                    match download_peer.download_file(context.clone(), None, stream) {
                        Ok((download, filename)) => {
                            trace!("[worker] downloaded {} bytes {:?}", filename, download.size);
                            let _ = download.sender.send(DownloadStatus::Completed);
                            context
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .update_download_with_status(download.token, DownloadStatus::Completed);
                        }
                        Err(e) => {
                            trace!("[worker] failed to download: {}", e);
                        }
                    }
                    // Signal download slot freed
                    if let Ok(ctx) = context.read() {
                        if let Some(ref sender) = ctx.sender {
                            let _ = sender.send(ClientOperation::DownloadSlotFreed);
                        }
                    }
                });
            }
            ConnectionType::D => {
                error!("ConnectionType::D not implemented")
            }
        }
    }

    fn pierce_firewall(peer: Peer, context: Arc<RwLock<ClientContext>>, own_username: String) {
        debug!("Piercing firewall for peer: {:?}", peer);
        {
            let ctx = context.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref server_sender) = ctx.server_sender {
                if let Some(token) = peer.token {
                    if let Err(e) = server_sender.send(ServerMessage::PierceFirewall(token)) {
                        error!("Failed to send PierceFirewall message: {}", e);
                    }
                } else {
                    error!("No token available for PierceFirewall");
                }
            } else {
                error!("No server sender available for PierceFirewall");
            }
        }
        Self::connect_to_peer(peer, context, own_username, None);
    }
}
