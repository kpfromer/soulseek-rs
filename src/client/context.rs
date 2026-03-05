use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use crate::actor::ActorSystem;
use crate::actor::server_actor::ServerMessage;
use crate::peer::PeerRegistry;
use crate::types::DownloadStatus;
use crate::types::{Download, Search};

use super::ClientOperation;

pub struct ClientContext {
    pub peer_registry: Option<PeerRegistry>,
    pub sender: Option<UnboundedSender<ClientOperation>>,
    pub server_sender: Option<UnboundedSender<ServerMessage>>,
    pub searches: HashMap<String, Search>,
    pub downloads: Vec<Download>,
    pub actor_system: Arc<ActorSystem>,
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
