# Soulseek Protocol

This document describes the Soulseek protocol as implemented by this library.
Soulseek is a peer-to-peer file-sharing network with a central coordination
server. There is no official protocol specification; this is derived from
reverse engineering by the community.

References:
- https://nicotine-plus.org/doc/SLSKPROTOCOL.html (Nicotine+ protocol notes)
- https://museek-plus.org/wiki/SoulseekProtocol (archived)

---

## Network topology

```
                    ┌──────────────────┐
                    │  Soulseek server  │
                    │  server.slsknet.org:2416 │
                    └──────┬───────────┘
                           │ TCP (persistent)
              ┌────────────┤
              │            │
        ┌─────▼────┐  ┌────▼─────┐
        │  Client A│  │  Client B │
        └─────┬────┘  └────┬─────┘
              │             │
              └──── TCP ────┘  (direct peer connection for file transfer)
```

There are two distinct connection types:

1. **Client ↔ Server** — one persistent TCP connection to the coordination
   server. Used for login, search, and address lookups.
2. **Client ↔ Peer** — short-lived TCP connections between clients. Used for
   browsing shares and downloading files.

---

## Wire format

All integers are **little-endian**. Every message is prefixed by a 4-byte
unsigned integer giving the length of the remaining payload (i.e. the length
does not include itself).

```
┌──────────────────────────────┬─────────────────────────────────┐
│  Message length (4 bytes LE) │  Payload (length bytes)         │
└──────────────────────────────┴─────────────────────────────────┘
```

Within the payload, the first 4 bytes are the **message code** (also LE u32),
followed by message-specific fields.

String fields are encoded as a 4-byte length followed by that many UTF-8 bytes
(no null terminator).

---

## Server messages

The client maintains one persistent TCP connection to the Soulseek server.

### Login (code 1)

**Client → Server**

| Field | Type | Notes |
|---|---|---|
| username | string | |
| password | string | sent in plain text |
| version | u32 | protocol version (157 or 180) |
| hash | string | MD5 of `username + password` (login validation) |
| minor_version | u32 | 17 |

**Server → Client (LoginStatus)**

| Field | Type | Notes |
|---|---|---|
| success | bool | 1 = logged in |
| greet / reason | string | server greeting or failure reason |

After a successful login the client should send:
- `SetSharedFolders` (code 35) — number of shared folders/files
- `NoParents` (code 71) — indicate we don't want distributed search parents
- `SetStatus` (code 28) — online status (2 = online)
- `SetWaitPort` (code 2) — the TCP port we listen on for incoming peer connections

### FileSearch (code 26)

**Client → Server**

| Field | Type | Notes |
|---|---|---|
| token | u32 | chosen by client, echoed back in results |
| query | string | search terms |

The server broadcasts the query to online users. Peers that have matching files
respond directly to the searching client (not through the server) with a
`FileSearchResponse` peer message.

### GetPeerAddress (code 3)

**Client → Server**

| Field | Type | Notes |
|---|---|---|
| username | string | user whose address we want |

**Server → Client (GetPeerAddressResponse, code 3)**

| Field | Type | Notes |
|---|---|---|
| username | string | |
| ip | u32 | big-endian IPv4 |
| port | u32 | |
| obfuscation_type | u32 | 0 = plain, 2 = obfuscated |
| obfuscated_port | u16 | only meaningful when obfuscation_type ≠ 0 |

### ConnectToPeer (code 18)

Sent in both directions to broker peer connections.

**Server → Client** (server asks us to connect to a peer)

| Field | Type | Notes |
|---|---|---|
| token | u32 | pierce-firewall token |
| username | string | peer's username |
| connection_type | string | `"P"` = peer, `"F"` = pierce, `"D"` = distributed |
| ip | u32 | peer's address |
| port | u32 | |
| obfuscation_type | u32 | |
| obfuscated_port | u16 | |
| privileged | bool | whether the peer is a privileged user |

**Client → Server** (we ask the server to tell a peer to connect to us)
Same fields — the server forwards it to the named peer.

### PierceFirewall (code 18 / peer message 0)

Used for NAT traversal when the target peer cannot accept inbound connections
(is behind a firewall). See [Peer connections](#peer-connections) below.

---

## Peer messages

Peer connections use the same length-prefixed wire format but a separate set of
message codes.

### Handshake (PeerInit, code 1)

The connecting side sends this immediately after the TCP connection is
established:

| Field | Type | Notes |
|---|---|---|
| username | string | connecting user's username |
| connection_type | string | `"P"` or `"F"` |
| token | u32 | |

### QueueUpload (code 43)

**Downloader → Uploader**

| Field | Type | Notes |
|---|---|---|
| filename | string | backslash-separated Soulseek path |

Requests that the peer queue the named file for upload.

### TransferRequest (code 40)

**Uploader → Downloader**

| Field | Type | Notes |
|---|---|---|
| direction | u32 | 0 = uploader→downloader |
| token | u32 | new token for this specific transfer |
| filename | string | |
| size | u64 | file size in bytes |

Sent by the peer to acknowledge the queue and propose a token + size for the
actual transfer.

### TransferResponse (code 41)

**Downloader → Uploader**

| Field | Type | Notes |
|---|---|---|
| token | u32 | echoes TransferRequest token |
| allowed | bool | whether we accept the transfer |

### PlaceInQueueResponse (code 44)

**Uploader → Downloader** — tells the downloader their queue position.

### UploadFailed (code 46)

**Uploader → Downloader** — signals that the uploader cannot send the file.

### FileSearchResponse (code 9)

**Peer → Searching client** — compressed (zlib deflate) list of matching files.

Each file entry contains:
- code (u8, always 1)
- filename (string, backslash path)
- size (u64)
- extension (string, ignored)
- attributes (repeated `{code u32, value u32}` pairs):
  - 0 = bitrate (kbps)
  - 1 = duration (seconds)
  - 2 = VBR flag
  - 4 = sample rate (Hz)
  - 5 = bit depth

---

## Peer connections

### Normal connection (type "P")

```
Client A (downloader)                Client B (uploader)
        │                                    │
        │────── TCP connect ────────────────▶│
        │────── PeerInit(username, "P") ────▶│
        │                                    │
        │◀───── QueueUpload (ack) ───────────│
        │◀───── TransferRequest(token, size) │
        │────── TransferResponse(ok) ───────▶│
        │                                    │
        │  ─── direct TCP download ──────── ▶│
```

### Pierce-firewall connection (type "F")

Used when the uploader cannot accept inbound connections. The server acts as a
rendezvous:

```
Client A (downloader)     Server          Client B (uploader)
        │                    │                    │
        │ GetPeerAddress(B) ─▶│                    │
        │◀─ PeerAddressResp ─│                    │
        │                    │                    │
        │ ConnectToPeer(F) ──▶│                    │
        │                    │─ ConnectToPeer(F) ─▶│
        │                    │                    │ B connects to A
        │◀────────────── TCP connect ─────────────│
        │◀── PierceFirewall(token) ───────────────│
        │                                         │
  (A resolves token → Download, sends START_DOWNLOAD, streams data)
```

In the pierce path the first bytes from the peer are a 4-byte download token
(not a standard message frame). The downloader uses this token to look up which
`Download` the connection is for, then proceeds the same as a normal download.

---

## Download wire sequence

After the peer connection is established:

```
Downloader → Uploader:  8 zero bytes (START_DOWNLOAD signal)
Uploader   → Downloader: raw file bytes until size is reached
```

The file is streamed raw with no additional framing after the start signal.

---

## Path conventions

Soulseek paths use **backslashes** as separators (Windows convention), e.g.:

```
Shared Music\Artist\Album\01 - Track.flac
```

The first segment is the virtual share name (not a real filesystem directory).
`SoulseekPath::filename()` returns the last backslash-separated component.
`SoulseekPath::components()` returns all segments after the virtual share name.

Paths received from the wire are normalised: any `/` characters are converted
to `\` by `SoulseekPath::from_wire()`.

---

## Tokens

Tokens are opaque 32-bit unsigned integers chosen by the client. There are two
distinct token spaces:

| Type | Purpose |
|---|---|
| `SearchToken` | Identifies a search query; echoed in `FileSearchResponse` |
| `DownloadToken` | Identifies a specific file transfer; chosen per-download |
| `PierceToken` | Identifies a pierce-firewall connection attempt |

This library uses separate newtype wrappers for each (`token.rs`) to prevent
accidentally mixing them.

---

## Rate limiting

The Soulseek server enforces search rate limits. Exceeding them results in a
temporary ban. The default settings in this library cap searches at **34 per
220 seconds**, which matches observed server behaviour.
