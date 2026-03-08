use crate::actor::server_actor::{ServerActor, ServerMessage};
use crate::path::SoulseekPath;
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
use crate::token::{DownloadToken, SearchToken};
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

        Self {
            settings,
            inner: Arc::new(Mutex::new(ClientInner {
                state: ClientState::Disconnected,
                active: None,
                pending_downloads: VecDeque::new(),
                search_limiter,
            })),
        }
    }

    /// Shutdown all spawned tasks (actors, listener, worker).
    /// Called automatically on drop.
    pub fn shutdown(&self) {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref active) = guard.active {
            active.actor_system.shutdown();
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
        let max_concurrent = self
            .settings
            .download_rate_limit_settings
            .as_ref()
            .map(|s| s.concurrent_downloads);

        // Create actor_system upfront — no lock needed later to retrieve it.
        let actor_system = Arc::new(crate::actor::ActorSystem::new());
        let cancellation_token = actor_system.cancellation_token().clone();

        let (op_tx, op_rx) = mpsc::unbounded_channel::<ClientOperation>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WorkerEvent>();

        // Build fully-initialized context before spawning anything.
        let context = {
            let peer_registry =
                PeerRegistry::new(actor_system.clone(), op_tx.clone(), username.clone());
            let mut ctx = ClientContext::new();
            ctx.peer_registry = Some(peer_registry);
            Arc::new(RwLock::new(ctx))
        };

        // Drain any pre-connect pending downloads into the worker's queue.
        let pre_pending = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.pending_downloads.drain(..).collect::<VecDeque<_>>()
        };

        // Spawn ConnectedWorker FIRST so it is already consuming op_tx when ServerActor starts.
        let worker = ConnectedWorker {
            own_username: username.clone(),
            op_tx: op_tx.clone(),
            op_rx,
            event_tx,
            context: context.clone(),
            cancellation_token: cancellation_token.clone(),
            server_sender: None,
            logged_in: false,
            pending: pre_pending,
            max_concurrent,
            active_downloads: 0,
        };
        tokio::spawn(async move { worker.run().await });

        // Spawn state_monitor
        let inner_clone = self.inner.clone();
        tokio::spawn(async move { state_monitor(event_rx, inner_clone).await });

        // Spawn ServerActor — starts sending to op_tx; worker is already running.
        let server_actor = ServerActor::new(
            self.settings.server_address.clone(),
            op_tx.clone(),
            self.settings.listen_port,
            self.settings.enable_listen,
            self.settings.tcp_keepalive_settings.clone(),
            self.settings.reconnect_settings.clone(),
        );

        let server_handle = actor_system.spawn_with_handle(server_actor, |actor, handle| {
            actor.set_self_handle(handle);
        });

        // Store active connection (after server handle exists).
        {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.active = Some(ActiveConnection {
                server_handle: server_handle.clone(),
                context: context.clone(),
                op_tx: op_tx.clone(),
                actor_system: actor_system.clone(),
            });
        }

        // Spawn listener task
        if self.settings.enable_listen {
            let listen_port = self.settings.listen_port;
            let own_username = username.clone();
            let token = cancellation_token.clone();

            tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => {
                        trace!("[listener] Shutdown signal received");
                    }
                    result = Listen::start(listen_port, op_tx, context, own_username) => {
                        match result {
                            Ok(_) => info!("[listener] Listener started successfully"),
                            Err(e) => error!("[listener] Failed to start listener: {}", e),
                        }
                    }
                }
            });
        }

        // Send Login — queued by ServerActor until TCP is ready.
        let (login_tx, login_rx) = tokio::sync::oneshot::channel();
        let _ = server_handle.send(ServerMessage::Login {
            username: username.clone(),
            password: password.clone(),
            response: login_tx,
        });

        // Await login result. ClientState::Connected is set by state_monitor on LoginSucceeded.
        match login_rx.await {
            Ok(Ok(true)) => Ok(()),
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
        let token = SearchToken(u32::from_str_radix(&hash[0..5], 16)?);

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
        } else {
            vec![]
        }
    }

    pub fn download(
        &self,
        filename: impl Into<SoulseekPath>,
        username: String,
        size: u64,
        download_directory: String,
    ) -> Result<(Download, UnboundedReceiver<DownloadStatus>)> {
        let filename: SoulseekPath = filename.into();
        info!("[client] Downloading {} from {}", filename, username);

        let hash = md5::md5(filename.as_str());
        let token = DownloadToken(u32::from_str_radix(&hash[0..5], 16)?);

        let (download_sender, download_receiver) = mpsc::unbounded_channel::<DownloadStatus>();

        let pending = PendingDownload {
            filename: filename.clone(),
            username: username.clone(),
            size,
            download_directory,
            token,
            status_sender: download_sender,
        };
        let download = pending.to_download();

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(ref active) = guard.active {
            // Active connection exists — add to context and let the worker route it.
            active
                .context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .add_download(download.clone());
            let _ = active.op_tx.send(ClientOperation::RequestDownload(pending));
        } else {
            // No active connection yet — buffer until connect().
            guard.pending_downloads.push_back(pending);
        }

        Ok((download, download_receiver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_context_downloads() {
        use crate::token::DownloadToken;
        let mut context = ClientContext::new();
        let token = DownloadToken(123);
        let download = Download {
            username: "test".to_string(),
            filename: SoulseekPath::from("test.txt"),
            token,
            size: 100,
            download_directory: "test".to_string(),
            status: DownloadStatus::Queued,
            sender: mpsc::unbounded_channel().0,
        };
        context.add_download(download);
        assert!(context.get_download_by_token(token).is_some());
        assert_eq!(context.get_download_tokens(), vec![token]);
        assert_eq!(context.get_downloads().len(), 1);
        if let Some(download) = context.get_download_by_token_mut(token) {
            assert_eq!(download.token, token);
            download.status = DownloadStatus::Failed;
        }
        assert!(matches!(
            context.get_download_by_token(token).unwrap().status,
            DownloadStatus::Failed
        ));
        context.remove_download(token);
        assert_eq!(context.get_downloads().len(), 0);
        assert!(context.get_download_by_token(token).is_none());
    }

    #[test]
    fn test_client_settings_default_compiles() {
        let settings = ClientSettings::default();
        assert_eq!(settings.listen_port, 2234);
        assert!(settings.search_rate_limit_settings.is_some());
        assert!(settings.download_rate_limit_settings.is_some());
    }
}
