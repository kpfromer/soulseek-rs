# Refactoring Plan

Goals: eliminate implicit state machines, reduce shared mutable state, prefer message passing.

---

## Item 1: Download queuing — two queues, split across client and state_monitor

### What it is now

Two separate `VecDeque<PendingDownload>` queues exist for the same conceptual purpose ("defer this download"):

- `ClientInner.pending_downloads` — holds downloads requested while disconnected. Drained by `state_monitor` on `LoginSucceeded`.
- `DownloadConcurrencyLimiter.queue` (inside `ClientInner.download_limiter`) — holds downloads that arrived while connected but the concurrency cap was full. Drained by `state_monitor` on `DownloadSlotFreed`.

The two queues can cascade: on `LoginSucceeded`, queue 1 is drained and any overflow (when concurrency cap is already full) spills into queue 2. Neither queue knows about the other.

`Client::download()` contains the routing decision: which queue to write to (or call `queue_upload` directly) depending on connection state and limiter capacity. This means `Client::download()` directly reads and mutates `ClientInner.download_limiter` and `ClientContext.peer_registry` — shared state that the actor side also owns.

The concurrency-check logic (`lim.active < lim.max_concurrent`) is duplicated in both `Client::download()` and `state_monitor`'s `LoginSucceeded` handler.

### What we want

One queue. `Client::download()` does only two things: create the `Download` and send a `RequestDownload` message to the actor system. It never touches the limiter or registry directly.

The `ConnectedWorker` (or a dedicated queue owner) receives `RequestDownload` and makes the routing decision:
- If connected and slot available → `queue_upload` immediately
- If connected and at cap → push to the single concurrency queue
- If disconnected → push to the same concurrency queue (replayed on `LoginSucceeded`)

On `LoginSucceeded` and `DownloadSlotFreed`, the same dequeue logic runs against the same queue. No cascade, no duplication.

### Why

- Single responsibility: `Client::download()` should not need to know about connection state or concurrency limits.
- One queue means one place to reason about ordering and fairness.
- Eliminates the duplicated concurrency-check logic.
- Reduces shared state access from the client side — limiter and registry become internal to the worker.

---

## Item 2: DownloadFromPeer — blocking task as mini-coordinator

### What it is now

When a download completes, the `spawn_blocking` closure does three things inline:
1. `download.sender.send(Completed/Failed)` — notifies the external caller
2. `context.write().update_download_with_status(...)` — updates internal context
3. `context.read().sender.send(ClientOperation::DownloadSlotFreed)` — unblocks the concurrency limiter

Step 3 is especially problematic: to signal back to the worker, the blocking task acquires a read lock on `ClientContext` and looks for `ctx.sender: Option<UnboundedSender<ClientOperation>>` — a channel back to the worker stored *inside* shared state. If it is `None`, the concurrency limiter stalls silently and no further downloads can proceed.

This also means `ClientContext` has a mixed responsibility: it holds domain state (downloads, searches, registry) *and* a communication channel back to the worker. These are unrelated concerns.

The same `spawn_blocking` + download + completion pattern is duplicated in `connect_to_peer`'s `ConnectionType::F` branch (lines 360–391), but the error path there does not call `update_download_with_status(Failed)` — the two paths have already diverged.

### What we want

The blocking task does one thing: run the download, then send a single `ClientOperation::DownloadCompleted(token, result)` message back to the worker via a directly-captured `op_tx` clone.

The worker's `DownloadCompleted` handler performs all three completion steps in one place, in async code, with a clear sequence. The blocking task needs no reference to `context` after the download finishes.

`ctx.sender` is removed from `ClientContext` entirely — the field only existed so blocking tasks could reach the worker indirectly through shared state.

The `DownloadFromPeer` and `ConnectionType::F` paths are unified into a single private method that spawns the blocking task and always reports the result via `DownloadCompleted`.

### Why

- The blocking task should only do I/O, not coordinate state.
- Removes the silent-stall risk: slot is always freed because the worker always handles `DownloadCompleted`.
- `ClientContext` becomes purely state — no embedded channels.
- Eliminates the divergence between `DownloadFromPeer` and `ConnectionType::F`.

---

## Item 3: ClientContext — mixed concerns, optional fields, embedded channels

### What it is now

```rust
pub struct ClientContext {
    pub peer_registry: Option<PeerRegistry>,
    pub sender: Option<UnboundedSender<ClientOperation>>,
    pub server_sender: Option<UnboundedSender<ServerMessage>>,
    pub searches: HashMap<String, Search>,
    pub downloads: Vec<Download>,
    pub actor_system: Arc<ActorSystem>,
}
```

Three problems:

**1. Optional fields that aren't really optional.** `peer_registry`, `sender`, and `server_sender` are all `Option` not because they're ever absent at runtime during a connection, but because the struct is constructed before the connection is fully established. Every reader has to defensively `if let Some(...)` before using them. Nothing prevents connected code from hitting the `None` branch silently.

**2. Channels embedded in shared state.** `sender` (back to the worker) and `server_sender` (to the server actor) are only ever accessed from `connected_worker.rs`. They ended up in `ClientContext` as a workaround: `connect_to_peer` and `pierce_firewall` are static methods that only receive `context`, so storing channels there was the only way for them to communicate. The channels don't belong to the domain — they're operational plumbing.

**3. `downloads` is a flat `Vec` with linear scans.** `get_download_by_token` iterates the whole list. Should be `HashMap<u32, Download>`.

### What we want

Split into a state enum:

```rust
struct ConnectedState {
    peer_registry: PeerRegistry,
    searches: HashMap<String, Search>,
    downloads: HashMap<u32, Download>,
    actor_system: Arc<ActorSystem>,
}

enum ClientContext {
    Disconnected,
    Connected(ConnectedState),
}
```

Channels move out of `ClientContext` and onto `ConnectedWorker` as direct fields:

```rust
pub struct ConnectedWorker {
    pub op_tx: UnboundedSender<ClientOperation>,   // for spawned closures to capture
    pub server_sender: UnboundedSender<ServerMessage>,
    // ...existing fields...
}
```

`connect_to_peer` and `pierce_firewall` become instance methods using `self.server_sender` directly. Spawned closures capture `op_tx` / `server_sender` by clone — no lock required to reach a channel.

### Why

- The compiler now enforces that `peer_registry` and friends are only accessed when connected. Every silent `None` branch becomes a type error.
- `ClientContext` becomes purely domain state — no communication concerns.
- Removes the need for `SetServerSender` as a `ClientOperation` variant (it only existed to inject the channel into context).
- Linear scan on downloads becomes O(1) lookup.

---

## Item 4: DownloadPeer — two protocols merged into one, coupled to ClientContext

### What it is now

`read_download_stream` handles two fundamentally different TCP protocols in one loop, controlled by `no_pierce`:

- **`no_pierce = true`** (direct): download is known upfront, file opened before loop, `START_DOWNLOAD` sent immediately.
- **`no_pierce = false`** (pierce): first chunk is a 4-byte token identifying the download, file opened lazily after token resolved, remaining bytes in that first chunk are the start of file data.

Both paths share one loop with `!self.no_pierce && !received_handshake` branching the first iteration. `writer` and `download` are both `Option` throughout, with `writer.as_mut().unwrap()` at line 261 that would panic if the implicit invariant broke. After the handshake `download` is always `Some`, but every subsequent iteration still defensively pattern-matches it because the compiler only sees `Option`.

`read_download_stream` and `handle_pierce_firewall_response` both take `&Arc<RwLock<ClientContext>>`, locking shared state from inside a blocking I/O loop, for two reasons:
1. Pierce path: looking up `Download` by token (line 185)
2. Progress updates: calling `update_download_with_status` on every 15th chunk (line 295-298)

### What we want

Split into two explicit methods:
- `read_stream_direct(download: Download, stream, ...)` — straight-line, no branching
- `read_stream_pierced(stream, resolve_download: impl Fn(Token) -> Option<Download>, ...)` — handles first-chunk token, then straight-line

The `resolve_download` closure lets the caller decide how to look up a download by token, without `DownloadPeer` knowing about `ClientContext`:

```rust
// at the call site in the worker:
download_peer.download_file(
    stream,
    |token| context.read().unwrap().get_download_by_token(token).cloned(),
)

// inside read_stream_pierced:
let download = resolve_download(token).ok_or(DownloadError::TokenNotFound(token))?;
// proceed with straight-line file writing
```

Progress updates are sent through `download.sender` (already a `UnboundedSender<DownloadStatus>`) directly — no context lock needed. The worker receives progress via the same channel as completion.

`ClientContext` is removed from `DownloadPeer` entirely.

### Why

- Two protocols should be two methods — no shared mutable state flags, no `unwrap()` on implicit invariants.
- `DownloadPeer` becomes pure I/O: reads bytes, writes a file, sends progress. No knowledge of `ClientContext`.
- Removes a synchronous lock acquisition (`context.write()`) inside a hot I/O loop.
- Closure approach keeps the token→download lookup testable and swappable without mocking the entire context.

---

## Item 5: Token — raw u32 used for multiple purposes

### What it is now

`u32` is used as a token type throughout the codebase for at least two distinct purposes:
- Download tokens (identifying a download request)
- Pierce firewall tokens (identifying a connection attempt)

Both are raw `u32`, making it easy to accidentally pass one where the other is expected, and making function signatures ambiguous (`fn foo(token: u32)` — which kind?).

### What we want

Newtype wrappers:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownloadToken(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PierceToken(u32);
```

The compiler then rejects mixing them. `HashMap<u32, Download>` becomes `HashMap<DownloadToken, Download>`. Function signatures become self-documenting.

### Why

- Prevents a class of bugs at compile time with zero runtime cost.
- Makes function signatures self-documenting — no need to check docs or callers to know which token kind is expected.
- Consistent with the `SoulseekPath` newtype already in the codebase.

---

## Item 6: ServerActor — three interlocked state concerns on one struct

### What it is now

Three distinct state concerns are spread across individual fields on `ServerActor`:

```rust
// TCP connection
connection_state: ConnectionState,     // Disconnected / Connecting / Connected
stream: Option<TcpStream>,             // should always match connection_state

// Login
credentials: Option<(String, String)>,
pending_login: Option<oneshot::Sender<Result<bool, SoulseekRs>>>,
login_deadline: Option<Instant>,

// Reconnect
reconnect_attempt: u32,
last_disconnect: Option<Instant>,

// Dispatcher (always initialized together, but three separate Options)
dispatcher: Option<MessageDispatcher<ServerMessage>>,
dispatcher_receiver: Option<UnboundedReceiver<ServerMessage>>,
dispatcher_sender: Option<UnboundedSender<ServerMessage>>,
```

Each group has implicit invariants the compiler cannot check:
- `stream` should be `Some` when `Connected` — but `on_connection_established` defensively checks `stream.is_none()` (line 635) revealing the code doesn't trust this
- `login_deadline` should only be `Some` when `pending_login` is `Some` — nothing enforces this
- `last_disconnect` should only be `Some` when `reconnect_attempt > 0` — also unenforced
- `dispatcher`, `dispatcher_receiver`, `dispatcher_sender` are always `None` or `Some` together, but three separate `Option` fields force three separate `if let Some` guards throughout

`stream` and `connection_state` are updated separately and can drift. `check_connection_status` calls `self.stream.as_ref().unwrap()` two lines after checking `stream.is_none()` — the code knows they should agree but can't prove it.

### What we want

Collapse TCP connection + dispatcher into a proper state enum:

```rust
enum ServerConnection {
    Disconnected {
        reconnect_attempt: u32,
        last_disconnect: Option<Instant>,
    },
    Connecting {
        stream: TcpStream,
        since: Instant,
    },
    Connected {
        stream: TcpStream,
        dispatcher: Dispatcher,  // non-optional — only exists when connected
    },
}
```

`stream` and connection state are now the same field — they cannot drift. `dispatcher` is only accessible when `Connected`, eliminating all `if let Some(ref dispatcher)` guards. Reconnect counters live inside `Disconnected` and are automatically gone when transitioning out of it.

Login state as a separate independent enum:

```rust
enum LoginState {
    NotAttempted,
    Pending { response: oneshot::Sender<...>, deadline: Instant },
    LoggedIn { credentials: (String, String) },
}
```

`credentials` and `pending_login` can no longer be inconsistently `Some`/`None` simultaneously. The reconnect path checks `LoginState::LoggedIn` to decide whether to auto-queue login on reconnect.

### Why

- `stream` and `connection_state` become one field — impossible to have `Connected` with no stream.
- `dispatcher` becomes non-optional inside `Connected` — no defensive guards needed.
- Login and reconnect invariants are encoded in types, not comments.
- Reconnect counters are scoped to `Disconnected` — no manual reset needed on successful login.

---

## Item 7: Connect setup — sequential task spawning with implicit ordering

### What it is now

`connect()` spawns five tasks in sequence with shared state partially initialized before some start:

```
1. Create context (empty)
2. Populate context (sender, peer_registry)   ← write lock held
3. Spawn ServerActor                           ← starts running, sends to op_tx immediately
4. Store active connection in inner
5. Send Login message
6. Spawn listener
7. Spawn ConnectedWorker                       ← only NOW starts consuming op_tx messages
8. Spawn state_monitor
9. Await login result
```

Between steps 3 and 7, `ServerActor` is running and sending messages to `op_tx` but `ConnectedWorker` hasn't started consuming them. Messages queue silently in the channel. This works but the ordering dependency is invisible — nothing in the code signals that steps 3–6 must complete before step 7 for the queue to drain in the right order.

`ClientState::Connected` is set in two places on first connect: `state_monitor` sets it on `LoginSucceeded`, and `connect()` sets it again after `login_rx.await` resolves (line 222). On reconnect only `state_monitor` handles it. Same transition, two code paths, one silently fires twice.

`actor_system` is accessed via a context read lock (lines 148–152) purely to clone it out — a value that never changes after construction. This is a symptom of `actor_system` living inside `ClientContext` (item 3).

### What we want

Fully initialize context before spawning anything. Tasks should start with a complete, valid context rather than racing against initialization.

Remove the duplicate `Connected` state transition from `connect()`. The `login_rx` oneshot already tells `connect()` whether login succeeded — it doesn't need to also set `ClientState`. `state_monitor` owns all state transitions; `connect()` just awaits the result and returns.

With item 3 resolved (`actor_system` moved out of `ClientContext`), the extra lock acquisition disappears.

### Why

- Initialization ordering bugs are silent — the channel buffers hide them until load increases or ordering changes.
- Two places setting the same state transition means two places to update when the transition logic changes.
- Context should be fully valid before any task can observe it.

---

## Item 8: Operation dispatch — complexity as symptom

### What it is now

`handle_operation` in `connected_worker.rs` has 11 match arms across ~250 lines. Several arms exist not because of genuine message handling but because of design issues in other items:

- `SetServerSender` — injects a channel into `ClientContext`. Exists solely because of item 3's channel-in-shared-state problem. Goes away entirely.
- `DownloadSlotFreed`, `ServerDisconnected`, `LoginSucceeded` — three arms that only forward to `state_monitor` via `event_tx`. Pure plumbing, not message handling.
- `ConnectToPeer`, `NewPeer`, `GetPeerAddressResponse` — all call `connect_to_peer`, a static method because it has no access to `self`. Become simpler instance methods once item 3 gives `ConnectedWorker` direct field access.
- `DownloadFromPeer` — the blocking task coordinator, simplified by item 2.

`process_failed_uploads` (lines 301–323) is a two-phase mutation: collect tokens under a read lock, then update each under a write lock. This exists because the borrow checker prevents holding a write lock while iterating. With `ClientContext` as a state enum (item 3), this collapses into a single match arm mutation with no two-phase awkwardness.

### What we want

This item has no implementation of its own — it resolves as a consequence of items 2, 3, and 6. After those are implemented:
- Four arms disappear (`SetServerSender`, `DownloadSlotFreed` forwarding changes, static method workarounds removed)
- `DownloadFromPeer` becomes a simple spawn + one message
- `process_failed_uploads` collapses

The match shrinks to genuine message handling only. If it still feels large after the other items land, splitting into focused handler methods is a natural follow-up.

### Why

Documenting this as a symptom rather than a standalone fix prevents treating the large match as the root problem. Extracting methods or splitting the file without fixing the underlying causes would just move the complexity around.

---

