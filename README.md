# soulseek-rs-lib

A Rust library for the Soulseek peer-to-peer file-sharing protocol.

## What it does

- Connects and authenticates to the Soulseek server
- Searches for files across the network
- Downloads files from peers (direct and pierce-firewall paths)
- Reconnects automatically with exponential backoff
- Optionally listens for inbound peer connections

## Feature flags

| Flag | Default | Effect |
|---|---|---|
| `tracing` | off | Enables logging via the `tracing` crate |

## Quick start

Add to `Cargo.toml`:

```toml
[dependencies]
soulseek-rs-lib = { path = "." }       # or the published crate name/version
```

Connect, search, and download:

```rust
use soulseek_rs::{Client, DownloadStatus};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::new("username", "password");

    // connect() blocks until login succeeds (or returns an error).
    client.connect().await?;

    // search() blocks for `timeout`, then returns whatever results arrived.
    let results = client.search("Artist - Album", Duration::from_secs(10)).await?;

    if let Some(file) = results.iter().flat_map(|r| &r.files).next() {
        let (_download, mut rx) = client.download(
            file.name.clone(),
            file.username.clone(),
            file.size,
            "./downloads".to_string(),
        )?;

        while let Some(status) = rx.recv().await {
            match status {
                DownloadStatus::InProgress { bytes_downloaded, total_bytes, .. } => {
                    println!("{}/{} bytes", bytes_downloaded, total_bytes);
                }
                DownloadStatus::Completed => { println!("Done!"); break; }
                DownloadStatus::Failed    => { eprintln!("Failed"); break; }
                _ => {}
            }
        }
    }

    client.shutdown();
    Ok(())
}
```

## Custom settings

```rust
use soulseek_rs::{Client, ClientSettings};
use soulseek_rs::actor::server_actor::PeerAddress;
use soulseek_rs::{KeepAliveSettings, ReconnectSettings};
use std::time::Duration;

let settings = ClientSettings {
    server_address: PeerAddress::new("server.slsknet.org".to_string(), 2416),
    enable_listen: true,
    listen_port: 2234,
    reconnect_settings: ReconnectSettings::EnabledExponentialBackoff {
        min_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(120),
        max_attempts: Some(10),
    },
    ..ClientSettings::new("username", "password")
};

let mut client = Client::with_settings(settings);
client.connect().await?;
```

## Running the example

```sh
cargo run --example search_and_download --features tracing
```

The example prompts for credentials, runs a search, and downloads a chosen
file. Logs are written to `downloads/soulseek.log`.

## Documentation

- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) — Soulseek wire protocol reference
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — internal design and message
  flow

## Notes on security

Soulseek transmits credentials (username and password) in plain text over an
unencrypted TCP connection. The `PlainTextUnencrypted` wrapper in this library
makes this explicit at the type level.
