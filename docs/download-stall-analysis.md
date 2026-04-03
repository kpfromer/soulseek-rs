# Download Stall Analysis

Comparison of soulseek-rs download/peer handling against [slskd](https://github.com/nicholasgasior/slskd)
(`/tmp/slskd` — commit used as reference). Documents known gaps that cause downloads to queue up and stall.

---

## 1. CRITICAL — Queue response timeout is 30s vs slskd's 3 minutes

**File**: `soulseek-rs/src/client/connected_worker.rs:23`

```rust
const QUEUE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
```

**slskd equivalent** (`DownloadService.cs`):
```csharp
maxTimeToWaitForEnqueueRequestAck = TimeSpan.FromMinutes(3)
```

### What goes wrong

After `QueueUpload` is sent to a peer, the worker starts a 30-second countdown waiting
for a `TransferRequest` reply. In practice, peers have their own upload queues — you
might be #42 and the wait is several minutes. When the timer fires:

1. `DownloadResponseTimeout` removes the download from `self.downloads`.
2. The peer eventually sends `TransferRequest` → `UpdateDownloadTokens` finds no matching
   download → silently dropped.
3. The peer then sends `TransferResponse` → `DownloadFromPeer` searches by `peer_token`
   → not found → error logged, download permanently dead.
4. The peer actor stays alive (no crash), consuming a registry slot.
5. The user must retry manually.

### Fix

Increase `QUEUE_RESPONSE_TIMEOUT` to `Duration::from_secs(180)` to match slskd.
Additionally reset the deadline when a `PlaceInQueueResponse` is received (see §2).

---

## 2. MEDIUM — `PlaceInQueueResponse` is logged but not used to reset the timeout

**File**: `soulseek-rs/src/actor/peer_actor.rs:204`

```rust
PeerSignal::PlaceInQueueResponse { filename, place } => {
    debug!("[peer:{}] Place in queue response...");
    // just logs — no action taken
}
```

When a peer sends `PlaceInQueueResponse` it is actively confirming "you are still in my
queue at position N — please keep waiting." The 30-second (or future 3-minute) timeout
should be reset on every such message so legitimately-queued downloads are not killed.

### Fix

Forward `PlaceInQueueResponse` to the worker as a new `ClientOperation` (e.g.
`ClientOperation::ResetDownloadTimeout { username, filename }`) and in the worker find
the matching download by username + filename, then cancel and respawn the timeout task.

---

## 3. MEDIUM — `TransferResponse(allowed=false)` is logged but not acted on

**File**: `soulseek-rs/src/actor/peer_actor.rs:183`

```rust
if !allowed {
    if let Some(reason_text) = reason {
        debug!("Transfer rejected: {} ...");
    }
    // timer keeps running; nothing else happens
}
```

`TransferResponse(allowed=false)` means "I'm busy right now but you are in my queue."
The peer will send a `TransferRequest` when it is ready. The timeout continues ticking
unchanged, so a briefly-busy peer causes a spurious timeout.

### Fix

Treat `TransferResponse(allowed=false)` the same as `PlaceInQueueResponse`: reset the
queue-response deadline so the download keeps waiting.

---

## 4. MEDIUM — `connect_f` snapshot may miss newly-set `peer_token` (race)

**File**: `soulseek-rs/src/client/connected_worker.rs:423`

```rust
let peer_downloads: HashMap<PeerTransferToken, Download> = downloads
    .values()
    .filter_map(|d| d.peer_token.map(|pt| (pt, d.clone())))
    .collect();
```

This snapshot only includes downloads where `peer_token` has already been set by an
earlier `UpdateDownloadTokens` (which comes from the peer's `TransferRequest` over the
P-type connection).

If the server's `ConnectToPeer(F)` message arrives and is processed by the worker
*before* the peer's `TransferRequest` is processed, `peer_downloads` will be empty. The
pierce-firewall handshake then fails with `TokenNotFound` and the download is marked
failed.

### Fix

Options in order of complexity:
- Delay `connect_f` until `peer_token` is set (query the worker just before connecting).
- Have `connect_f` retry the token lookup from the live downloads map rather than a
  snapshot, by sending a `QueryDownloadByToken` oneshot at the point the token is read
  from the wire.

---

## 5. MINOR — `PeerDisconnected` fails ALL downloads for a peer

**File**: `soulseek-rs/src/client/connected_worker.rs:148`

```rust
ClientOperation::PeerDisconnected(username, maybe_error) => {
    ...
    self.process_failed_uploads(&username, None); // None = every file from this peer
}
```

The P-type peer actor and the F-type file-transfer TCP connection are independent.
If the peer actor's messaging connection drops (e.g. brief TCP reset), any in-flight
F-type transfer (which runs in its own blocking task with its own socket) will be
killed in the downloads map even though the actual byte stream may still be healthy.

### Fix

Before failing a download in `process_failed_uploads`, check whether it already has
`status == InProgress` (or has a live `peer_token`). Downloads that have progressed
past the queuing stage should not be cancelled by a P-type disconnect.

---

## 6. MINOR — Double-insert of download in `RequestDownload` + `try_initiate`

**File**: `soulseek-rs/src/client/connected_worker.rs:109,314`

`pd.to_download()` is called in `RequestDownload` and again inside `try_initiate`,
inserting the same token twice. Not a correctness bug (both `Download` instances share
the same `status_sender` via `Clone`), but it is redundant and may cause confusion.

### Fix

Remove the insert from `RequestDownload` and rely solely on `try_initiate` (or the
`pending.push_back` path) to insert into `self.downloads`.

---

## Protocol flow reference (for context)

```
Client                   Peer (P-type)             Peer (F-type)
  |                           |                          |
  |-- QueueUpload ----------->|                          |
  |<- TransferResponse(false) |  (peer is busy, wait)   |
  |<- PlaceInQueueResponse    |  (you are #N in queue)  |
  |<- TransferRequest ------->|  (peer is ready)         |
  |-- TransferResponse(true) ->|                         |
  |                            |                         |
  |<========================== F-type connection ========|
  |<-- [4-byte token][file data] ======================= |
```

The `peer_token` is not set until `TransferRequest` arrives. All timeout logic must
account for the full delay between `QueueUpload` and `TransferRequest`.
