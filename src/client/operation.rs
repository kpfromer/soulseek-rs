use crate::actor::server_actor::{ServerActor, ServerMessage};
use crate::search_rate_limiter::SlidingRateLimiter;
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
use crate::{debug, error, info, trace, warn};
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
use tokio_util::sync::CancellationToken;

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
