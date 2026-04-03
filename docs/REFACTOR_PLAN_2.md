# Refactor Plan 2: Peer Connections, Traceability, and Bugs

This document covers issues beyond downloads: peer connection lifecycle,
message direction ambiguity, shared ownership of the peer registry, and
several bugs found in the listener path.

---

## Bug 1: Fake peer usernames in the listener cause registry leaks and missed counter decrements

**Location:** `src/peer/listen.rs`

The listener creates peers with fabricated usernames:

```rust
// Pierce-firewall path:
Peer::new(format!("{}:pierce", peer_ip), ...)

// F-type PeerInit path:
Peer::new(format!("{}:direct", peer_username), ...)

// P-type PeerInit path:
Peer::new(format!("{}:direct", init_data.username), ...)
```

**Consequences:**

- **F-type and pierce paths:** the peer is never registered in the registry so
  there is no leak, but the fabricated username means `process_failed_uploads`
  will never find downloads for this peer (downloads are keyed by the real
  username).
- **P-type path:** the peer IS registered under the fabricated key
  `"{username}:direct"`. When it disconnects, `PeerDisconnected` fires with the
  fake username. `process_failed_uploads` looks for downloads where
  `d.username == "{username}:direct"` — finds none. The `active_downloads`
  counter is never decremented. The registry entry is never cleaned up. Under
  heavy use this leaks registry entries and corrupts the concurrency counter.

**Fix:** use the real username as the registry key. For P-type inbound
connections, use `init_data.username` directly (no suffix). For F-type and
pierce paths that bypass `PeerActor` entirely, do not register them at all
(they already aren't).

---

## Bug 2: Listener paths send `DownloadStatus::Completed` twice

**Location:** `src/peer/listen.rs` — pierce-firewall and F-type handlers

```rust
match download_peer.download_direct(download, Some(std_stream)) {
    Ok((dl, filename)) => {
        let _ = dl.sender.send(DownloadStatus::Completed);          // ← first send
        let _ = client_sender.send(ClientOperation::DownloadCompleted(dl.token, Ok(filename)));
        // DownloadCompleted handler also calls download.sender.send(Completed) ← second send
    }
```

The `DownloadCompleted` handler in the worker sends the status to the user's
`DownloadHandle`. The listener paths also send it directly before emitting
the op. The user's channel receives `Completed` twice.

The worker `DownloadFromPeer` path does NOT have this bug — it only sends via
the handler.

**Fix:** remove the direct `dl.sender.send(...)` calls in the listener. Let
the `DownloadCompleted` handler be the single place that sends status and
decrements the counter (see Issue 3 below for the unified path).

---

## Issue 3: Listen and outbound paths have inconsistent failure models

**Current state:**

| Path | Success | Failure |
|---|---|---|
| Worker `DownloadFromPeer` | `DownloadCompleted(Ok)` | `DownloadCompleted(Err)` |
| Listener pierce-firewall | direct send + `DownloadCompleted(Ok)` | `DownloadCompleted(Err)` |
| Listener F-type (PeerInit) | direct send + `DownloadCompleted(Ok)` | `DownloadCompleted(Err)` |

Three different call sites construct `DownloadPeer`, call `download_direct`,
and send results back — each with slightly different logic. Any future change
to the completion path has to be applied in all three places.

**Fix: single `run_download` helper**

```rust
fn run_download(
    download: Download,
    peer:     DownloadPeer,
    stream:   Option<std::net::TcpStream>,
    op_tx:    UnboundedSender<ClientOperation>,
) {
    tokio::task::spawn_blocking(move || {
        let token = download.token;
        let result = peer
            .download_direct(download, stream)
            .map(|(_, path)| path)
            .map_err(|e| /* convert to SoulseekRs */);
        let _ = op_tx.send(ClientOperation::DownloadCompleted(token, result));
    });
}
```

All three paths call `run_download`. The `DownloadCompleted` handler is the
single place that sends status to the handle and decrements the counter.
Bugs 1 and 2 above are fixed as a side effect.

---

## Issue 4: `connect_to_peer` is a free function with a stale downloads snapshot

**Location:** `src/client/connected_worker.rs`

`connect_to_peer` is a `fn` (not a method) taking 6 parameters including a
full clone of the `downloads` map:

```rust
fn connect_to_peer(
    peer: Peer,
    context: Arc<ClientContext>,
    own_username: String,
    stream: Option<std::net::TcpStream>,
    op_tx: UnboundedSender<ClientOperation>,
    downloads: HashMap<DownloadToken, Download>,  // ← full clone, immediately stale
) { ... }
```

The clone is passed so that the F-type blocking task can call a `resolve`
closure to look up the `Download` by token. But by the time the task runs,
the map snapshot may already be out of date — a cancellation that happened
after the clone won't be visible.

It is also called from 4 different match arms, making it hard to follow the
call graph.

**Fix: `PeerConnector` struct owned by the worker**

```rust
struct PeerConnector {
    context:      Arc<ClientContext>,
    own_username: String,
    op_tx:        UnboundedSender<ClientOperation>,
}

impl PeerConnector {
    fn connect_p(&self, peer: Peer, stream: Option<TcpStream>) { ... }

    fn connect_f(&self, peer: Peer, download: Download, stream: Option<std::net::TcpStream>) {
        run_download(download, DownloadPeer::new(...), stream, self.op_tx.clone());
    }
}
```

The worker looks up the specific `Download` before calling `connect_f` and
passes just that one. No full map clone, no stale snapshot. The connector is
a plain struct — easy to test in isolation.

---

## Issue 5: No visibility into in-flight peer connection attempts

**Current state:** when the worker needs a peer for a download it calls
`connect_to_peer`, which spawns a task and returns nothing. There is no record
of "I am connecting to username X because download Y needs them." If the
connection silently fails or times out, the download hangs with no indication
of why.

**Fix: `PendingConnection` map in the worker**

```rust
struct PendingConnection {
    download_id:  DownloadId,   // which download triggered this
    started_at:   Instant,
    timeout:      Duration,
}

// in ConnectedWorker:
pending_connections: HashMap<String, PendingConnection>  // keyed by username
```

- Add an entry when initiating a connection.
- Remove it on `ConnectionEstablished` (peer actor reports back) or
  `ConnectionFailed`.
- The worker's tick (or `DownloadCompleted`) checks for entries that have
  exceeded their timeout and fails the associated download with a clear
  `ConnectionTimeout` error rather than silently hanging.

Now you can inspect `pending_connections` to see exactly what is in flight and
why.

---

## Issue 6: `PeerRegistry` is shared mutable state across multiple contexts

**Current state:** `PeerRegistry` wraps `Arc<Mutex<HashMap>>` and is accessed
from:

- the worker (register, remove, contains, queue_upload)
- the listener (register — via `ClientContext`)
- `ClientContext` carries it everywhere it is passed

The `Arc<Mutex>` is not a correctness problem (the critical section is very
short), but the shared ownership makes it unclear who is authoritative for
peer state. The listener can register peers independently of the worker, which
means the worker can miss registration races.

**Fix: worker exclusively owns the registry; listener sends an op**

Add a new operation:

```rust
ClientOperation::RegisterPeer {
    peer:   Peer,
    stream: TcpStream,
    reader: MessageReader,
}
```

The listener sends this operation instead of calling `registry.register_peer()`
directly. The worker handles it in its event loop. The registry becomes a plain
`HashMap<String, ActorHandle<PeerMessage>>` with no `Arc<Mutex>` — just a
field on `ConnectedWorker`.

Side effects:
- `ClientContext` no longer needs to carry the registry.
- `ClientContext` may become empty and can be removed entirely.
- The listener only needs `op_tx: UnboundedSender<ClientOperation>` —
  no `Arc<ClientContext>` at all.

Trade-off: one extra channel hop for inbound peer registration. Imperceptible
for a P2P file transfer use case.

---

## Issue 7: `ServerMessage` mixes inbound commands with internal signals

**Current state:** `ServerMessage` is used in two opposite directions:

- **Inbound commands** (worker → actor): `Login`, `FileSearch`,
  `PierceFirewall`, `GetPeerAddress`, `SendMessage`
- **Internal signals** (wire handler → actor, forwarded to worker):
  `LoginStatus`, `ConnectToPeer`, `GetPeerAddressResponse`, `ProcessRead`

The actor handle exposed to the worker is `ActorHandle<ServerMessage>` — the
worker could technically send `LoginStatus` to the actor, which is nonsensical.
`GetPeerAddressResponse` appears in `ServerMessage` even though it is a
response, not a command.

**Fix: split into command and signal enums**

```rust
// Public — what the worker/client sends IN to the actor
pub enum ServerCommand {
    Login { username: String, password: String, response: oneshot::Sender<Result<bool, SoulseekRs>> },
    FileSearch { token: SearchToken, query: String },
    PierceFirewall(PierceToken),
    GetPeerAddress(String),
}

// Private — signals flowing inside the actor between wire handlers and handle_message
enum ServerSignal {
    LoginStatus(bool),
    ConnectToPeer(Peer),
    GetPeerAddressResponse { username: String, host: String, port: u32, ... },
    ProcessRead,
}
```

`ActorHandle<ServerCommand>` is all anyone outside the actor can hold. Sending
an internal signal from the outside becomes a compile error. The same split
applies to `PeerMessage` / `PeerCommand`.

---

## Suggested order of implementation

| Step | Change | Value | Effort |
|---|---|---|---|
| 1 | Fix fake peer usernames in listener | Fixes registry leak + counter bug | Small |
| 2 | Unified `run_download` helper | Fixes double-send bug + consistent failure model | Small |
| 3 | `PeerConnector` struct + pass single download not full map | Removes stale snapshot | Small–Medium |
| 4 | Worker exclusively owns registry, listener sends op | Eliminates shared Mutex, simplifies `ClientContext` | Medium |
| 5 | `PendingConnection` map | In-flight visibility, enforced timeouts | Medium |
| 6 | Split `ServerMessage` into command / signal | Compile-time direction enforcement | Small |
