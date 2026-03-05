use crate::actor::ActorHandle;
use crate::actor::server_actor::{PeerAddress, ServerActor, ServerMessage};
use crate::types::DownloadStatus;
use crate::utils::logger;
use crate::{
    Transfer,
    actor::{ActorSystem, peer_registry::PeerRegistry},
    error::{Result, SoulseekRs},
    peer::{ConnectionType, DownloadPeer, NewPeer, Peer, listen::Listen},
    types::{Download, Search, SearchResult},
    utils::md5,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    sync::{
        RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::{debug, error, info, trace, warn};
const DEFALT_LISTEN_PORT: u32 = 2234;

/// TCP keepalive settings.
#[derive(Debug, Clone)]
pub enum KeepAliveSettings {
    Enabled {
        /// The idle time before sending a keepalive packet.
        idle: Duration,
        /// The interval between keepalive packets.
        interval: Duration,
        /// The number of keepalive packets to send before considering the connection dead.
        count: u32,
        /// The maximum idle time before considering the connection dead.
        max_idle: Duration,
    },
    Disabled,
}

#[derive(Debug, Clone)]
pub enum ReconnectSettings {
    Disabled,
    EnabledExponentialBackoff {
        min_delay: Duration,
        max_delay: Duration,
        /// The maximum number of attempts to reconnect.
        /// If None, the reconnect will continue indefinitely.
        max_attempts: Option<u32>,
    },
}

#[derive(Debug, Clone)]
pub struct SearchRateLimitSettings {
    pub searches: u32,
    pub per_period: Duration,
}

#[derive(Debug, Clone)]
pub struct DownloadRateLimitSettings {
    pub concurrent_downloads: u32,
}

/// A wrapper for a string that is plain text and is sent over the network via an unencrypted channel.
/// Soulseek uses an unencrypted channel for the username and password.
#[derive(Debug, Clone)]
pub struct PlainTextUnencrypted(pub String);

impl PlainTextUnencrypted {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PlainTextUnencrypted {
    fn from(s: &str) -> Self {
        PlainTextUnencrypted(s.to_string())
    }
}

impl From<String> for PlainTextUnencrypted {
    fn from(s: String) -> Self {
        PlainTextUnencrypted(s)
    }
}

#[derive(Debug, Clone)]
pub struct ClientSettings {
    pub username: PlainTextUnencrypted,
    pub password: PlainTextUnencrypted,
    pub server_address: PeerAddress,
    pub enable_listen: bool,
    pub listen_port: u32,
    /// These are the settings for the TCP keepalive.
    /// This only affects the connections to the Soulseek server.
    pub tcp_keepalive_settings: KeepAliveSettings,
    /// These settings affect how to reconnect to the Soulseek server in case of a connection failure.
    pub reconnect_settings: ReconnectSettings,
    /// Controls the rate limiting of searches to prevent abuse, and being banned.
    pub search_rate_limit_settings: Option<SearchRateLimitSettings>,
    /// Controls the rate limiting of downloads to prevent abuse, and being banned.
    pub download_rate_limit_settings: Option<DownloadRateLimitSettings>,
}

impl ClientSettings {
    pub fn new(
        username: impl Into<PlainTextUnencrypted>,
        password: impl Into<PlainTextUnencrypted>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            ..Default::default()
        }
    }
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            username: PlainTextUnencrypted(String::new()),
            password: PlainTextUnencrypted(String::new()),
            server_address: PeerAddress::new("server.slsknet.org".to_string(), 2416),
            enable_listen: true,
            listen_port: DEFALT_LISTEN_PORT,
            tcp_keepalive_settings: KeepAliveSettings::Enabled {
                idle: Duration::from_secs(10),
                interval: Duration::from_secs(2),
                count: 10,
                max_idle: Duration::from_secs(30),
            },
            reconnect_settings: ReconnectSettings::EnabledExponentialBackoff {
                min_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(60),
                max_attempts: None,
            },
            search_rate_limit_settings: Some(SearchRateLimitSettings {
                searches: 34,
                per_period: Duration::from_secs(220),
            }),
            download_rate_limit_settings: Some(DownloadRateLimitSettings {
                concurrent_downloads: 2,
            }),
        }
    }
}

enum ClientState {
    Disconnected,
    Connecting,
    Connected,
}

#[allow(dead_code)]
struct PendingDownload {
    filename: String,
    username: String,
    size: u64,
    download_directory: String,
    token: u32,
    status_sender: UnboundedSender<DownloadStatus>,
}

struct SearchRateLimiter {
    limit: u32,
    period: Duration,
    issued_at: VecDeque<Instant>,
}

impl SearchRateLimiter {
    fn new(limit: u32, period: Duration) -> Self {
        Self {
            limit,
            period,
            issued_at: VecDeque::new(),
        }
    }

    /// Returns Some(wait_duration) if rate-limited, None if a search can proceed immediately.
    fn wait_duration(&mut self) -> Option<Duration> {
        let now = Instant::now();
        // Evict entries older than the period
        while let Some(&front) = self.issued_at.front() {
            if now.duration_since(front) >= self.period {
                self.issued_at.pop_front();
            } else {
                break;
            }
        }
        if self.issued_at.len() >= self.limit as usize {
            let oldest = *self.issued_at.front().unwrap();
            let elapsed = now.duration_since(oldest);
            if elapsed < self.period {
                Some(self.period - elapsed)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn record(&mut self) {
        self.issued_at.push_back(Instant::now());
    }
}

struct DownloadConcurrencyLimiter {
    max_concurrent: u32,
    active: u32,
    queue: VecDeque<PendingDownload>,
}

impl DownloadConcurrencyLimiter {
    fn new(max_concurrent: u32) -> Self {
        Self {
            max_concurrent,
            active: 0,
            queue: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub enum ClientOperation {
    NewPeer(NewPeer),
    ConnectToPeer(Peer),
    SearchResult(SearchResult),
    PeerDisconnected(String, Option<SoulseekRs>),
    PierceFireWall(Peer),
    DownloadFromPeer(u32, Peer, bool),
    UpdateDownloadTokens(Transfer, String),
    GetPeerAddressResponse {
        username: String,
        host: String,
        port: u32,
        obfuscation_type: u32,
        obfuscated_port: u16,
    },
    UploadFailed(String, String),
    SetServerSender(UnboundedSender<ServerMessage>),
    /// Server TCP connection was lost; reconnect will be handled by ServerActor.
    ServerDisconnected,
    /// (Re)login confirmed; replay pending downloads.
    LoginSucceeded,
    /// A download finished; dequeue next from concurrency limiter.
    DownloadSlotFreed,
}

pub struct ClientContext {
    pub peer_registry: Option<PeerRegistry>,
    pub(crate) sender: Option<UnboundedSender<ClientOperation>>,
    server_sender: Option<UnboundedSender<ServerMessage>>,
    searches: HashMap<String, Search>,
    downloads: Vec<Download>,
    actor_system: Arc<ActorSystem>,
}

impl Default for ClientContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientContext {
    pub fn add_download(&mut self, download: Download) {
        self.downloads.push(download);
    }

    pub fn remove_download(&mut self, token: u32) {
        self.downloads.retain(|d| d.token != token);
    }

    pub fn get_download_by_token(&self, token: u32) -> Option<&Download> {
        self.downloads.iter().find(|d| d.token == token)
    }

    pub fn get_download_by_token_mut(&mut self, token: u32) -> Option<&mut Download> {
        self.downloads.iter_mut().find(|d| d.token == token)
    }

    pub fn get_download_tokens(&self) -> Vec<u32> {
        self.downloads.iter().map(|d| d.token).collect()
    }

    pub fn get_downloads(&self) -> &Vec<Download> {
        &self.downloads
    }

    pub fn update_download_with_status(&mut self, token: u32, status: DownloadStatus) {
        if let Some(download) = self.get_download_by_token_mut(token) {
            download.status = status;
        }
    }
}

impl ClientContext {
    #[must_use]
    pub fn new() -> Self {
        let actor_system = Arc::new(ActorSystem::new());

        Self {
            peer_registry: None,
            sender: None,
            server_sender: None,
            searches: HashMap::new(),
            downloads: Vec::new(),
            actor_system,
        }
    }
}

pub struct Client {
    settings: ClientSettings,
    state: Arc<Mutex<ClientState>>,
    context: Arc<RwLock<ClientContext>>,
    server_handle: Option<ActorHandle<ServerMessage>>,
    pending_downloads: Arc<Mutex<VecDeque<PendingDownload>>>,
    search_limiter: Option<Arc<Mutex<SearchRateLimiter>>>,
    download_limiter: Option<Arc<Mutex<DownloadConcurrencyLimiter>>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Client {
    pub fn new(
        username: impl Into<PlainTextUnencrypted>,
        password: impl Into<PlainTextUnencrypted>,
    ) -> Self {
        Self::with_settings(ClientSettings::new(username, password))
    }

    pub fn with_settings(settings: ClientSettings) -> Self {
        logger::init();

        let search_limiter = settings.search_rate_limit_settings.as_ref().map(|s| {
            Arc::new(Mutex::new(SearchRateLimiter::new(s.searches, s.per_period)))
        });

        let download_limiter = settings.download_rate_limit_settings.as_ref().map(|s| {
            Arc::new(Mutex::new(DownloadConcurrencyLimiter::new(
                s.concurrent_downloads,
            )))
        });

        Self {
            settings,
            state: Arc::new(Mutex::new(ClientState::Disconnected)),
            context: Arc::new(RwLock::new(ClientContext::new())),
            server_handle: None,
            pending_downloads: Arc::new(Mutex::new(VecDeque::new())),
            search_limiter,
            download_limiter,
        }
    }

    /// Shutdown all spawned tasks (actors, listener, client operations loop).
    /// Called automatically on drop.
    pub fn shutdown(&self) {
        if let Ok(ctx) = self.context.read() {
            ctx.actor_system.shutdown();
        }
    }

    /// Connect to the Soulseek server and login. Blocks until login succeeds or fails.
    pub async fn connect(&mut self) -> Result<()> {
        // Guard: if already Connected, return Ok
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*state, ClientState::Connected) {
                return Ok(());
            }
        }

        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = ClientState::Connecting;

        let username = self.settings.username.0.clone();
        let password = self.settings.password.0.clone();

        let (sender, message_reader) = mpsc::unbounded_channel::<ClientOperation>();

        {
            let mut ctx = self.context.write().unwrap_or_else(|e| e.into_inner());
            ctx.sender = Some(sender.clone());
            let peer_registry = PeerRegistry::new(
                ctx.actor_system.clone(),
                sender.clone(),
                username.clone(),
            );
            ctx.peer_registry = Some(peer_registry);
        }

        let server_actor = ServerActor::new(
            self.settings.server_address.clone(),
            sender.clone(),
            self.settings.listen_port,
            self.settings.enable_listen,
            self.settings.tcp_keepalive_settings.clone(),
            self.settings.reconnect_settings.clone(),
        );

        let actor_system = self
            .context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .actor_system
            .clone();

        self.server_handle = Some(actor_system.spawn_with_handle(server_actor, |actor, handle| {
            actor.set_self_handle(handle);
        }));

        // Send Login — will be queued by ServerActor until TCP is connected
        let (login_tx, login_rx) = tokio::sync::oneshot::channel();
        if let Some(ref handle) = self.server_handle {
            let _ = handle.send(ServerMessage::Login {
                username: username.clone(),
                password: password.clone(),
                response: login_tx,
            });
        }

        let cancellation_token = actor_system.cancellation_token().clone();

        if self.settings.enable_listen {
            let listen_port = self.settings.listen_port;
            let client_sender = sender.clone();
            let context = self.context.clone();
            let own_username = username.clone();
            let token = cancellation_token.clone();

            tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => {
                        trace!("[listener] Shutdown signal received");
                    }
                    result = Listen::start(
                        listen_port,
                        client_sender,
                        context,
                        own_username,
                    ) => {
                        match result {
                            Ok(_) => {
                                info!("[listener] Listener started successfully");
                            }
                            Err(e) => {
                                error!("[listener] Failed to start listener: {}", e);
                            }
                        }
                    }
                }
            });
        }

        Self::listen_to_client_operations(
            message_reader,
            self.context.clone(),
            username.clone(),
            cancellation_token,
            self.state.clone(),
            self.pending_downloads.clone(),
            self.download_limiter.clone(),
        );

        // Wait for login result
        match login_rx.await {
            Ok(Ok(true)) => {
                *self.state.lock().unwrap_or_else(|e| e.into_inner()) = ClientState::Connected;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Ok(Ok(false)) => Err(SoulseekRs::AuthenticationFailed),
            Err(_) => Err(SoulseekRs::Timeout),
        }
    }

    #[allow(dead_code)]
    pub fn remove_peer(&self, username: &str) {
        let context = self.context.read().unwrap_or_else(|e| e.into_inner());
        if let Some(ref registry) = context.peer_registry
            && let Some(handle) = registry.remove_peer(username)
        {
            let _ = handle.stop();
        }
    }

    pub async fn search(&self, query: &str, timeout: Duration) -> Result<Vec<SearchResult>> {
        self.search_with_cancel(query, timeout, None).await
    }

    pub async fn search_with_cancel(
        &self,
        query: &str,
        timeout: Duration,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<SearchResult>> {
        info!("Searching for {}", query);

        // Rate limiting: wait until a slot is available, then record the search
        if let Some(ref lim) = self.search_limiter {
            loop {
                let wait = lim.lock().unwrap_or_else(|e| e.into_inner()).wait_duration();
                match wait {
                    None => break,
                    Some(d) => tokio::time::sleep(d).await,
                }
            }
            lim.lock().unwrap_or_else(|e| e.into_inner()).record();
        }

        if let Some(handle) = &self.server_handle {
            let hash = md5::md5(query);
            let token = u32::from_str_radix(&hash[0..5], 16)?;

            // Initialize new search with query string as key
            self.context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .searches
                .insert(
                    query.to_string(),
                    Search {
                        token,
                        results: Vec::new(),
                    },
                );

            let _ = handle.send(ServerMessage::FileSearch {
                token,
                query: query.to_string(),
            });
        } else {
            return Err(SoulseekRs::NotConnected);
        }

        let start = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Check if cancelled
            if let Some(ref flag) = cancel_flag
                && flag.load(Ordering::Relaxed)
            {
                info!("Search cancelled by user");
                break;
            }

            // Check if timeout reached
            if start.elapsed() >= timeout {
                break;
            }
        }

        Ok(self.get_search_results(query))
    }

    pub fn get_search_results_count(&self, search_key: &str) -> usize {
        self.context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .searches
            .get(search_key)
            .map(|s| s.results.len())
            .unwrap_or(0)
    }

    pub fn get_search_results(&self, search_key: &str) -> Vec<SearchResult> {
        self.context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .searches
            .get(search_key)
            .map(|s| s.results.clone())
            .unwrap_or_default()
    }

    /// Non-blocking variant that returns None if the lock is unavailable
    pub fn try_get_search_results(&self, search_key: &str) -> Option<Vec<SearchResult>> {
        self.context
            .try_read()
            .ok()
            .and_then(|ctx| ctx.searches.get(search_key).map(|s| s.results.clone()))
    }

    pub fn get_all_searches(&self) -> HashMap<String, Search> {
        self.context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .searches
            .clone()
    }

    pub fn get_all_downloads(&self) -> Vec<Download> {
        self.context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get_downloads()
            .clone()
    }

    pub fn download(
        &self,
        filename: String,
        username: String,
        size: u64,
        download_directory: String,
    ) -> Result<(Download, UnboundedReceiver<DownloadStatus>)> {
        info!("[client] Downloading {} from {}", filename, username);

        let hash = md5::md5(&filename);
        let token = u32::from_str_radix(&hash[0..5], 16)?;

        let (download_sender, download_receiver) = mpsc::unbounded_channel::<DownloadStatus>();

        let download = Download {
            username: username.clone(),
            filename: filename.clone(),
            token,
            size,
            download_directory: download_directory.clone(),
            status: DownloadStatus::Queued,
            sender: download_sender.clone(),
        };

        // Check state before write-locking context (consistent lock order: state → context)
        let is_connected = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            matches!(*state, ClientState::Connected)
        };

        let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());
        context.add_download(download.clone());

        let download_initiated = if is_connected {
            if let Some(ref registry) = context.peer_registry {
                if let Some(ref lim_arc) = self.download_limiter {
                    let mut lim = lim_arc.lock().unwrap_or_else(|e| e.into_inner());
                    if lim.active < lim.max_concurrent {
                        lim.active += 1;
                        drop(lim);
                        registry
                            .queue_upload(&username, download.filename.clone())
                            .is_ok()
                    } else {
                        // At concurrency limit — queue for later dispatch
                        lim.queue.push_back(PendingDownload {
                            filename: filename.clone(),
                            username: username.clone(),
                            size,
                            download_directory,
                            token,
                            status_sender: download_sender,
                        });
                        true
                    }
                } else {
                    registry
                        .queue_upload(&username, download.filename.clone())
                        .is_ok()
                }
            } else {
                false
            }
        } else {
            // Disconnected: queue for replay after reconnect
            self.pending_downloads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(PendingDownload {
                    filename: filename.clone(),
                    username: username.clone(),
                    size,
                    download_directory,
                    token,
                    status_sender: download_sender,
                });
            true
        };

        drop(context);

        if !download_initiated {
            let _ = download.sender.send(DownloadStatus::Failed);
            self.context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .update_download_with_status(token, DownloadStatus::Failed);
        }

        Ok((download, download_receiver))
    }

    fn process_failed_uploads(
        client_context: Arc<RwLock<ClientContext>>,
        username: &str,
        filename: Option<&str>,
    ) {
        let context = client_context.read().unwrap_or_else(|e| e.into_inner());
        let failed_downloads: Vec<_> = context
            .get_downloads()
            .iter()
            .filter(|download| {
                download.username == username && (filename.is_none_or(|f| download.filename == *f))
            })
            .map(|download| {
                let _ = download.sender.send(DownloadStatus::Failed);
                download.token
            })
            .collect();
        drop(context);

        failed_downloads.iter().for_each(|token| {
            let mut context = client_context.write().unwrap_or_else(|e| e.into_inner());
            context.update_download_with_status(*token, DownloadStatus::Failed);
            context.remove_download(*token);
        });
    }

    fn listen_to_client_operations(
        mut reader: UnboundedReceiver<ClientOperation>,
        client_context: Arc<RwLock<ClientContext>>,
        own_username: String,
        cancellation_token: tokio_util::sync::CancellationToken,
        state: Arc<Mutex<ClientState>>,
        pending_downloads: Arc<Mutex<VecDeque<PendingDownload>>>,
        download_limiter: Option<Arc<Mutex<DownloadConcurrencyLimiter>>>,
    ) {
        tokio::spawn(async move {
            loop {
                let operation = tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        trace!("[client] Shutdown signal received");
                        break;
                    }
                    msg = reader.recv() => {
                        match msg {
                            Some(op) => op,
                            None => {
                                error!("[client] Channel closed");
                                break;
                            }
                        }
                    }
                };

                match operation {
                    ClientOperation::ServerDisconnected => {
                        *state.lock().unwrap_or_else(|e| e.into_inner()) =
                            ClientState::Disconnected;
                    }
                    ClientOperation::LoginSucceeded => {
                        *state.lock().unwrap_or_else(|e| e.into_inner()) = ClientState::Connected;

                        let pending: Vec<PendingDownload> = {
                            let mut pd =
                                pending_downloads.lock().unwrap_or_else(|e| e.into_inner());
                            pd.drain(..).collect()
                        };

                        for pd in pending {
                            let context =
                                client_context.read().unwrap_or_else(|e| e.into_inner());
                            if let Some(ref registry) = context.peer_registry {
                                if let Some(ref lim_arc) = download_limiter {
                                    let mut lim =
                                        lim_arc.lock().unwrap_or_else(|e| e.into_inner());
                                    if lim.active < lim.max_concurrent {
                                        lim.active += 1;
                                        drop(lim);
                                        let _ =
                                            registry.queue_upload(&pd.username, pd.filename);
                                    } else {
                                        lim.queue.push_back(pd);
                                    }
                                } else {
                                    let _ = registry.queue_upload(&pd.username, pd.filename);
                                }
                            }
                        }
                    }
                    ClientOperation::DownloadSlotFreed => {
                        if let Some(ref lim_arc) = download_limiter {
                            let next = {
                                let mut lim =
                                    lim_arc.lock().unwrap_or_else(|e| e.into_inner());
                                lim.active = lim.active.saturating_sub(1);
                                lim.queue.pop_front()
                            };
                            if let Some(pd) = next {
                                let context =
                                    client_context.read().unwrap_or_else(|e| e.into_inner());
                                if let Some(ref registry) = context.peer_registry {
                                    {
                                        let mut lim =
                                            lim_arc.lock().unwrap_or_else(|e| e.into_inner());
                                        lim.active += 1;
                                    }
                                    let _ = registry.queue_upload(&pd.username, pd.filename);
                                }
                            }
                        }
                    }
                    ClientOperation::ConnectToPeer(peer) => {
                        let client_context_clone = client_context.clone();
                        let own_username_clone = own_username.clone();

                        tokio::spawn(async move {
                            Self::connect_to_peer(
                                peer,
                                client_context_clone,
                                own_username_clone,
                                None,
                            );
                        });
                    }
                    ClientOperation::SearchResult(search_result) => {
                        trace!("[client] SearchResult {:?}", search_result);
                        let mut context = client_context.write().unwrap_or_else(|e| e.into_inner());
                        let result_token = search_result.token;

                        // Find the search with matching token
                        for search in context.searches.values_mut() {
                            if search.token == result_token {
                                search.results.push(search_result);
                                break;
                            }
                        }
                    }
                    ClientOperation::PeerDisconnected(username, error) => {
                        let context = client_context.read().unwrap_or_else(|e| e.into_inner());
                        if let Some(ref registry) = context.peer_registry
                            && let Some(handle) = registry.remove_peer(&username)
                        {
                            let _ = handle.stop();
                        }
                        if let Some(error) = error {
                            warn!(
                                "[client] Peer {} disconnected with error: {:?}",
                                username, error
                            );
                            Self::process_failed_uploads(
                                client_context.clone(),
                                &username,
                                None,
                            );
                        }
                    }
                    ClientOperation::PierceFireWall(peer) => {
                        Self::pierce_firewall(peer, client_context.clone(), own_username.clone());
                    }
                    ClientOperation::DownloadFromPeer(token, peer, allowed) => {
                        let maybe_download = {
                            let client_context = client_context.read().unwrap_or_else(|e| e.into_inner());
                            client_context.get_download_by_token(token).cloned()
                        };
                        let own_username = own_username.clone();
                        let client_context_clone = client_context.clone();

                        trace!(
                            "[client] DownloadFromPeer token: {} peer: {:?}",
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
                                    let filename: Option<&str> =
                                        download.filename.split('\\').next_back();
                                    match filename {
                                        Some(filename) => {
                                            match download_peer.download_file(
                                                client_context_clone.clone(),
                                                Some(download.clone()),
                                                None,
                                            ) {
                                                Ok((download, filename)) => {
                                                    let _ = download
                                                        .sender
                                                        .send(DownloadStatus::Completed);
                                                    client_context_clone
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
                                                    let _ = download
                                                        .sender
                                                        .send(DownloadStatus::Failed);
                                                    client_context_clone
                                                        .write()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .update_download_with_status(
                                                            download.token,
                                                            DownloadStatus::Failed,
                                                        );
                                                    error!(
                                                        "Failed to download file '{}' from {}:{} (token: {}) - Error: {}",
                                                        filename,
                                                        peer.host,
                                                        peer.port,
                                                        download.token,
                                                        e
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
                                    if let Ok(ctx) = client_context_clone.read() {
                                        if let Some(ref sender) = ctx.sender {
                                            let _ = sender.send(ClientOperation::DownloadSlotFreed);
                                        }
                                    }
                                });
                            }
                            None => {
                                error!("Can't find download with token {:?}", token);
                            }
                        };
                    }
                    ClientOperation::NewPeer(new_peer) => {
                        let peer_exists = client_context
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .peer_registry
                            .as_ref()
                            .map(|r| r.contains(&new_peer.username))
                            .unwrap_or(false);

                        if peer_exists {
                            debug!("Already connected to {}", new_peer.username);
                        } else if let Some(server_sender) =
                            &client_context
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .server_sender
                        {
                            server_sender
                                .send(ServerMessage::GetPeerAddress(new_peer.username.clone()))
                                .unwrap_or_else(|e| {
                                    error!("[client] Failed to send GetPeerAddress: {}", e)
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
                            client_context.clone(),
                            own_username.clone(),
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

                        let peer_exists = client_context
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
                            let client_context_clone = client_context.clone();
                            let own_username_clone = own_username.clone();

                            tokio::spawn(async move {
                                Self::connect_to_peer(
                                    peer,
                                    client_context_clone,
                                    own_username_clone,
                                    None,
                                );
                            });
                        }
                    }
                    ClientOperation::UpdateDownloadTokens(transfer, username) => {
                        let mut context = client_context.write().unwrap_or_else(|e| e.into_inner());

                        let download_to_update = context.get_downloads().iter().find_map(|d| {
                            if d.username == username && d.filename == transfer.filename {
                                Some((d.token, d.clone()))
                            } else {
                                None
                            }
                        });

                        if let Some((old_token, download)) = download_to_update {
                            trace!(
                                "[client] UpdateDownloadTokens found {old_token}, transfer: {:?}",
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
                        Self::process_failed_uploads(
                            client_context.clone(),
                            &username,
                            Some(&filename),
                        );
                    }
                    ClientOperation::SetServerSender(sender) => {
                        client_context
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .server_sender = Some(sender);
                        debug!("[client] Server sender initialized");
                    }
                }
            }
        });
    }

    fn connect_to_peer(
        peer: Peer,
        client_context: Arc<RwLock<ClientContext>>,
        own_username: String,
        stream: Option<std::net::TcpStream>,
    ) {
        let client_context = client_context.clone();

        let peer_clone = peer.clone();
        trace!(
            "[client] connecting to {}, with connection_type: {}, and token {:?}",
            peer.username, peer.connection_type, peer.token
        );
        match peer.connection_type {
            ConnectionType::P => {
                let username = peer.username.clone();

                // Convert std TcpStream to tokio if provided
                let tokio_stream = stream.and_then(|s| {
                    s.set_nonblocking(true).ok();
                    tokio::net::TcpStream::from_std(s).ok()
                });

                let context = client_context.read().unwrap_or_else(|e| e.into_inner());
                if let Some(ref registry) = context.peer_registry {
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
                    "[client] downloading from: {}, {:?}",
                    peer.username, peer.token
                );
                let download_peer = DownloadPeer::new(
                    peer.username,
                    peer.host,
                    peer.port,
                    peer.token.unwrap(),
                    false,
                    own_username.clone(),
                );

                match download_peer.download_file(client_context.clone(), None, stream) {
                    Ok((download, filename)) => {
                        trace!(
                            "[client] downloaded {} bytes {:?} ",
                            filename, download.size
                        );
                        let _ = download.sender.send(DownloadStatus::Completed);
                        client_context
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .update_download_with_status(download.token, DownloadStatus::Completed);
                    }
                    Err(e) => {
                        trace!("[client] failed to download: {}", e);
                    }
                }

                // Signal download slot freed (for concurrency limiter)
                if let Ok(ctx) = client_context.read() {
                    if let Some(ref sender) = ctx.sender {
                        let _ = sender.send(ClientOperation::DownloadSlotFreed);
                    }
                }
            }
            ConnectionType::D => {
                error!("ConnectionType::D not implemented")
            }
        }
    }

    fn pierce_firewall(
        peer: Peer,
        client_context: Arc<RwLock<ClientContext>>,
        own_username: String,
    ) {
        debug!("Piercing firewall for peer: {:?}", peer);

        let context = client_context.read().unwrap_or_else(|e| e.into_inner());
        if let Some(server_sender) = &context.server_sender {
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

        drop(context);
        Self::connect_to_peer(peer, client_context, own_username, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_context_downloads() {
        let mut context = ClientContext::new();
        let token = 123;
        let new_token = 1234;
        let download = Download {
            username: "test".to_string(),
            filename: "test.txt".to_string(),
            token,
            size: 100,
            download_directory: "test".to_string(),
            status: DownloadStatus::Queued,
            sender: mpsc::unbounded_channel().0,
        };
        context.add_download(download);
        assert!(context.get_download_by_token(123).is_some());
        assert_eq!(context.get_download_tokens(), vec![123]);
        assert_eq!(context.get_downloads().len(), 1);
        if let Some(download) = context.get_download_by_token_mut(token) {
            assert_eq!(download.token, token);
            download.token = new_token
        }
        assert!(context.get_download_by_token(new_token).is_some());
        assert_eq!(context.get_download_tokens(), vec![new_token]);
        context.remove_download(new_token);
        assert_eq!(context.get_downloads().len(), 0);
        assert!(context.get_download_by_token(1234).is_none());
    }

    #[test]
    fn test_client_settings_default_compiles() {
        let settings = ClientSettings::default();
        assert_eq!(settings.listen_port, 2234);
        assert!(settings.search_rate_limit_settings.is_some());
        assert!(settings.download_rate_limit_settings.is_some());
    }

    #[test]
    fn test_search_rate_limiter() {
        let mut lim = SearchRateLimiter::new(2, Duration::from_secs(10));
        assert!(lim.wait_duration().is_none());
        lim.record();
        assert!(lim.wait_duration().is_none());
        lim.record();
        // Now at limit — should need to wait
        assert!(lim.wait_duration().is_some());
    }
}
