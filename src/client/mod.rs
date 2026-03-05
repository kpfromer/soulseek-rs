use crate::actor::server_actor::{ServerActor, ServerMessage};
use crate::search_rate_limiter::SlidingRateLimiter;
use crate::types::DownloadStatus;
use crate::utils::logger;
use crate::{
    actor::peer_registry::PeerRegistry,
    error::{Result, SoulseekRs},
    peer::listen::Listen,
    types::{Download, Search, SearchResult},
    utils::md5,
};
use crate::{error, info, trace};
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
use tokio::sync::mpsc::{self, UnboundedReceiver};

mod connected_worker;
mod context;
mod inner;
pub(super) mod operation;
mod settings;
pub(super) mod state_monitor;

use connected_worker::ConnectedWorker;
pub use context::ClientContext;
pub use inner::{ActiveConnection, ClientInner, ClientState, PendingDownload};
pub use operation::ClientOperation;
pub use settings::*;
use state_monitor::{WorkerEvent, state_monitor};

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
pub struct Client {
    settings: ClientSettings,
    /// Single lock for all mutable state.
    inner: Arc<Mutex<ClientInner>>,
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

        let search_limiter = settings
            .search_rate_limit_settings
            .as_ref()
            .map(|s| SlidingRateLimiter::new(s.searches, s.per_period));

        let download_limiter = settings
            .download_rate_limit_settings
            .as_ref()
            .map(|s| DownloadConcurrencyLimiter::new(s.concurrent_downloads));

        Self {
            settings,
            inner: Arc::new(Mutex::new(ClientInner {
                state: ClientState::Disconnected,
                active: None,
                pending_downloads: VecDeque::new(),
                search_limiter,
                download_limiter,
            })),
        }
    }

    /// Shutdown all spawned tasks (actors, listener, worker).
    /// Called automatically on drop.
    pub fn shutdown(&self) {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            if let Ok(ctx) = active.context.read() {
                ctx.actor_system.shutdown();
            }
        }
    }

    /// Connect to the Soulseek server and login. Blocks until login succeeds or fails.
    pub async fn connect(&mut self) -> Result<()> {
        {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(guard.state, ClientState::Connected) {
                return Ok(());
            }
        }

        self.inner.lock().unwrap_or_else(|e| e.into_inner()).state = ClientState::Connecting;

        let username = self.settings.username.0.clone();
        let password = self.settings.password.0.clone();

        let (op_tx, op_rx) = mpsc::unbounded_channel::<ClientOperation>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WorkerEvent>();

        // Build context
        let context = Arc::new(RwLock::new(ClientContext::new()));
        {
            let mut ctx = context.write().unwrap_or_else(|e| e.into_inner());
            ctx.sender = Some(op_tx.clone());
            let peer_registry =
                PeerRegistry::new(ctx.actor_system.clone(), op_tx.clone(), username.clone());
            ctx.peer_registry = Some(peer_registry);
        }

        // Spawn ServerActor
        let server_actor = ServerActor::new(
            self.settings.server_address.clone(),
            op_tx.clone(),
            self.settings.listen_port,
            self.settings.enable_listen,
            self.settings.tcp_keepalive_settings.clone(),
            self.settings.reconnect_settings.clone(),
        );

        let actor_system = context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .actor_system
            .clone();

        let server_handle = actor_system.spawn_with_handle(server_actor, |actor, handle| {
            actor.set_self_handle(handle);
        });

        // Store active connection
        {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.active = Some(ActiveConnection {
                server_handle: server_handle.clone(),
                context: context.clone(),
            });
        }

        // Send Login — queued by ServerActor until TCP is ready
        let (login_tx, login_rx) = tokio::sync::oneshot::channel();
        let _ = server_handle.send(ServerMessage::Login {
            username: username.clone(),
            password: password.clone(),
            response: login_tx,
        });

        let cancellation_token = actor_system.cancellation_token().clone();

        // Spawn listener task
        if self.settings.enable_listen {
            let listen_port = self.settings.listen_port;
            let client_sender = op_tx.clone();
            let context_clone = context.clone();
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
                        context_clone,
                        own_username,
                    ) => {
                        match result {
                            Ok(_) => info!("[listener] Listener started successfully"),
                            Err(e) => error!("[listener] Failed to start listener: {}", e),
                        }
                    }
                }
            });
        }

        // Spawn ConnectedWorker
        let worker = ConnectedWorker {
            own_username: username.clone(),
            op_rx,
            event_tx,
            context: context.clone(),
            cancellation_token: cancellation_token.clone(),
        };
        tokio::spawn(async move { worker.run().await });

        // Spawn state_monitor
        let inner_clone = self.inner.clone();
        tokio::spawn(async move { state_monitor(event_rx, inner_clone).await });

        // Await login result
        match login_rx.await {
            Ok(Ok(true)) => {
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).state = ClientState::Connected;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Ok(Ok(false)) => Err(SoulseekRs::AuthenticationFailed),
            Err(_) => Err(SoulseekRs::Timeout),
        }
    }

    #[allow(dead_code)]
    pub fn remove_peer(&self, username: &str) {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            let ctx = active.context.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref registry) = ctx.peer_registry
                && let Some(handle) = registry.remove_peer(username)
            {
                let _ = handle.stop();
            }
        }
    }

    pub async fn search(&self, query: &str, timeout: Duration) -> Result<Vec<SearchResult>> {
        self.search_with_cancel(query, timeout, None).await
    }

    async fn wait_for_search_rate_limit(&self) -> Result<()> {
        let wait = {
            if let Some(ref mut lim) = self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .search_limiter
            {
                Some(lim.clone().acquire())
            } else {
                None
            }
        };
        if let Some(wait) = wait {
            wait.await;
        }
        Ok(())
    }

    pub async fn search_with_cancel(
        &self,
        query: &str,
        timeout: Duration,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<SearchResult>> {
        info!("Searching for {}", query);

        // Record search and get connection handles
        let (server_handle, context) = {
            self.wait_for_search_rate_limit().await?;
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match guard.active.as_ref() {
                Some(active) => (active.server_handle.clone(), active.context.clone()),
                None => return Err(SoulseekRs::NotConnected),
            }
        };

        let hash = md5::md5(query);
        let token = u32::from_str_radix(&hash[0..5], 16)?;

        // Insert search entry and send FileSearch
        context
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

        let _ = server_handle.send(ServerMessage::FileSearch {
            token,
            query: query.to_string(),
        });

        // Poll until timeout or cancel
        let start = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;

            if let Some(ref flag) = cancel_flag
                && flag.load(Ordering::Relaxed)
            {
                info!("Search cancelled by user");
                break;
            }

            if start.elapsed() >= timeout {
                break;
            }
        }

        Ok(self.get_search_results(query))
    }

    pub fn get_search_results_count(&self, search_key: &str) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            active
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .searches
                .get(search_key)
                .map(|s| s.results.len())
                .unwrap_or(0)
        } else {
            0
        }
    }

    pub fn get_search_results(&self, search_key: &str) -> Vec<SearchResult> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            active
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .searches
                .get(search_key)
                .map(|s| s.results.clone())
                .unwrap_or_default()
        } else {
            vec![]
        }
    }

    /// Non-blocking variant that returns None if the lock is unavailable.
    pub fn try_get_search_results(&self, search_key: &str) -> Option<Vec<SearchResult>> {
        let guard = self.inner.try_lock().ok()?;
        let active = guard.active.as_ref()?;
        active
            .context
            .try_read()
            .ok()
            .and_then(|ctx| ctx.searches.get(search_key).map(|s| s.results.clone()))
    }

    pub fn get_all_searches(&self) -> HashMap<String, Search> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            active
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .searches
                .clone()
        } else {
            HashMap::new()
        }
    }

    pub fn get_all_downloads(&self) -> Vec<Download> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            active
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get_downloads()
                .clone()
        } else {
            vec![]
        }
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

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let is_connected = matches!(guard.state, ClientState::Connected);

        // Extract context Arc before needing to borrow guard for download_limiter
        let context_arc = guard.active.as_ref().map(|a| a.context.clone());

        if let Some(context_arc) = context_arc {
            // Add download to context regardless of connection state
            context_arc
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .add_download(download.clone());

            if is_connected {
                let mut ctx = context_arc.write().unwrap_or_else(|e| e.into_inner());
                let initiated = if let Some(ref registry) = ctx.peer_registry {
                    if let Some(ref mut lim) = guard.download_limiter {
                        if lim.active < lim.max_concurrent {
                            lim.active += 1;
                            registry
                                .queue_upload(&username, download.filename.clone())
                                .is_ok()
                        } else {
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
                };

                if !initiated {
                    let _ = download.sender.send(DownloadStatus::Failed);
                    ctx.update_download_with_status(token, DownloadStatus::Failed);
                }
            } else {
                guard.pending_downloads.push_back(PendingDownload {
                    filename: filename.clone(),
                    username: username.clone(),
                    size,
                    download_directory,
                    token,
                    status_sender: download_sender,
                });
            }
        } else {
            // No active connection yet — queue for replay after first connect
            guard.pending_downloads.push_back(PendingDownload {
                filename: filename.clone(),
                username: username.clone(),
                size,
                download_directory,
                token,
                status_sender: download_sender,
            });
        }

        Ok((download, download_receiver))
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
}
