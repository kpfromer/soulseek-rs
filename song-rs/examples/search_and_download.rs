//! song-rs example: search for a song and download the best match.
//!
//! Run with:
//!   cargo run -p song-rs --example search_and_download

use song_rs::{Client, SongQuery};
use soulseek_rs::types::DownloadStatus;
use std::io::{self, Write};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Credentials ---
    let username = prompt("Username: ")?;
    let password = prompt("Password: ")?;

    let mut client = Client::new(username.trim(), password.trim());

    // --- Song query ---
    let title = prompt("Song title: ")?;
    let artist = prompt("Artist: ")?;
    let duration_str = prompt("Duration in seconds (0 if unknown): ")?;
    let duration_secs: u32 = duration_str.trim().parse().unwrap_or(0);

    let query = SongQuery {
        title: title.trim().to_string(),
        artist: artist.trim().to_string(),
        album: None,
        duration_secs,
    };

    // --- Search ---
    println!("\nSearching for \"{} - {}\" (15s timeout)...", query.title, query.artist);
    let results = client.search(&query, Duration::from_secs(15)).await?;

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    // --- Display top results ---
    let display = results.len().min(15);
    println!("\nTop {} results (sorted by match score):\n", display);
    for (i, r) in results.iter().take(display).enumerate() {
        let size_mb = r.size as f64 / 1_048_576.0;
        let dur = r
            .duration
            .map(|d| format!(", {}s", d))
            .unwrap_or_default();
        let br = r
            .bitrate
            .map(|b| format!(", {}kbps", b))
            .unwrap_or_default();
        println!(
            "  [{i:2}] score={:.2}  {}  ({size_mb:.1} MB{dur}{br})  — {}",
            r.score,
            r.filename.filename(),
            r.username,
        );
    }

    // --- Pick ---
    let choice_str = prompt(&format!("\nSelect [0-{}] (Enter = 0): ", display - 1))?;
    let idx: usize = choice_str.trim().parse().unwrap_or(0).min(display - 1);
    let result = &results[idx];

    // --- Download ---
    println!("\nDownloading \"{}\"...", result.filename.filename());
    let (_dl, mut rx) = client.download(result, "./downloads").await?;

    while let Some(status) = rx.recv().await {
        match status {
            DownloadStatus::Queued => println!("  Queued..."),
            DownloadStatus::InProgress {
                bytes_downloaded,
                total_bytes,
                speed_bytes_per_sec,
            } => {
                let pct = bytes_downloaded as f64 / total_bytes as f64 * 100.0;
                let speed_kb = speed_bytes_per_sec / 1024.0;
                print!("\r  {pct:5.1}%  ({speed_kb:.0} KB/s)   ");
                io::stdout().flush()?;
            }
            DownloadStatus::Completed => {
                println!("\n  Done! Saved to ./downloads/");
                break;
            }
            DownloadStatus::Failed => {
                eprintln!("\n  Download failed.");
                break;
            }
            DownloadStatus::TimedOut => {
                eprintln!("\n  Download timed out.");
                break;
            }
        }
    }

    client.shutdown();
    Ok(())
}

fn prompt(msg: &str) -> io::Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(s)
}
