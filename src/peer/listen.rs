use std::io;
use std::sync::{Arc, RwLock};

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{ClientContext, ClientOperation};

use crate::message::{Message, MessageReader};
use crate::peer::{ConnectionType, DownloadPeer, Peer};
use crate::types::Download;
use crate::{DownloadStatus, debug, error, info, trace};

const PEER_INIT_MESSAGE_CODE: u8 = 1;
const PIERCE_FIREWALL_MESSAGE_CODE: u8 = 0;

#[derive(Clone)]
struct ConnectionContext {
    #[allow(dead_code)]
    client_sender: UnboundedSender<ClientOperation>,
    client_context: Arc<RwLock<ClientContext>>,
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

fn parse_pierce_firewall_token(message: &mut Message) -> Option<u32> {
    message.set_pointer(4);
    let message_code = message.read_int8();

    if message_code != PIERCE_FIREWALL_MESSAGE_CODE {
        return None;
    }

    Some(message.read_int32())
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

fn parse_token_from_buffer(buffer: &[u8], username: &str) -> Option<u32> {
    let token_bytes = buffer.get(0..4)?;
    let token = u32::from_le_bytes(
        token_bytes
            .try_into()
            .unwrap_or_else(|_| panic!("[listener:{}] slice with incorrect length, can't extract transfer_token", username)),
    );
    Some(token)
}

fn extract_download_from_buffer(
    reader: &mut MessageReader,
    client_context: &Arc<RwLock<ClientContext>>,
    username: &str,
    peer_ip: &str,
    peer_port: u16,
) -> Option<Download> {
    if reader.buffer_len() == 0 {
        return None;
    }
    let buffer = reader.get_buffer();
    let token = parse_token_from_buffer(&buffer, username)?;
    trace!(
        "[listener:{}] got transfer_token: {} from data chunk",
        username, token
    );

    let context = client_context.read().unwrap();
    let download = context.get_download_by_token(token).cloned();

    if download.is_none() {
        let download_tokens = context.get_download_tokens();
        trace!(
            "[listener:{peer_ip}:{peer_port}] download token not found: {:?}, download tokens: {:?}",
            token, download_tokens
        );
    }

    download
}

fn handle_peer_connection(
    peer: Peer,
    stream: TcpStream,
    reader: MessageReader,
    context: &ConnectionContext,
    _peer_ip: &str,
    _peer_port: u16,
) {
    let client_context = context.client_context.read().unwrap();
    if let Some(ref registry) = client_context.peer_registry {
        match registry.register_peer(peer.clone(), Some(stream), Some(reader)) {
            Ok(_) => (),
            Err(e) => {
                error!(
                    "Failed to spawn peer actor for {:?}: {:?}",
                    peer.username, e
                );
            }
        }
    } else {
        error!("PeerRegistry not initialized");
    }
}

fn handle_file_connection(
    peer: Peer,
    stream: std::net::TcpStream,
    mut reader: MessageReader,
    token: u32,
    context: &ConnectionContext,
    peer_ip: &str,
    peer_port: u16,
) {
    trace!(
        "[client] DownloadFromPeer token: {} peer: {:?}",
        token, peer
    );

    let download = extract_download_from_buffer(
        &mut reader,
        &context.client_context,
        &peer.username,
        peer_ip,
        peer_port,
    );

    let download_peer = DownloadPeer::new(
        format!("{}:direct", peer.username),
        peer.host.clone(),
        peer.port,
        token,
        true,
        context.own_username.clone(),
    );

    match download_peer.download_file(
        context.client_context.clone(),
        download,
        Some(stream),
    ) {
        Ok((download, filename)) => {
            let _ = download.sender.send(DownloadStatus::Completed);
            context
                .client_context
                .write()
                .unwrap()
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
            error!(
                "Failed to download file from {}:{} (token: {}) - Error: {}",
                peer.host, peer.port, token, e
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

    let Ok(mut message) = read_peer_init_message(&mut stream, &mut reader).await
    else {
        error!(
            "[listener:{peer_ip}:{peer_port}] Failed to read peer init message"
        );
        return;
    };

    // Check for PierceFireWall message (code 0)
    if let Some(token) = parse_pierce_firewall_token(&mut message) {
        debug!(
            "[listener:{peer_ip}:{peer_port}] PierceFireWall token: {}",
            token
        );

        let download = {
            let client_context = context.client_context.read().unwrap();
            client_context.get_download_by_token(token).cloned()
        };

        if download.is_none() {
            debug!(
                "[listener:{peer_ip}:{peer_port}] No download found for PierceFireWall token: {}",
                token
            );
            return;
        }

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

        tokio::task::spawn_blocking(move || {
            let download_peer = DownloadPeer::new(
                peer.username.clone(),
                peer.host.clone(),
                peer.port,
                token,
                true,
                context.own_username.clone(),
            );

            match download_peer.download_file(
                context.client_context.clone(),
                download,
                Some(std_stream),
            ) {
                Ok((download, filename)) => {
                    let _ = download.sender.send(DownloadStatus::Completed);
                    context
                        .client_context
                        .write()
                        .unwrap()
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
                    error!(
                        "Failed to download file via PierceFireWall (token: {}) - Error: {}",
                        token, e
                    );
                }
            }
        });
        return;
    }

    let Some(init_data) = parse_peer_init_message(message) else {
        error!(
            "[listener:{peer_ip}:{peer_port}] Invalid or unknown peer init message"
        );
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
        ConnectionType::P => handle_peer_connection(
            peer, stream, reader, &context, &peer_ip, peer_port,
        ),

        ConnectionType::F => {
            // Convert tokio TcpStream to std for blocking download
            let std_stream = stream.into_std().unwrap();
            tokio::task::spawn_blocking(move || {
                trace!(
                    "[listener:{peer_ip}:{peer_port}] handling file connection in blocking task"
                );
                handle_file_connection(
                    peer,
                    std_stream,
                    reader,
                    init_data.token,
                    &context,
                    &peer_ip,
                    peer_port,
                )
            });
        }
        ConnectionType::D => {
            debug!(
                "[listener:{peer_ip}:{peer_port}] connection type is D, not supported yet, closing connection. "
            );
        }
    }
}

pub struct Listen {}

impl Listen {
    pub async fn start(
        port: u32,
        client_sender: UnboundedSender<ClientOperation>,
        client_context: Arc<RwLock<ClientContext>>,
        own_username: String,
    ) {
        info!("[listener] starting listener on port {port}");

        let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
            .await
            .expect("Failed to bind listener to port");

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
                    error!(
                        "[listener] Failed to accept connection: {}",
                        e
                    );
                }
            }
        }
    }
}
