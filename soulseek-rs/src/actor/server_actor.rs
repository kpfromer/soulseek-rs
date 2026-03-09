use crate::actor::{Actor, ActorHandle};
use crate::client::{ClientOperation, KeepAliveSettings, ReconnectSettings};
use crate::dispatcher::MessageDispatcher;
use crate::message::server::ConnectToPeerHandler;
use crate::message::server::ExcludedSearchPhrasesHandler;
use crate::message::server::FileSearchHandler;
use crate::message::server::GetPeerAddressHandler;
use crate::message::server::LoginHandler;
use crate::message::server::MessageFactory;
use crate::message::server::MessageUser;
use crate::message::server::ParentMinSpeedHandler;
use crate::message::server::ParentSpeedRatioHandler;
use crate::message::server::PrivilegedUsersHandler;
use crate::message::server::RoomListHandler;
use crate::message::server::WishListIntervalHandler;
use crate::message::{Handlers, MessageType};
use crate::message::{Message, MessageReader};
use crate::peer::ConnectionType;
use crate::peer::Peer;

use std::io;
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::token::{PierceToken, SearchToken};
use crate::{SoulseekRs, debug, error, info, trace, warn};

#[derive(Debug, Clone)]
pub struct PeerAddress {
    host: String,
    port: u16,
}

impl PeerAddress {
    pub fn new(host: String, port: u16) -> Self {
        Self { host, port }
    }

    pub fn get_host(&self) -> &str {
        &self.host
    }

    pub fn get_port(&self) -> u16 {
        self.port
    }
}

impl std::fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct UserMessage {
    id: u32,
    timestamp: u32,
    username: String,
    message: String,
    new_message: bool,
}
impl UserMessage {
    pub fn new(
        id: u32,
        timestamp: u32,
        username: String,
        message: String,
        new_message: bool,
    ) -> Self {
        Self {
            id,
            timestamp,
            username,
            message,
            new_message,
        }
    }
    pub fn print(&self) {
        debug!(
            "Timestamp: {}. User: {}, Id: #{}, New message: {} Message: {}",
            self.timestamp, self.username, self.id, self.new_message, self.message
        );
    }
}

#[derive(Debug)]
pub enum ServerMessage {
    ProcessRead,
    LoginStatus(bool),
    SendMessage(Message),
    Login {
        username: String,
        password: String,
        response: tokio::sync::oneshot::Sender<Result<bool, SoulseekRs>>,
    },
    FileSearch {
        token: SearchToken,
        query: String,
    },
    #[allow(dead_code)]
    ConnectToPeer(Peer),
    PierceFirewall(PierceToken),
    GetPeerAddress(String),
    GetPeerAddressResponse {
        username: String,
        host: String,
        port: u32,
        obfuscation_type: u32,
        obfuscated_port: u16,
    },
}

struct Dispatcher {
    inner: MessageDispatcher<ServerMessage>,
    receiver: UnboundedReceiver<ServerMessage>,
    sender: UnboundedSender<ServerMessage>,
}

enum ServerConnection {
    Disconnected {
        reconnect_attempt: u32,
        last_disconnect: Option<Instant>,
    },
    Connecting {
        stream: TcpStream,
        since: Instant,
        /// Preserved from `Disconnected` so `disconnect_with_error` can increment it.
        reconnect_attempt: u32,
    },
    Connected {
        stream: TcpStream,
        dispatcher: Dispatcher,
    },
}

enum LoginState {
    NotAttempted,
    Pending {
        credentials: (String, String),
        response: tokio::sync::oneshot::Sender<Result<bool, SoulseekRs>>,
        deadline: Instant,
    },
    /// Login succeeded (or login was in progress when TCP dropped); credentials available for reconnect.
    LoggedIn {
        credentials: (String, String),
    },
}

pub struct ServerActor {
    address: PeerAddress,
    listen_port: u32,
    enable_listen: bool,
    connection: ServerConnection,
    login_state: LoginState,
    reader: MessageReader,
    client_channel: UnboundedSender<ClientOperation>,
    self_handle: Option<ActorHandle<ServerMessage>>,
    queued_messages: Vec<ServerMessage>,
    reconnect_settings: ReconnectSettings,
    tcp_keepalive: KeepAliveSettings,
}

impl ServerActor {
    pub fn new(
        address: PeerAddress,
        client_channel: UnboundedSender<ClientOperation>,
        listen_port: u32,
        enable_listen: bool,
        tcp_keepalive: KeepAliveSettings,
        reconnect_settings: ReconnectSettings,
    ) -> Self {
        Self {
            address,
            listen_port,
            enable_listen,
            connection: ServerConnection::Disconnected { reconnect_attempt: 0, last_disconnect: None },
            login_state: LoginState::NotAttempted,
            reader: MessageReader::new(),
            client_channel,
            self_handle: None,
            queued_messages: Vec::new(),
            reconnect_settings,
            tcp_keepalive,
        }
    }

    pub fn get_address(&self) -> &PeerAddress {
        &self.address
    }

    pub fn get_sender(&self) -> Option<&UnboundedSender<ServerMessage>> {
        match &self.connection {
            ServerConnection::Connected { dispatcher, .. } => Some(&dispatcher.sender),
            _ => None,
        }
    }

    fn current_reconnect_attempt(&self) -> u32 {
        match &self.connection {
            ServerConnection::Disconnected { reconnect_attempt, .. } => *reconnect_attempt,
            ServerConnection::Connecting { reconnect_attempt, .. } => *reconnect_attempt,
            ServerConnection::Connected { .. } => 0,
        }
    }

    fn initiate_connection(&mut self) -> bool {
        let host = self.address.host.clone();
        let port = self.address.port;

        let addr_str = format!("{}:{}", host, port);

        let mut socket_addrs = match addr_str.to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(e) => {
                error!("[server] Failed to resolve address: {}", e);

                self.disconnect_with_error(io::Error::new(io::ErrorKind::InvalidInput, e));
                return false;
            }
        };

        let socket_addr = socket_addrs.next();

        match socket_addr {
            Some(addr) => {
                match std::net::TcpStream::connect(addr) {
                    Ok(std_stream) => {
                        // Apply TCP keepalive via socket2
                        let socket = socket2::Socket::from(std_stream);
                        if let KeepAliveSettings::Enabled {
                            idle,
                            interval,
                            count,
                            ..
                        } = &self.tcp_keepalive
                        {
                            let ka = socket2::TcpKeepalive::new()
                                .with_time(*idle)
                                .with_interval(*interval)
                                .with_retries(*count);
                            socket.set_tcp_keepalive(&ka).ok();
                        }
                        let std_stream: std::net::TcpStream = socket.into();

                        std_stream.set_nodelay(true).ok();
                        std_stream.set_nonblocking(true).ok();

                        match TcpStream::from_std(std_stream) {
                            Ok(stream) => {
                                let reconnect_attempt = self.current_reconnect_attempt();
                                self.connection = ServerConnection::Connecting {
                                    stream,
                                    since: Instant::now(),
                                    reconnect_attempt,
                                };
                                true
                            }
                            Err(e) => {
                                error!("[server] Failed to convert to tokio TcpStream: {}", e);
                                self.disconnect_with_error(e);
                                false
                            }
                        }
                    }
                    Err(e) => {
                        self.disconnect_with_error(e);
                        false
                    }
                }
            }
            None => {
                let error_msg = format!("No socket addresses found for {}:{}", host, port);
                error!("[server] {}", error_msg);
                self.disconnect_with_error(io::Error::new(io::ErrorKind::InvalidInput, error_msg));
                false
            }
        }
    }

    pub fn set_self_handle(&mut self, handle: ActorHandle<ServerMessage>) {
        self.self_handle = Some(handle);
    }

    fn process_dispatcher_messages(&mut self) {
        let messages: Vec<ServerMessage> =
            if let ServerConnection::Connected { dispatcher, .. } = &mut self.connection {
                let mut msgs = Vec::new();
                while let Ok(msg) = dispatcher.receiver.try_recv() {
                    msgs.push(msg);
                }
                msgs
            } else {
                Vec::new()
            };

        for msg in messages {
            self.handle_message(msg);
        }
    }

    pub fn file_search(&mut self, token: u32, query: &str) {
        self.queue_message(MessageFactory::build_file_search_message(token, query));
    }

    fn handle_message(&mut self, msg: ServerMessage) {
        if !matches!(self.connection, ServerConnection::Connected { .. }) {
            match &msg {
                ServerMessage::ProcessRead => {
                    // Always process read operations
                }
                _ => {
                    // Queue all other messages when not connected
                    self.queued_messages.push(msg);
                    return;
                }
            }
        }

        match msg {
            ServerMessage::ConnectToPeer(peer) => {
                if let Some(op) = match peer.connection_type {
                    ConnectionType::P | ConnectionType::F => {
                        Some(ClientOperation::ConnectToPeer(peer.clone()))
                    }
                    ConnectionType::D => None,
                } {
                    if let Err(e) = self.client_channel.send(op) {
                        error!("[server] Failed to send ConnectToPeer: {}", e);
                    }
                }
            }
            ServerMessage::LoginStatus(logged_in) => {
                // Resolve the pending connect() caller and transition login state.
                match std::mem::replace(&mut self.login_state, LoginState::NotAttempted) {
                    LoginState::Pending { credentials, response, .. } => {
                        if logged_in {
                            let _ = response.send(Ok(true));
                            self.login_state = LoginState::LoggedIn { credentials };
                        } else {
                            let _ = response.send(Err(SoulseekRs::AuthenticationFailed));
                            // login_state stays NotAttempted — no reconnect
                        }
                    }
                    other => {
                        // Reconnect path: no response to send, credentials already in LoggedIn.
                        self.login_state = other;
                    }
                }

                if logged_in {
                    // reconnect_attempt / last_disconnect are in ServerConnection::Disconnected,
                    // which we've already left — no manual reset needed.

                    // Notify client that login succeeded (used for reconnect path)
                    if let Err(e) = self.client_channel.send(ClientOperation::LoginSucceeded) {
                        error!("[server] Failed to send LoginSucceeded: {}", e);
                    }

                    // Post-login setup: tell the server about ourselves
                    self.queue_message(MessageFactory::build_shared_folders_message(1, 499));
                    self.queue_message(MessageFactory::build_no_parent_message());
                    self.queue_message(MessageFactory::build_set_status_message(2));
                    if self.enable_listen {
                        self.queue_message(MessageFactory::build_set_wait_port_message(
                            self.listen_port,
                        ));
                    }
                }
            }
            ServerMessage::PierceFirewall(token) => {
                self.send_message(MessageFactory::build_pierce_firewall_message(token.0));
            }
            ServerMessage::SendMessage(message) => {
                self.send_message(message);
            }
            ServerMessage::GetPeerAddress(username) => {
                self.send_message(MessageFactory::build_get_peer_address(&username));
            }
            ServerMessage::GetPeerAddressResponse {
                username,
                host,
                port,
                obfuscation_type,
                obfuscated_port,
            } => {
                debug!(
                    "[server] Received GetPeerAddress response for {}: {}:{} (obf_type: {}, obf_port: {})",
                    username, host, port, obfuscation_type, obfuscated_port
                );

                if let Err(e) = self
                    .client_channel
                    .send(ClientOperation::GetPeerAddressResponse {
                        username,
                        host,
                        port,
                        obfuscation_type,
                        obfuscated_port,
                    })
                {
                    error!(
                        "[server] Error forwarding GetPeerAddress response to client: {}",
                        e
                    );
                }
            }
            ServerMessage::ProcessRead => {
                self.process_read();
            }
            ServerMessage::Login {
                username,
                password,
                response,
            } => {
                self.queue_message(MessageFactory::build_login_message(&username, &password));

                // Park the caller's oneshot; resolved when LoginStatus arrives or timed out.
                self.login_state = LoginState::Pending {
                    credentials: (username, password),
                    response,
                    deadline: Instant::now() + Duration::from_secs(5),
                };
            }
            ServerMessage::FileSearch { token, query } => {
                self.file_search(token.0, &query);
            }
        }
    }

    fn process_read(&mut self) {
        if self.reader.buffer_len() > 0 {
            self.extract_and_process_messages();
        }

        {
            let ServerConnection::Connected { ref stream, .. } = self.connection else {
                return;
            };

            // Use try_read for non-blocking read on tokio TcpStream
            let mut temp_buffer = [0u8; 1024];
            match stream.try_read(&mut temp_buffer) {
                Ok(0) => {
                    // Connection closed
                    return;
                }
                Ok(n) => {
                    self.reader.push_bytes(&temp_buffer[..n]);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                    debug!("[server] Read operation timed out",);
                }
                Err(e) => {
                    error!(
                        "[server] Error reading from server: {} (kind: {:?}). Disconnecting.",
                        e,
                        e.kind()
                    );
                    self.disconnect_with_error(e);
                    return;
                }
            }
        }
        self.extract_and_process_messages();
    }

    fn extract_and_process_messages(&mut self) {
        let mut extracted_count = 0;
        loop {
            match self.reader.extract_message() {
                Ok(Some(mut message)) => {
                    extracted_count += 1;
                    trace!(
                        "[server] ← Message #{}: {:?}",
                        extracted_count,
                        message
                            .get_message_name(
                                MessageType::Server,
                                message.get_message_code() as u32
                            )
                            .map_err(|e| e.to_string())
                    );
                    if let ServerConnection::Connected { dispatcher, .. } = &self.connection {
                        dispatcher.inner.dispatch(&mut message);
                    } else {
                        warn!("[server] No dispatcher available!");
                    }
                }
                Err(e) => {
                    warn!("[server] Error extracting message: {}. Disconnecting.", e);
                    self.disconnect_with_error(e);
                    return;
                }
                Ok(None) => {
                    break;
                }
            }
        }

        self.process_dispatcher_messages();
    }

    fn queue_message(&mut self, message: Message) {
        if let ServerConnection::Connected { dispatcher, .. } = &self.connection {
            let sender = &dispatcher.sender;
            match sender.send(ServerMessage::SendMessage(message)) {
                Ok(_) => {}
                Err(e) => error!("Failed to send: {}", e),
            }
        } else {
            self.queued_messages
                .push(ServerMessage::SendMessage(message));
        }
    }

    fn send_message(&mut self, message: Message) {
        let ServerConnection::Connected { ref stream, .. } = self.connection else {
            error!("[server] Cannot send message: not connected");
            return;
        };

        trace!(
            "[server] ➡ {:?}",
            message
                .get_message_name(
                    MessageType::Server,
                    u32::from_le_bytes(message.get_slice(0, 4).try_into().unwrap())
                )
                .map_err(|e| e.to_string())
        );

        let buf = message.get_buffer();
        match stream.try_write(&buf) {
            Ok(n) if n == buf.len() => {}
            Ok(n) => {
                // Partial write - for now treat as error
                error!("[server] Partial write: {} of {} bytes", n, buf.len());
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // TODO: buffer for later write
                warn!("[server] Write would block, message may be lost");
            }
            Err(e) => {
                error!("[server] Error writing message: {}. Disconnecting.", e);
                self.disconnect_with_error(e);
            }
        }
    }

    fn disconnect_with_error(&mut self, _error: io::Error) {
        debug!("[server] disconnect");

        // Only transition if we were previously connected or connecting.
        let new_reconnect_attempt = match &self.connection {
            ServerConnection::Connected { .. } => 1,
            ServerConnection::Connecting { reconnect_attempt, .. } => reconnect_attempt + 1,
            ServerConnection::Disconnected { .. } => return,
        };
        self.connection = ServerConnection::Disconnected {
            reconnect_attempt: new_reconnect_attempt,
            last_disconnect: Some(Instant::now()),
        };

        // Cancel any pending connect() caller, preserving credentials for reconnect.
        if let LoginState::Pending { credentials, response, .. } =
            std::mem::replace(&mut self.login_state, LoginState::NotAttempted)
        {
            let _ = response.send(Err(SoulseekRs::ConnectionClosed));
            self.login_state = LoginState::LoggedIn { credentials };
        }

        if let Err(e) = self
            .client_channel
            .send(ClientOperation::ServerDisconnected)
        {
            error!("[server] Failed to send ServerDisconnected: {}", e);
        }
    }

    fn check_connection_status(&mut self) {
        let ServerConnection::Connecting { since, .. } = self.connection else {
            return;
        };

        if since.elapsed() > Duration::from_secs(20) {
            error!("[server] Connection timeout after 20 seconds");
            self.disconnect_with_error(io::Error::new(
                io::ErrorKind::TimedOut,
                "Connection timeout",
            ));
            return;
        }

        // Since std::net::TcpStream::connect() is blocking and succeeded,
        // the connection is established once we have a stream.
        // Try a zero-byte read to verify the connection is still alive.
        let ServerConnection::Connecting { ref stream, .. } = self.connection else {
            return;
        };
        let mut buf = [0u8; 1];
        match stream.try_read(&mut buf) {
            Ok(_) => {
                // Got data or EOF - either way, connection was established
                if buf[0] != 0 {
                    self.reader.push_bytes(&buf[..1]);
                }
                self.on_connection_established();
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No data yet but socket is ready - connection established
                self.on_connection_established();
            }
            Err(e) => {
                error!("[server] Connection failed: {}", e);
                self.disconnect_with_error(e);
            }
        }
    }

    fn on_connection_established(&mut self) {
        let stream = match std::mem::replace(
            &mut self.connection,
            ServerConnection::Disconnected { reconnect_attempt: 0, last_disconnect: None },
        ) {
            ServerConnection::Connecting { stream, .. } => stream,
            other => {
                self.connection = other;
                error!("[server] on_connection_established called in wrong state");
                return;
            }
        };

        let (dispatcher_sender, dispatcher_receiver) = mpsc::unbounded_channel::<ServerMessage>();

        if let Err(e) = self
            .client_channel
            .send(ClientOperation::SetServerSender(dispatcher_sender.clone()))
        {
            error!("[server] Failed to send SetServerSender: {}", e);
        }

        let mut handlers = Handlers::new();
        handlers.register_handler(LoginHandler);
        handlers.register_handler(RoomListHandler);
        handlers.register_handler(ExcludedSearchPhrasesHandler);
        handlers.register_handler(PrivilegedUsersHandler);
        handlers.register_handler(MessageUser);
        handlers.register_handler(WishListIntervalHandler);
        handlers.register_handler(ParentMinSpeedHandler);
        handlers.register_handler(ParentSpeedRatioHandler);
        handlers.register_handler(FileSearchHandler);
        handlers.register_handler(GetPeerAddressHandler);
        handlers.register_handler(ConnectToPeerHandler);

        let dispatcher = Dispatcher {
            inner: MessageDispatcher::new("server".into(), dispatcher_sender.clone(), handlers),
            receiver: dispatcher_receiver,
            sender: dispatcher_sender,
        };

        self.connection = ServerConnection::Connected { stream, dispatcher };

        // On reconnect (previously logged in), auto-queue login with stored credentials.
        if let LoginState::LoggedIn { credentials: (ref u, ref p) } = self.login_state {
            let (username, password) = (u.clone(), p.clone());
            info!("[server] Reconnect: auto-queuing login for {}", username);
            self.queue_message(MessageFactory::build_login_message(&username, &password));
        }

        let queued = std::mem::take(&mut self.queued_messages);
        for msg in queued {
            self.handle_message(msg);
        }

        if let Some(ref handle) = self.self_handle {
            handle.send(ServerMessage::ProcessRead).ok();
        }

        self.process_read();
    }

    fn maybe_reconnect(&mut self) {
        // Only reconnect after a successful login (credentials stored in LoggedIn state).
        if !matches!(self.login_state, LoginState::LoggedIn { .. }) {
            return;
        }

        let (reconnect_attempt, last_disconnect) = match &self.connection {
            ServerConnection::Disconnected { reconnect_attempt, last_disconnect } => {
                (*reconnect_attempt, *last_disconnect)
            }
            _ => return,
        };

        let Some(last_disconnect) = last_disconnect else {
            return;
        };

        let backoff = match &self.reconnect_settings {
            ReconnectSettings::Disabled => return,
            ReconnectSettings::EnabledExponentialBackoff {
                min_delay,
                max_delay,
                max_attempts,
            } => {
                if let Some(max) = max_attempts {
                    if reconnect_attempt > *max {
                        warn!(
                            "[server] Max reconnect attempts ({}) reached, giving up",
                            max
                        );
                        return;
                    }
                }
                // Exponential backoff: min_delay * 2^(attempt-1), capped at max_delay
                let exp = reconnect_attempt.saturating_sub(1);
                let factor = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
                let delay_secs = (min_delay.as_secs()).saturating_mul(factor);
                Duration::from_secs(delay_secs.min(max_delay.as_secs()))
            }
        };

        if last_disconnect.elapsed() < backoff {
            return;
        }

        info!(
            "[server] Attempting reconnect (attempt {}, backoff {}s)...",
            reconnect_attempt,
            backoff.as_secs()
        );
        self.initiate_connection();
    }
}

impl Actor for ServerActor {
    type Message = ServerMessage;

    fn handle(&mut self, msg: Self::Message) {
        self.handle_message(msg);
    }

    fn on_start(&mut self) {
        if matches!(self.connection, ServerConnection::Disconnected { .. }) {
            self.initiate_connection();
        } else {
            self.on_connection_established();
        }
    }

    fn on_stop(&mut self) {
        trace!("[server] actor stopping");
        self.connection = ServerConnection::Disconnected { reconnect_attempt: 0, last_disconnect: None };
    }

    fn tick(&mut self) {
        // Time out any pending login wait.
        if let LoginState::Pending { ref deadline, .. } = self.login_state {
            if Instant::now() >= *deadline {
                if let LoginState::Pending { response, .. } =
                    std::mem::replace(&mut self.login_state, LoginState::NotAttempted)
                {
                    let _ = response.send(Err(SoulseekRs::Timeout));
                    // login_state stays NotAttempted — login protocol timed out, don't reconnect
                }
            }
        }

        match &self.connection {
            ServerConnection::Connecting { .. } => {
                self.check_connection_status();
            }
            ServerConnection::Connected { .. } => {
                self.process_read();
            }
            ServerConnection::Disconnected { .. } => {
                self.maybe_reconnect();
            }
        }
    }
}
