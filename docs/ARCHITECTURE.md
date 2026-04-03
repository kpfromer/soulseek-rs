# Architecture

This document describes the internal structure of `soulseek-rs-lib`: how the
components fit together, how messages flow, and why key design decisions were
made. It also documents known architectural issues and their severity.

---

## High-level overview

```
User code
    │
    ▼
Client  (public API façade)
    │
    ├─── ServerActor  (server TCP + reconnect)
    │         │
    │         └─── [emits ClientOperation]
    │
    ├─── ConnectedWorker  (state + routing loop)
    │         │
    │         ├─── PeerRegistry  (peer actor directory)
    │         │         └─── PeerActor ×N  (per-peer protocol)
    │         │
    │         └─── DownloadPeer  (actual file I/O task)
    │
    └─── Listen  (inbound peer accept loop)
```

There are two independent layers of shared state:

- **`ClientInner`** (behind `Arc<Mutex<>>`) — the public-facing layer. Holds
  connection state, the search rate limiter, and downloads queued before
  `connect()` is called. Accessed only by `Client` methods and `state_monitor`.

- **`ConnectedWorker`** — the live-connection layer. Owns `downloads` and
  `searches` maps directly (no lock). Accessed only from the worker's own
  async loop; all external reads go through oneshot query operations.

---

## Component ownership and responsibilities

| Component | Owns | Responsible For |
|---|---|---|
| `Client` | `Arc<Mutex<ClientInner>>` | Public API, connect orchestration, query dispatch |
| `ClientInner` | State machine, active connection handles, pending queue | Connection state transitions |
| `ConnectedWorker` | `downloads`, `searches`, `pending` queue, `active_downloads` | Operations routing, download concurrency, snapshot queries |
| `ServerActor` | TCP stream, login state, reconnect state/credentials | Server protocol, auto-reconnect |
| `PeerActor` (×N) | One peer's TCP stream, queued messages | Per-peer protocol, forward events to worker |
| `PeerRegistry` | `HashMap<username, ActorHandle>` | Spawn, lookup, remove peer actors |
| `Listen` | `TcpListener` | Accept inbound peer connections (PeerInit + PierceFirewall) |
| `DownloadPeer` | Download execution | TCP connect to peer, file I/O, status reporting |
| `DownloadHandle` | Status receiver channel, cancel flag | User-facing progress and cancellation |
| `ActorSystem` | CancellationTokens for all actors | Actor lifecycle, graceful shutdown |

---

## The actor system

`src/actor/mod.rs` provides a tiny, self-contained actor framework:

- **`Actor` trait** — implement `handle()`, and optionally `on_start()`,
  `on_stop()`, `tick()`.
- **`ActorSystem`** — wraps a `CancellationToken`; `shutdown()` cancels all
  child tokens at once.
- **`ActorHandle<M>`** — a cloneable sender; `send()` enqueues a message,
  `stop()` sends a stop signal.

Each actor runs in its own `tokio::spawn` task. The event loop calls `tick()`
every 100 ms between messages, used by `ServerActor` for reconnect backoff and
login timeout checks.

---

## ServerActor

`src/actor/server_actor.rs` manages the TCP connection to the Soulseek server.

### State machine

Two orthogonal enums express the actor's state without any `Option` fields that
could drift out of sync:

```
ServerConnection
  Disconnected { reconnect_attempt, last_disconnect }
  Connecting   { stream, since, reconnect_attempt }
  Connected    { stream, dispatcher }

LoginState
  NotAttempted
  Pending  { credentials, response: oneshot::Sender, deadline }
  LoggedIn { credentials }
```

`stream` and connection state are the same variant — it is impossible to be
`Connected` without a live `TcpStream`. The dispatcher (message router) lives
inside `Connected`, so any dispatcher access is only valid in the connected
state.

### Lifecycle

```
on_start()
  └─ initiate_connection()     (std::net blocking connect → Connecting)

tick() [every 100 ms]
  ├─ Connecting   → check_connection_status() → on_connection_established()
  ├─ Connected    → process_read()  (non-blocking try_read)
  └─ Disconnected → maybe_reconnect()  (exponential backoff)

on_connection_established()
  ├─ build Dispatcher + register all server message handlers
  ├─ send SetServerSender to ConnectedWorker (so it can reach us)
  ├─ if LoggedIn: auto-queue login (reconnect path)
  └─ replay queued_messages accumulated while disconnected
```

### Message handling

`ServerMessage` variants split into two groups:

- **External commands** sent by `Client` / `ConnectedWorker` (e.g. `Login`,
  `FileSearch`, `GetPeerAddress`, `PierceFirewall`) — result in wire messages
  sent to the server or state transitions.
- **Internal signals** dispatched by message handlers back into the actor (e.g.
  `LoginStatus`, `ConnectToPeer`, `GetPeerAddressResponse`) — forwarded to
  `ConnectedWorker` as `ClientOperation` messages.

Messages that arrive while disconnected are pushed onto `queued_messages` and
replayed once `on_connection_established()` fires.

### Reconnection

`maybe_reconnect()` runs on every tick while `Disconnected`. It only reconnects
if `login_state` is `LoggedIn` (i.e. we have credentials). Backoff:

```
delay = min(min_delay × 2^(attempt−1), max_delay)
```

On `LoginStatus(true)`, `reconnect_attempt` resets automatically because
transitioning from `Disconnected → Connected` discards the old state variant.

---

## ConnectedWorker

`src/client/connected_worker.rs` is the central event loop while connected. It
receives every `ClientOperation` and routes it. It is the **single source of
truth** for downloads and searches — no other component writes those maps.

### Key responsibilities

| Operation | Action |
|---|---|
| `LoginSucceeded` | Set `logged_in = true`, drain pending download queue |
| `ServerDisconnected` | Set `logged_in = false`, forward to `state_monitor` |
| `RequestDownload` | If slot available and logged in: initiate; else queue |
| `DownloadCompleted` | Update status, notify caller, free slot, dequeue next |
| `DownloadFromPeer` | Spawn blocking task → `DownloadPeer::download_direct` |
| `ConnectToPeer` / `NewPeer` | Spawn `connect_to_peer()` → register `PeerActor` |
| `PierceFireWall` | Send PierceFirewall to server, then `connect_to_peer(F)` |
| `SearchResult` | Append to matching `Search` entry |
| `UpdateDownloadTokens` | Replace token on an in-flight download |
| `UploadFailed` / `PeerDisconnected` | Mark downloads failed, decrement counter |
| `SetServerSender` | Store sender to `ServerActor` dispatcher |
| `Query*` | Snapshot state and send via oneshot channel (all queries are async) |

### Download concurrency

```
active_downloads: u32       ← incremented on initiate, decremented on complete
max_concurrent: Option<u32> ← from ClientSettings
pending: VecDeque<PendingDownload>
```

When `RequestDownload` arrives:
- logged-in **and** slot available → `try_initiate()` (calls
  `peer_registry.queue_upload`, increments counter)
- otherwise → push to `pending`

`drain_pending_queue()` runs on `LoginSucceeded`; `try_dequeue_next()` runs on
every `DownloadCompleted`.

### Connection types in `connect_to_peer`

| `ConnectionType` | Meaning | Action |
|---|---|---|
| `P` | Normal peer connection | Register `PeerActor` via `peer_registry` |
| `F` | Pierce-firewall (reversed TCP) | Spawn blocking `download_pierced` |
| `D` | Distributed (not implemented) | Error log |

---

## PeerActor

`src/actor/peer_actor.rs` manages a single peer connection. Each peer runs in
its own actor task managed by `ActorSystem`.

After registering, the actor sends:
- `PeerMessage::PeerInit` (handshake identifying our username and connection type)
- `PeerMessage::QueueUpload` (when `queue_upload()` is called)

Inbound messages dispatched:
- `TransferRequest` → sends `UpdateDownloadTokens` + `DownloadFromPeer` to worker
- `PlaceInQueueResponse` → logged
- `UploadFailed` → sends `UploadFailed` to worker
- `FileSearchResponse` → sends `SearchResult` to worker

---

## Message system

`src/message/mod.rs` provides `Message`: a buffer with a cursor for sequential
reads (little-endian). All Soulseek wire types are 4-byte-length-prefixed.

`MessageDispatcher<M>` (in `src/dispatcher.rs`) maps message codes to
`MessageHandler` implementations. Handlers read fields from a `Message` and
send the result back through an `UnboundedSender<M>`. Unknown codes produce a
warning, never a panic.

Server handlers live in `src/message/server/`; peer handlers in
`src/message/peer/`.

---

## Key architectural patterns

| Pattern | Where |
|---|---|
| One actor = one task | `ActorSystem::spawn()` — actors never share threads |
| Oneshot for queries | All `Query*` ops — no locks on downloads/searches maps |
| `ClientOperation` enum | The single pipe: all actors → worker communication |
| `AtomicBool` cancel | Shared between `DownloadHandle` (user) and `DownloadPeer` (task) |
| Credentials replay | `ServerActor` auto-reconnects + re-logins transparently |
| Pending queue | Downloads before login are buffered, drained on `LoginSucceeded` |

---

## Data flow

### Search

```
Client::search(query)
  └─ sends InitiateSearch(query, token) → worker stores Search entry
  └─ sends FileSearch { token, query } → ServerActor → wire
       Peers respond → SearchResult op → worker appends to search.results
  └─ polls QuerySearchResults(query) via oneshot → returns snapshot
```

### Download

```
Client::download(filename, username, ...)
  └─ creates PendingDownload, sends RequestDownload
       worker: insert to downloads map immediately (visible to queries)
       worker: try_initiate → peer_registry.queue_upload → PeerActor
         PeerActor sends QueueUpload wire message
         Peer responds: TransferRequest (new token + size)
         PeerActor sends UpdateDownloadTokens + DownloadFromPeer
       worker: spawn_blocking → DownloadPeer
         DownloadPeer: TCP connect, START_DOWNLOAD, read chunks, write file
         sends DownloadStatus updates via download.sender
       DownloadPeer sends DownloadCompleted(token, Ok(path))
       worker: update status, decrement counter, try_dequeue_next
```

### Firewall pierce

```
Peer connects to our listener (Listen task)
  └─ reads PierceFirewall message (code 0), extracts token
  └─ QueryDownloadByToken oneshot → worker resolves Download
  └─ registers PeerActor for accepted connection
       PeerActor handles rest of download normally
```

---

## Shutdown

`Client::shutdown()` (also called on `Drop`) calls `ActorSystem::shutdown()`,
which cancels the root `CancellationToken`. All child tokens (each actor's
`select!` branch) see the cancellation and break their loops.

`ConnectedWorker` has its own copy of the same token and breaks its `select!`
loop on cancellation. The listener task also selects on the token and exits
cleanly.

---

## Known issues

These are architectural problems identified in the current implementation,
ordered roughly by severity.

### 1. `initiate_connection()` blocks the tokio thread (high)

**Location:** `src/actor/server_actor.rs` — `initiate_connection()`

```rust
match std::net::TcpStream::connect(addr) {  // blocking
```

`std::net::TcpStream::connect` is a blocking call inside an async runtime task.
It holds a tokio worker thread for the full TCP connection duration (up to the
OS timeout, typically 20–120 s on failure). Should use
`tokio::net::TcpStream::connect().await` instead.

---

### 2. `try_write` silently drops messages under back-pressure (high)

**Location:** `src/actor/server_actor.rs` — `send_message()`

```rust
Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
    // TODO: buffer for later write
    warn!("[server] Write would block, message may be lost");
}
```

When the socket send buffer is full, `try_write` returns `WouldBlock` and the
message is silently discarded. There is no write buffer or retry. Under load
(e.g. sending many search requests) this is a correctness issue.

---

### 3. Tick-driven polling instead of async reads (medium)

**Location:** `src/actor/server_actor.rs` — `tick()` → `process_read()`

`process_read()` uses `stream.try_read()` (non-blocking poll) driven by the
100 ms actor tick, rather than `stream.readable().await`. This means:

- Up to **100 ms latency** on every inbound server message
- CPU wakes up every 100 ms even when there is nothing to read

This is a structural consequence of the `Actor::tick()` being a synchronous
method. A proper fix would require the actor loop to select on the stream
directly, which the current trait does not support.

---

### 4. `downloads` map cloned per peer connection (medium)

**Location:** `src/client/connected_worker.rs` — `ConnectToPeer` and
`GetPeerAddressResponse` handlers

```rust
let downloads = self.downloads.clone();
tokio::spawn(async move {
    Self::connect_to_peer(peer, context, own_username, None, op_tx, downloads);
});
```

The entire downloads map is cloned for each peer connection task. The snapshot
becomes **immediately stale** — a download cancelled after the clone will not
be seen by the spawned task's `resolve` closure. For the `F`-type path, this
means a cancelled download could still attempt to transfer bytes.

---

### 5. `searches` keyed by query string, found by token — O(n) scan (medium)

**Location:** `src/client/connected_worker.rs` — `SearchResult` handler

```rust
for search in self.searches.values_mut() {
    if search.token == result_token {  // linear scan on every result
```

Every incoming `SearchResult` (which can arrive at high frequency) does a
linear scan over all searches. The map should be keyed by `SearchToken` instead
of query string.

Additionally, `InitiateSearch` inserts by query string, so two searches for the
same query clobber each other:

```rust
self.searches.insert(key, Search { token, results: vec![] });
```

---

### 6. `active_downloads` counter manually bookkept across multiple paths (medium)

The counter is decremented in three separate places:

- `DownloadCompleted` handler
- `PierceFirewallPreTokenFailed` handler
- `process_failed_uploads()`

And incremented only in `try_initiate()`. Any new failure path that forgets to
call one of these will silently corrupt the counter (this already caused a bug
once). A RAII guard that decrements on drop would be safer.

---

### 7. `SetServerSender` timing hazard (low–medium)

**Location:** `src/actor/server_actor.rs` — `on_connection_established()`

When a connection is established, `ServerActor` sends `SetServerSender` to the
worker via an async channel. If `PierceFireWall` operations arrive at the worker
before `SetServerSender` is processed, `self.server_sender` is `None` and the
pierce message is silently dropped:

```rust
} else {
    error!("No server sender available for PierceFirewall");
}
```

This window exists on every reconnect.

---

### 8. `searches` map never cleaned up (low)

`ConnectedWorker::searches` grows indefinitely. Once a search times out on the
client side there is no corresponding `RemoveSearch` operation, so the entry and
all its results remain in memory forever.

---

### 9. `DownloadHandle` Drop auto-cancel is a silent footgun (low)

The `Drop` impl on `DownloadHandle` cancels the download when the handle is
dropped. This is the right behaviour for scope-based cancellation, but it is
easy to accidentally cancel a download:

```rust
let (download, _) = client.download(...).await?;
//                           ^ handle dropped here — download silently cancelled
```

There is no warning at compile time or runtime. The handle must be stored
explicitly.

---

### 10. `state_monitor` adds indirection for little value (low)

`src/client/state_monitor.rs` is a dedicated task whose only job is to relay
two `WorkerEvent` variants (`ServerDisconnected`, `LoginSucceeded`) to
`ClientInner.state`. The extra task, channel, and `WorkerEvent` enum exist
solely to avoid giving the worker a reference to `ClientInner`. This could be
simplified by having the worker hold an `Arc<Mutex<ClientState>>` directly.

---

### Summary table

| Issue | Severity |
|---|---|
| Blocking TCP connect in async context | High |
| `try_write` silently drops messages | High |
| Tick-driven reads (100 ms latency) | Medium |
| `downloads` cloned per connection (stale snapshot) | Medium |
| `searches` keyed by string, scanned by token | Medium |
| `active_downloads` manual bookkeeping | Medium |
| `SetServerSender` timing window on reconnect | Low–Medium |
| `searches` map never cleaned up | Low |
| `DownloadHandle` drop footgun | Low |
| `state_monitor` unnecessary indirection | Low |
