use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::client::{ClientContext, ClientOperation};
use crate::message::{Message, MessageReader};
use crate::peer::{ConnectionType, DownloadPeer, Peer};
use crate::token::DownloadToken;
use crate::{DownloadStatus, debug, error, info, trace};

const PEER_INIT_MESSAGE_CODE: u8 = 1;
const PIERCE_FIREWALL_MESSAGE_CODE: u8 = 0;
const PEER_INIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ConnectionContext {
    client_sender: UnboundedSender<ClientOperation>,
    client_context: Arc<ClientContext>,
    own_username: String,
}

struct PeerInitData {
    username: String,
    connection_type: ConnectionType,
    token: u32,
}

async fn read_peer_init_message(
    stream: &mut TcpStream,
    reader: &mut MessageReader,
) -> io::Result<Message> {
    let mut temp_buffer = [0u8; 1024];
    loop {
        let n = stream.read(&mut temp_buffer).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Connection closed while reading peer init",
            ));
        }
        reader.push_bytes(&temp_buffer[..n]);

        if let Ok(Some(msg)) = reader.extract_message() {
            return Ok(msg);
        }
    }
}

fn parse_pierce_firewall_token(message: &mut Message) -> Option<DownloadToken> {
    message.set_pointer(4);
    let message_code = message.read_int8();

    if message_code != PIERCE_FIREWALL_MESSAGE_CODE {
        return None;
    }

    Some(DownloadToken(message.read_int32()))
}

fn parse_peer_init_message(mut message: Message) -> Option<PeerInitData> {
    message.set_pointer(4);
    let message_code = message.read_int8();

    if message_code != PEER_INIT_MESSAGE_CODE {
        return None;
    }

    Some(PeerInitData {
        username: message.read_string(),
        connection_type: message.read_string().parse().unwrap(),
        token: message.read_int32(),
    })
}

fn parse_token_from_buffer(buffer: &[u8], username: &str) -> Option<DownloadToken> {
    let token_bytes = buffer.get(0..4)?;
    let token = u32::from_le_bytes(token_bytes.try_into().unwrap_or_else(|_| {
        panic!(
            "[listener:{}] slice with incorrect length, can't extract transfer_token",
            username
        )
    }));
    Some(DownloadToken(token))
}

fn handle_peer_connection(
    peer: Peer,
    stream: TcpStream,
    reader: MessageReader,
    context: &ConnectionContext,
    _peer_ip: &str,
    _peer_port: u16,
) {
    match context.client_context.peer_registry.register_peer(peer.clone(), Some(stream), Some(reader)) {
        Ok(_) => (),
        Err(e) => {
            error!(
                "Failed to spawn peer actor for {:?}: {:?}",
                peer.username, e
            );
        }
    }
}

async fn handle_incoming_connection(stream: TcpStream, context: ConnectionContext) {
    let Ok(peer_addr) = stream.peer_addr() else {
        error!("[listener] failed to get peer address");
        return;
    };

    let peer_ip = peer_addr.ip().to_string();
    let peer_port = peer_addr.port();
    let mut stream = stream;
    let mut reader = MessageReader::new();

    let mut message = match tokio::time::timeout(
        PEER_INIT_TIMEOUT,
        read_peer_init_message(&mut stream, &mut reader),
    )
    .await
    {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            error!("[listener:{peer_ip}:{peer_port}] Failed to read peer init message: {e}");
            return;
        }
        Err(_) => {
            debug!("[listener:{peer_ip}:{peer_port}] Peer init timed out, dropping connection");
            return;
        }
    };

    // Check for PierceFireWall message (code 0)
    if let Some(token) = parse_pierce_firewall_token(&mut message) {
        debug!(
            "[listener:{peer_ip}:{peer_port}] PierceFireWall token: {}",
            token
        );

        // Query the worker for the download by token
        let (tx, rx) = oneshot::channel();
        let _ = context.client_sender.send(ClientOperation::QueryDownloadByToken(token, tx));
        let Some(download) = rx.await.ok().flatten() else {
            debug!(
                "[listener:{peer_ip}:{peer_port}] No download found for PierceFireWall token: {}",
                token
            );
            return;
        };

        let peer = Peer::new(
            format!("{}:pierce", peer_ip),
            ConnectionType::F,
            peer_ip.clone(),
            peer_port.into(),
            None,
            0,
            0,
            0,
        );

        // Convert tokio TcpStream to std for DownloadPeer (which still uses blocking I/O)
        let std_stream = stream.into_std().unwrap();
        let client_sender = context.client_sender.clone();
        let own_username = context.own_username.clone();

        tokio::task::spawn_blocking(move || {
            let download_peer = DownloadPeer::new(
                peer.username.clone(),
                peer.host.clone(),
                peer.port,
                token.0,
                own_username,
            );

            match download_peer.download_direct(download, Some(std_stream)) {
                Ok((dl, filename)) => {
                    let _ = dl.sender.send(DownloadStatus::Completed);
                    let _ = client_sender.send(ClientOperation::DownloadCompleted(dl.token, Ok(filename)));
                }
                Err(e) => {
                    error!(
                        "Failed to download file via PierceFireWall (token: {}) - Error: {}",
                        token, e
                    );
                    let _ = client_sender.send(ClientOperation::DownloadCompleted(
                        token,
                        Err(crate::error::SoulseekRs::InvalidMessage(e.to_string())),
                    ));
                }
            }
        });
        return;
    }

    let Some(init_data) = parse_peer_init_message(message) else {
        error!("[listener:{peer_ip}:{peer_port}] Invalid or unknown peer init message");
        return;
    };

    debug!(
        "[listener:{peer_ip}:{peer_port}] peerInit username: {} connection_type: {} token: {}",
        init_data.username, init_data.connection_type, init_data.token
    );

    let peer = Peer::new(
        format!("{}:direct", init_data.username),
        init_data.connection_type.clone(),
        peer_ip.clone(),
        peer_port.into(),
        None,
        0,
        0,
        0,
    );

    match init_data.connection_type {
        ConnectionType::P => {
            handle_peer_connection(peer, stream, reader, &context, &peer_ip, peer_port)
        }

        ConnectionType::F => {
            // Pre-fetch the download token from the buffered data before spawn_blocking
            let buffer = reader.get_buffer();
            let Some(download_token) = parse_token_from_buffer(&buffer, &init_data.username) else {
                error!(
                    "[listener:{}:{}] No download token in buffer for F connection",
                    peer_ip, peer_port
                );
                return;
            };
            trace!(
                "[listener:{}] got transfer_token: {} from data chunk",
                init_data.username, download_token
            );

            // Query the worker for the download
            let (tx, rx) = oneshot::channel();
            let _ = context.client_sender.send(ClientOperation::QueryDownloadByToken(download_token, tx));
            let Some(download) = rx.await.ok().flatten() else {
                error!(
                    "[listener:{}:{}] No download found for file connection token: {}",
                    peer_ip, peer_port, download_token
                );
                return;
            };

            let std_stream = stream.into_std().unwrap();
            let client_sender = context.client_sender.clone();
            let own_username = context.own_username.clone();
            let peer_host = peer.host.clone();
            let peer_port_val = peer.port;
            let connection_token = init_data.token;
            let peer_username = init_data.username.clone();

            tokio::task::spawn_blocking(move || {
                trace!(
                    "[listener:{}:{}] handling file connection in blocking task",
                    peer_ip, peer_port
                );
                let download_peer = DownloadPeer::new(
                    format!("{}:direct", peer_username),
                    peer_host.clone(),
                    peer_port_val,
                    connection_token,
                    own_username,
                );
                match download_peer.download_direct(download, Some(std_stream)) {
                    Ok((dl, filename)) => {
                        let _ = dl.sender.send(DownloadStatus::Completed);
                        let _ = client_sender.send(ClientOperation::DownloadCompleted(dl.token, Ok(filename)));
                    }
                    Err(e) => {
                        error!(
                            "Failed to download file from {}:{} (token: {}) - Error: {}",
                            peer_host, peer_port_val, download_token, e
                        );
                        let _ = client_sender.send(ClientOperation::DownloadCompleted(
                            download_token,
                            Err(crate::error::SoulseekRs::InvalidMessage(e.to_string())),
                        ));
                    }
                }
            });
        }
        ConnectionType::D => {
            debug!(
                "[listener:{peer_ip}:{peer_port}] connection type is D, not supported yet, closing connection. "
            );
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ListenError {
    #[error("failed to bind listener to port {0}")]
    FailedToBindListener(#[from] io::Error),
}

pub struct Listen;

impl Listen {
    pub async fn start(
        port: u32,
        client_sender: UnboundedSender<ClientOperation>,
        client_context: Arc<ClientContext>,
        own_username: String,
    ) -> Result<(), ListenError> {
        info!("[listener] starting listener on port {port}");

        let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
            .await
            .map_err(ListenError::FailedToBindListener)?;

        let context = ConnectionContext {
            client_sender,
            client_context,
            own_username,
        };

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let context = context.clone();
                    tokio::spawn(async move {
                        handle_incoming_connection(stream, context).await;
                    });
                }
                Err(e) => {
                    error!("[listener] Failed to accept connection: {}", e);
                }
            }
        }
    }
}
