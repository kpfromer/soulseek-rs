# Architecture Recommendations

Observations based on comparison with slskd and reading the current codebase.
These are design-level suggestions, not bug fixes — for bugs see `download-stall-analysis.md`.

---

## 1. Richer `DownloadStatus` with `QueuedRemotely`

**Current** (`types.rs`):
```rust
pub enum DownloadStatus {
    Queued,
    InProgress { ... },
    Completed, Failed, TimedOut, Cancelled,
}
```

`Queued` is used for two very different situations:
- **Locally queued**: we haven't even sent `QueueUpload` yet (waiting for a concurrency slot)
- **Remotely queued**: the peer has acknowledged us and we're waiting in their upload queue

slskd models these as `Queued | Locally` and `Queued | Remotely` (bitwise state flags)
and applies different timeout semantics to each.

**Recommended**:
```rust
pub enum DownloadStatus {
    /// Waiting for a local concurrency slot; QueueUpload not yet sent.
    QueuedLocally,
    /// Peer acknowledged us; waiting for TransferRequest.
    /// `place` is updated by PlaceInQueueResponse messages.
    QueuedRemotely { place: Option<u32> },
    InProgress { bytes_downloaded: u64, total_bytes: u64, speed_bytes_per_sec: f64 },
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}
```

This distinction unlocks several things:
- The short "did the peer respond at all?" timeout only applies to `QueuedLocally`.
- `PlaceInQueueResponse` transitions the download to `QueuedRemotely` (confirming the
  peer has us) and updates `place`. The timeout resets or is removed entirely.
- `TransferResponse(allowed=false)` also transitions to `QueuedRemotely`.
- `PeerDisconnected` can avoid killing downloads that are already `QueuedRemotely` or
  `InProgress` (different sockets — see issue #5 in the stall doc).

---

## 2. Store the queue-response timeout handle on the download

**Current**: the timeout is a fire-and-forget `tokio::spawn` (`connected_worker.rs:321`).
It checks `peer_token.is_none()` at expiry, which is a proxy for "did we get a
`TransferRequest`?" but can't be cancelled or reset.

**Recommended**: store an `AbortHandle` (or `CancellationToken`) on each download:

```rust
pub struct Download {
    // ...existing fields...
    /// Handle to the queue-response timeout task.
    /// Abort this when the download transitions out of QueuedLocally.
    pub queue_timeout_handle: Option<tokio::task::AbortHandle>,
}
```

Then in `try_initiate`:
```rust
let handle = tokio::spawn(async move { ... }).abort_handle();
download.queue_timeout_handle = Some(handle);
```

And in `UpdateDownloadTokens` / `PlaceInQueueResponse` handlers:
```rust
if let Some(h) = download.queue_timeout_handle.take() { h.abort(); }
// optionally spawn a new, longer timeout
```

This makes timeout management explicit and testable instead of relying on side-effect
checks. It also makes it trivial to reset the timeout when the peer sends a queue
position update.

---

## 3. Extract a `DownloadManager` from `ConnectedWorker`

**Current**: `ConnectedWorker` is a god object that handles downloads, searches, peer
lifecycle, server state, and internal queries in one 380-line match statement.

slskd splits this into `DownloadService.cs` (download lifecycle) vs `Application.cs`
(wiring). The Rust equivalent would be a focused struct that owns only download state:

```
ConnectedWorker  (thin dispatcher — routes ClientOperation variants)
├── DownloadManager  (pending queue, active_slots, downloads map, all download logic)
└── SearchManager    (searches map, result accumulation)
```

`DownloadManager` would own:
- `pending: VecDeque<PendingDownload>`
- `active_slots: HashMap<DownloadToken, DownloadSlot>`
- `downloads: HashMap<DownloadToken, Download>`
- `max_concurrent: Option<u32>`
- All methods: `try_initiate`, `try_dequeue_next`, `process_failed_uploads`, `on_completed`, `on_peer_disconnected`

`ConnectedWorker::handle_operation` becomes a thin match that delegates:
```rust
ClientOperation::RequestDownload(pd)       => self.downloads.enqueue(pd),
ClientOperation::DownloadCompleted(t, r)   => self.downloads.on_completed(t, r),
ClientOperation::PeerDisconnected(u, e)    => { self.downloads.on_peer_disconnected(&u); ... }
ClientOperation::InitiateSearch(tok, q)    => self.searches.initiate(tok, q),
ClientOperation::SearchResult(r)           => self.searches.on_result(r),
// ...
```

This makes the flow of each domain trivially followable in isolation.

---

## 4. Split `ClientOperation` into domain-specific message types

**Current**: one 18-variant enum flows through a single `UnboundedSender<ClientOperation>`.
Every component sends to the same channel; the worker dispatches all of it.

The variants fall into four natural groups:

| Group | Variants |
|---|---|
| Server lifecycle | `ServerDisconnected`, `LoginSucceeded`, `GetPeerAddressResponse` |
| Peer lifecycle | `NewPeer`, `ConnectToPeer`, `PeerDisconnected`, `PierceFireWall` |
| Download events | `RequestDownload`, `DownloadFromPeer`, `UpdateDownloadTokens`, `UploadFailed`, `DownloadCompleted`, `DownloadResponseTimeout`, `CancelDownload` |
| Queries | `QueryDownloadByToken`, `QueryDownloads`, `QuerySearchResults` |
| Search events | `InitiateSearch`, `SearchResult` |

**Option A** (low friction): keep one enum but group the variants with comments and
handle each group in a dedicated private method on `ConnectedWorker`:
```rust
fn handle_operation(&mut self, op: ClientOperation) {
    match op {
        op if op.is_download_event() => self.download_manager.handle(op),
        op if op.is_search_event()   => self.search_manager.handle(op),
        // ...
    }
}
```

**Option B** (cleaner, more work): separate sender/receiver pairs per domain.
The listener and peer actors send download events on `download_tx`, searches on
`search_tx`, etc. `ConnectedWorker` holds multiple receivers and selects on all of them.
This enables independent back-pressure and makes it obvious which component produces
which events.

Option A is the pragmatic starting point; Option B is worth it if the codebase grows.

---

## 5. Fix the `connect_f` live-lookup vs snapshot race

**Current** (`connected_worker.rs:423`): `connect_f` receives a cloned snapshot of
`self.downloads` at the moment `ConnectToPeer(F)` / `PierceFireWall` is processed. The
snapshot only contains downloads where `peer_token` is already set. If the server's
`ConnectToPeer` races ahead of the peer's `TransferRequest`, the snapshot is empty and
the download fails with `TokenNotFound`.

slskd avoids this because Soulseek.NET's internal state tracks the mapping itself.

**Recommended**: instead of resolving the token inside the blocking task from a snapshot,
send a `QueryDownloadByToken` oneshot back to the worker at the moment the wire token is
read from the peer. The blocking task pauses on the oneshot result:

```rust
// Inside the blocking task, after reading the 4-byte token from the wire:
let (tx, rx) = oneshot::channel();
op_tx.send(ClientOperation::QueryDownloadByToken(token, tx))?;
let download = rx.recv()?  // worker looks up the live map
    .ok_or(DownloadError::TokenNotFound(token))?;
```

The `QueryDownloadByToken` path already exists (`listen.rs` uses it). Reusing it for
`connect_f` makes the two paths consistent and eliminates the snapshot race.

---

## 6. `PendingDownload` and `Download` should be one type

**Current**: `PendingDownload` (`inner.rs`) and `Download` (`types.rs`) carry almost the
same fields. `to_download()` creates a `Download` from a `PendingDownload` and both live
for different parts of the lifecycle.

In Rust the natural model is a single type with a state enum (see §1). Once
`DownloadStatus` captures the full lifecycle (`QueuedLocally` → `QueuedRemotely` →
`InProgress` → terminal), there is no need for a separate `PendingDownload` type — a
`Download` in `QueuedLocally` state is the pending download. This eliminates the
double-insert in `RequestDownload` + `try_initiate` and removes `to_download()`.

---

## Summary / suggested order of attack

| # | Change | Effort | Payoff |
|---|--------|--------|--------|
| 1 | Add `QueuedRemotely` to `DownloadStatus` | Medium | High — unlocks timeout fixes |
| 2 | Store `AbortHandle` on download | Small | High — makes timeouts resettable |
| 3 | Extract `DownloadManager` | Medium | High — codebase clarity |
| 5 | `connect_f` live lookup | Small | Medium — removes silent race failure |
| 4 | Split `ClientOperation` | Medium | Medium — best done after #3 |
| 6 | Merge `PendingDownload` into `Download` | Medium | Low-Medium — cleanliness |

Items 1 and 2 together resolve the stalling issues documented in `download-stall-analysis.md`
while also improving the architecture. Item 3 makes subsequent changes easier to reason about.
