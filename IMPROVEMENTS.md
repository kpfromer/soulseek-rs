# Clarity & Cognitive Load Improvements

Ranked by impact. None are blocking; all are incremental.

---

## 1. Split `ClientOperation` by concern

**File:** `soulseek-rs/src/client/operation.rs`, `connected_worker.rs`

`ClientOperation` mixes four unrelated concerns into one 14-variant enum. The `ConnectedWorker::handle_operation` match grows to ~200 lines as a result.

**Proposed split:**

```rust
pub(crate) enum NetworkEvent {
    NewPeer(NewPeer),
    ConnectToPeer(Peer),
    PeerDisconnected(String, Option<SoulseekRs>),
    PierceFireWall(Peer),
    GetPeerAddressResponse { username, host, port, obfuscation_type, obfuscated_port },
    ServerDisconnected,
    LoginSucceeded,
}

pub(crate) enum DownloadEvent {
    RequestDownload(PendingDownload),
    DownloadFromPeer(DownloadToken, Peer, bool),
    UpdateDownloadTokens(Transfer, String),
    DownloadCompleted(DownloadToken, Result<String, SoulseekRs>),
    UploadFailed(String, SoulseekPath),
}

pub(crate) enum SearchEvent {
    InitiateSearch(SearchToken, String),
    SearchResult(SearchResult),
}

pub(crate) enum Query {
    DownloadByToken(DownloadToken, oneshot::Sender<Option<Download>>),
    AllDownloads(oneshot::Sender<Vec<Download>>),
    SearchResults(String, oneshot::Sender<Vec<SearchResult>>),
}
```

Alternatively, keep one enum but split the match into focused methods:
`handle_network_event()`, `handle_download_event()`, `handle_query()`.

---

## 2. Eliminate `PendingDownload` — merge into `Download`

**File:** `soulseek-rs/src/client/inner.rs`, `types.rs`

`PendingDownload` and `Download` carry nearly identical fields. The `to_download()` conversion is a code smell. The only real difference is field naming (`status_sender` vs `sender`) and the initial status value.

**Proposed change:** Remove `PendingDownload`. Construct a `Download` directly with `status: DownloadStatus::Queued`. Store it in the queue and the downloads map as the same type.

---

## 3. Rename `process_failed_uploads` → `handle_peer_download_failure`

**File:** `soulseek-rs/src/client/connected_worker.rs`

The current name is from the peer's perspective ("their upload failed"). The method actually handles *our* download failing. The reversed perspective adds a translation step every time the method is read.

---

## 4. Rename `connect_p` / `connect_f` to protocol-agnostic names

**File:** `soulseek-rs/src/client/connected_worker.rs` (`PeerConnector`)

`P` and `F` are raw wire connection-type codes. Anyone without the Soulseek protocol spec can't reason about them.

```rust
fn connect_peer_messaging(...)  // was connect_p — establishes the messaging/browse channel
fn connect_file_transfer(...)   // was connect_f — establishes a file download channel
```

---

## 5. Rename `UpdateDownloadTokens` → `RemapDownloadToken`

**File:** `soulseek-rs/src/client/operation.rs`

This operation re-keys a download entry from our locally-generated token to the one the peer sent on the wire during `TransferRequest`. The current name doesn't convey the rekeying intent.

`RemapDownloadToken(Transfer, String)` or `ReconcileTransferToken(Transfer, String)` makes the purpose clear.

---

## 6. Document `DownloadHandle` drop-cancel behavior

**File:** `soulseek-rs/src/client/download_handle.rs`

The `Drop` impl silently cancels the download. This surprises callers who just want to stop tracking progress without aborting the transfer.

**Option A** — doc comment at the type level:
```rust
/// Dropping this handle automatically cancels the in-progress download.
/// To let the download continue unattended, call [`DownloadHandle::detach`] first.
```

**Option B** — add a `detach(self)` method that sets a flag suppressing the drop-cancel, consuming the handle.
