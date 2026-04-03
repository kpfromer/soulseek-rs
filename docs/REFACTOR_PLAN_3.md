# Refactor Plan 3: Peer Actor, Listener, and Miscellaneous Issues

This document covers remaining issues: peer actor internals, missing timeouts,
silent failure modes, and hardcoded values.

---

## Issue 1: `PeerActor` has the same polling problems as `ServerActor`

**Location:** `src/actor/peer_actor.rs` — `process_read()`, `send_message()`

`PeerActor` uses the same tick-driven `try_read()` / `try_write()` pattern as
`ServerActor`:

- `process_read()` is called every 100 ms via `tick()` — up to 100 ms latency
  on every inbound peer message
- `send_message()` uses `try_write()` and silently drops messages on
  `WouldBlock`

Since there can be many peer actors running simultaneously (one per connected
peer), the cost is multiplied. A search with 50 responding peers means 50
actors each polling every 100 ms, and each one potentially dropping outbound
messages under any write pressure.

**Fix:** same as `ServerActor` — proper async reads (`readable().await`) and a
write buffer drained on `writable().await`. This likely requires revisiting the
`Actor` trait to support async select branches, or moving peer I/O out of the
actor loop entirely into a dedicated async task per peer.

---

## Issue 2: `PeerActor` wraps `Peer` in `Arc<RwLock<>>` unnecessarily

**Location:** `src/actor/peer_actor.rs`

```rust
peer: Arc<RwLock<Peer>>,
```

This lock is acquired on nearly every method call:

```rust
let username = self.peer.read().unwrap().username.clone();
```

`PeerActor` is single-threaded — it runs in its own tokio task and nothing
else holds a reference to this `Peer` at the same time. The `Arc<RwLock<>>`
provides no benefit and adds unwrap noise throughout.

**Fix:** make it a plain field:

```rust
peer: Peer,
```

All the `.read().unwrap()` and `.write().unwrap()` calls collapse to direct
field access.

---

## Issue 3: `TransferResponse(allowed=false)` silently hangs the download

**Location:** `src/actor/peer_actor.rs:189`

```rust
if !allowed {
    if let Some(reason_text) = reason {
        debug!("[peer:{}] Transfer rejected: {} - waiting for TransferRequest...", ...);
    }
    // falls through — does nothing
}
```

When a peer rejects a transfer request, the actor logs it and waits for a
`TransferRequest` to arrive anyway. If that message never comes:

- No `DownloadCompleted(Err)` is sent to the worker
- The `active_downloads` slot is never freed
- The download hangs indefinitely with status `Queued`

The Soulseek protocol does sometimes follow a rejection with a `TransferRequest`
(the peer is saying "not yet, I'll tell you when"), but there is no timeout
enforcing that this actually happens.

**Fix:** on `allowed=false`, start a timeout. If no `TransferRequest` arrives
within a configurable window (e.g. 60 s), send:

```rust
client_channel.send(ClientOperation::DownloadCompleted(
    token,
    Err(SoulseekRs::Timeout),
))
```

This frees the slot and surfaces the failure to the caller instead of hanging.
The `PendingConnection` map from Plan 2 Issue 5 can track this timeout.

---

## Issue 4: `PeerMessage` mixes external commands with internal wire signals

**Location:** `src/actor/peer_actor.rs`

Like `ServerMessage`, `PeerMessage` conflates things that flow in opposite
directions:

- **External commands** (worker → actor): `QueueUpload`, `RequestTransfer`,
  `SendMessage`, `SetUsername`
- **Internal wire signals** (dispatcher handler → actor): `TransferRequest`,
  `TransferResponse`, `FileSearchResult`, `PlaceInQueueResponse`, `UploadFailed`
- **Lifecycle signals** (internal only): `ProcessRead`, `ConnectionEstablished`,
  `ConnectionFailed`

`ActorHandle<PeerMessage>` is stored in the `PeerRegistry` and used by the
worker. The worker could accidentally send a wire signal (`TransferRequest`) to
a peer actor — the type system does not prevent it.

**Fix:** same split as proposed for `ServerMessage` in Plan 2 Issue 7:

```rust
// Public — what the worker sends to the actor
pub enum PeerCommand {
    QueueUpload(SoulseekPath),
    RequestTransfer(Download),
    SendMessage(Message),
    SetUsername(String),
}

// Private — wire handler results and lifecycle signals
enum PeerSignal {
    TransferRequest(Transfer),
    TransferResponse { token: DownloadToken, allowed: bool, reason: Option<String> },
    FileSearchResult(SearchResult),
    PlaceInQueueResponse { filename: SoulseekPath, place: u32 },
    UploadFailed(String, SoulseekPath),
    ProcessRead,
    ConnectionEstablished(TcpStream),
    ConnectionFailed(io::Error),
}
```

`PeerRegistry` stores `ActorHandle<PeerCommand>`. Sending an internal signal
from outside becomes a compile error.

---

## Issue 5: `disconnect` and `disconnect_with_error` are nearly identical

**Location:** `src/actor/peer_actor.rs:382` and `:395`

```rust
fn disconnect_with_error(&mut self, error: io::Error) {
    // takes stream, sends PeerDisconnected(username, Some(error.into()))
}

fn disconnect(&mut self) {
    // takes stream, sends PeerDisconnected(username, None)
}
```

The two methods are identical except for the `Option<SoulseekRs>` payload.
`disconnect()` is only called from `on_stop()`.

**Fix:** collapse into one method:

```rust
fn disconnect(&mut self, error: Option<io::Error>) {
    self.stream.take();
    let _ = self.client_channel.send(ClientOperation::PeerDisconnected(
        self.peer.username.clone(),
        error.map(Into::into),
    ));
}
```

---

## Issue 6: No timeout on `read_peer_init_message` in the listener

**Location:** `src/peer/listen.rs:31`

```rust
async fn read_peer_init_message(stream: &mut TcpStream, reader: &mut MessageReader) -> io::Result<Message> {
    let mut temp_buffer = [0u8; 1024];
    loop {
        let n = stream.read(&mut temp_buffer).await?;
        ...
    }
}
```

There is no timeout. A peer that connects and then sends nothing will hold the
spawned handler task open indefinitely. Under a malicious peer or a buggy
client this leaks tasks. Since every accepted connection spawns a task that
calls this function, the leak can accumulate silently.

**Fix:**

```rust
tokio::time::timeout(
    Duration::from_secs(10),
    read_peer_init_message(&mut stream, &mut reader),
)
.await
.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer init timeout"))??;
```

---

## Issue 7: Post-login share counts are hardcoded, not from `ClientSettings`

**Location:** `src/actor/server_actor.rs:362`

```rust
self.queue_message(MessageFactory::build_shared_folders_message(1, 499));
```

After a successful login the actor tells the server we are sharing 1 folder and
499 files. These values are hardcoded and have no relation to reality. The
Soulseek server uses shared file counts to rank peers in search results — a
peer sharing more files is ranked higher and gets more download slots.

**Fix:** add `shared_folders: u32` and `shared_files: u32` to `ClientSettings`
with reasonable defaults, and pass them through to `ServerActor::new()`:

```rust
// ClientSettings
pub shared_folders: u32,  // default: 1
pub shared_files:   u32,  // default: 0

// ServerActor post-login:
self.queue_message(MessageFactory::build_shared_folders_message(
    self.shared_folders,
    self.shared_files,
));
```

---

## Suggested order of implementation

| Step | Change | Value | Effort |
|---|---|---|---|
| 1 | `PeerActor`: remove `Arc<RwLock<Peer>>`, use plain field | Removes noise, no functional change | Small |
| 2 | Collapse `disconnect` / `disconnect_with_error` | Removes duplication | Small |
| 3 | Add timeout to `read_peer_init_message` | Prevents task leak | Small |
| 4 | Expose `shared_folders` / `shared_files` in `ClientSettings` | Correctness, better search ranking | Small |
| 5 | `TransferResponse(allowed=false)` timeout | Fixes silent hang | Medium |
| 6 | Split `PeerMessage` into command / signal | Compile-time direction enforcement | Medium |
| 7 | Async reads/writes in `PeerActor` | Fixes latency + message loss (all N actors) | Large |
