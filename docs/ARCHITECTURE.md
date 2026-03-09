# Architecture

This document describes the internal structure of `soulseek-rs-lib`: how the
components fit together, how messages flow, and why key design decisions were
made.

---

## High-level overview

```
┌─────────────────────────────────────────────────────┐
│  Client  (public API)                                │
│  connect() · search() · download() · shutdown()      │
└────────────┬────────────────────────────────────────┘
             │ holds Arc<Mutex<ClientInner>>
             │
     ┌───────▼────────┐          ┌────────────────────┐
     │  ClientInner   │          │   ActorSystem       │
     │  state         │──owns───▶│   (CancellationToken│
     │  active        │          │    + task lifecycle)│
     └───────┬────────┘          └────────┬───────────┘
             │                            │ spawns
             │ Arc<RwLock<ClientContext>> │
             │                   ┌────────▼────────────┐
             │                   │   ServerActor        │
             │                   │   (TCP ↔ server)     │
             │                   └────────┬────────────┘
             │                            │ ClientOperation channel
             │                   ┌────────▼────────────┐
             └──────────────────▶│  ConnectedWorker     │
                                 │  (operation loop)    │
                                 └────────┬────────────┘
                                          │ spawns PeerActors
                                          ▼
                                  ┌──────────────────┐
                                  │  PeerActor (×N)  │
                                  │  (one per peer)  │
                                  └──────────────────┘
```

There are two independent "layers" of shared state:

- **`ClientInner`** (behind `Arc<Mutex<>>`) — the public-facing layer. Holds
  connection state, the search rate limiter, and any downloads queued before
  `connect()` is called. Accessed by `Client` methods.

- **`ClientContext`** (behind `Arc<RwLock<>>`) — the live-connection layer.
  Holds the in-flight search and download maps plus the `PeerRegistry`. Owned
  by `ConnectedWorker` and accessed from spawned peer tasks.

The two layers are deliberately separate. `ClientInner` is locked briefly from
async call sites; `ClientContext` is locked from blocking I/O tasks and must not
be held for long.

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
  └─ initiate_connection()     (std::net blocking connect, then into Connecting)

tick() [every 100 ms]
  ├─ Connecting → check_connection_status() → on_connection_established()
  ├─ Connected  → process_read()            (non-blocking try_read)
  └─ Disconnected → maybe_reconnect()       (exponential backoff)

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
receives every `ClientOperation` and routes it.

### Key responsibilities

| Operation | Action |
|---|---|
| `LoginSucceeded` | Set `logged_in = true`, drain pending download queue |
| `ServerDisconnected` | Set `logged_in = false`, forward to `state_monitor` |
| `RequestDownload` | If slot available and logged in: initiate; else queue |
| `DownloadCompleted` | Update context, notify caller, free slot, dequeue next |
| `DownloadFromPeer` | Spawn blocking task → `DownloadPeer::download_direct` |
| `ConnectToPeer` / `NewPeer` | Spawn `connect_to_peer()` → register `PeerActor` |
| `PierceFireWall` | Send PierceFirewall to server, then `connect_to_peer(F)` |
| `SearchResult` | Append to matching `Search` in context |
| `UpdateDownloadTokens` | Replace token on an in-flight download |
| `UploadFailed` / `PeerDisconnected` | Mark downloads failed, remove from context |
| `SetServerSender` | Store sender to `ServerActor` dispatcher |

### Download concurrency

```
active_downloads: u32       ← incremented on initiate, decremented on complete
max_concurrent: Option<u32> ← from ClientSettings
pending: VecDeque<PendingDownload>
```

When `RequestDownload` arrives:
- logged-in **and** slot available → `try_initiate()` (calls `peer_registry.queue_upload`, increments counter)
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

## Download flow (end to end)

```
1.  client.download(filename, username, size, dir)
    └─ creates PendingDownload, sends ClientOperation::RequestDownload

2.  ConnectedWorker::RequestDownload
    └─ try_initiate → ctx.add_download + peer_registry.queue_upload(username, filename)

3.  PeerActor sends PeerMessage::QueueUpload to peer

4.  Peer responds with TransferRequest (new token + size)
    └─ PeerActor sends UpdateDownloadTokens (remaps token) + DownloadFromPeer

5.  ConnectedWorker::DownloadFromPeer
    └─ spawn_blocking → DownloadPeer::download_direct(download, None)
         ├─ establish_connection (TCP to peer)
         ├─ write START_DOWNLOAD (8 zero bytes)
         └─ read_stream (writes file, sends InProgress via download.sender)

6.  spawn_blocking finishes → sends ClientOperation::DownloadCompleted(token, Ok(path))

7.  ConnectedWorker::DownloadCompleted
    ├─ download.sender.send(Completed/Failed)
    ├─ ctx.update_download_with_status(token, status)
    ├─ active_downloads -= 1
    └─ try_dequeue_next()
```

The pierce-firewall path (ConnectionType::F) differs at step 5: the peer
initiates the TCP connection to us. `download_pierced` reads a 4-byte token
from the first bytes, calls the `resolve_download` closure to find the
`Download`, then proceeds identically.

---

## Shutdown

`Client::shutdown()` (also called on `Drop`) calls `ActorSystem::shutdown()`,
which cancels the root `CancellationToken`. All child tokens (each actor's
`select!` branch) see the cancellation and break their loops.

`ConnectedWorker` has its own copy of the same token and breaks its `select!`
loop on cancellation.

The listener task also selects on the token and exits cleanly.
