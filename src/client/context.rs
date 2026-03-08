use std::collections::HashMap;
use std::sync::Arc;

use crate::actor::ActorSystem;
use crate::peer::PeerRegistry;
use crate::token::DownloadToken;
use crate::types::DownloadStatus;
use crate::types::{Download, Search};

pub struct ClientContext {
    pub peer_registry: Option<PeerRegistry>,
    pub searches: HashMap<String, Search>,
    pub downloads: HashMap<DownloadToken, Download>,
    pub actor_system: Arc<ActorSystem>,
}

impl ClientContext {
    #[must_use]
    pub fn new() -> Self {
        let actor_system = Arc::new(ActorSystem::new());

        Self {
            peer_registry: None,
            searches: HashMap::new(),
            downloads: HashMap::new(),
            actor_system,
        }
    }
}

impl Default for ClientContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientContext {
    pub fn add_download(&mut self, download: Download) {
        self.downloads.insert(download.token, download);
    }

    pub fn remove_download(&mut self, token: DownloadToken) {
        self.downloads.remove(&token);
    }

    pub fn get_download_by_token(&self, token: DownloadToken) -> Option<&Download> {
        self.downloads.get(&token)
    }

    pub fn get_download_by_token_mut(&mut self, token: DownloadToken) -> Option<&mut Download> {
        self.downloads.get_mut(&token)
    }

    pub fn get_download_tokens(&self) -> Vec<DownloadToken> {
        self.downloads.keys().copied().collect()
    }

    pub fn get_downloads(&self) -> Vec<Download> {
        self.downloads.values().cloned().collect()
    }

    pub fn update_download_with_status(&mut self, token: DownloadToken, status: DownloadStatus) {
        if let Some(download) = self.get_download_by_token_mut(token) {
            download.status = status;
        }
    }
}
