use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::actor::server_actor::ServerMessage;
use crate::client::inner::PendingDownload;
use crate::path::SoulseekPath;
use crate::token::{DownloadToken, SearchToken};
use crate::{
    Transfer,
    error::SoulseekRs,
    peer::{NewPeer, Peer},
    types::{Download, SearchResult},
};

pub enum ClientOperation {
    NewPeer(NewPeer),
    ConnectToPeer(Peer),
    SearchResult(SearchResult),
    PeerDisconnected(String, Option<SoulseekRs>),
    PierceFireWall(Peer),
    DownloadFromPeer(DownloadToken, Peer, bool),
    UpdateDownloadTokens(Transfer, String),
    GetPeerAddressResponse {
        username: String,
        host: String,
        port: u32,
        obfuscation_type: u32,
        obfuscated_port: u16,
    },
    UploadFailed(String, SoulseekPath),
    SetServerSender(UnboundedSender<ServerMessage>),
    /// Server TCP connection was lost; reconnect will be handled by ServerActor.
    ServerDisconnected,
    /// (Re)login confirmed; replay pending downloads.
    LoginSucceeded,
    /// A download finished (success or failure); carries token and path-or-error.
    DownloadCompleted(DownloadToken, Result<String, SoulseekRs>),
    /// Initiate or queue a download; routed by ConnectedWorker.
    RequestDownload(PendingDownload),
    /// Pierce-firewall download failed before the download token was resolved; free the slot.
    PierceFirewallPreTokenFailed,
    /// Register a search entry in the worker (sent before FileSearch).
    InitiateSearch(SearchToken, String),
    /// Listener queries worker for a download by token.
    QueryDownloadByToken(DownloadToken, oneshot::Sender<Option<Download>>),
    /// Public API: query all downloads.
    QueryDownloads(oneshot::Sender<Vec<Download>>),
    /// Public API: query search results for a key.
    QuerySearchResults(String, oneshot::Sender<Vec<SearchResult>>),
}
