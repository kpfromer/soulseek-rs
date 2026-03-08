use crate::actor::server_actor::ServerMessage;
use crate::client::inner::PendingDownload;
use crate::path::SoulseekPath;
use crate::token::DownloadToken;
use crate::{
    Transfer,
    error::SoulseekRs,
    peer::{NewPeer, Peer},
    types::SearchResult,
};
use tokio::sync::mpsc::UnboundedSender;

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
    /// A download finished; dequeue next from concurrency queue.
    DownloadSlotFreed,
    /// Initiate or queue a download; routed by ConnectedWorker.
    RequestDownload(PendingDownload),
}
