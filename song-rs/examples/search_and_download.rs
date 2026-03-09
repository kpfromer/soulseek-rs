//! song-rs example: search for a song and download the best match.
//!
//! Run with:
//!   cargo run -p song-rs --example search_and_download

use clap::Parser;
use song_rs::{Client, SongQuery};
use soulseek_rs::types::DownloadStatus;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "./downloads")]
    download_dir: PathBuf,

    #[arg(long)]
    username: Option<String>,

    #[arg(long)]
    password: Option<String>,

    #[arg(long, default_value = "10")]
    timeout: u64,

    #[arg(long, default_value = "false")]
    download_best: bool,

    #[arg(long)]
    title: Option<String>,

    #[arg(long)]
    artist: Option<String>,

    #[arg(long)]
    album: Option<String>,

    #[arg(long)]
    duration: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // --- Credentials ---
    let username = args
        .username
        .map(Ok)
        .unwrap_or_else(|| prompt("Username: "))?;
    let password = args
        .password
        .map(Ok)
        .unwrap_or_else(|| prompt("Password: "))?;

    let mut client = Client::new(username, password);

    // --- Song query ---
    let title = args
        .title
        .map(Ok)
        .unwrap_or_else(|| prompt("Song title: "))?;
    let artist = args.artist.map(Ok).unwrap_or_else(|| prompt("Artist: "))?;
    let album = args.album.map(Ok).unwrap_or_else(|| prompt("Album: "))?;
    let duration_secs = args
        .duration
        .map(Ok::<_, Box<dyn std::error::Error>>)
        .unwrap_or_else(|| {
            let duration = prompt("Duration in seconds: ")?;
            let duration: u32 = duration.trim().parse()?;
            Ok(duration)
        })?;

    let query = SongQuery {
        title,
        artist,
        album: Some(album),
        duration_secs,
    };

    // --- Search ---
    println!(
        "\nSearching for \"{} - {}\" ({}s timeout)...",
        query.title, query.artist, args.timeout
    );
    if args.download_best {
        let (result, _dl, mut rx) = client
            .download_best(
                &query,
                Duration::from_secs(args.timeout),
                args.download_dir.to_string_lossy().to_string(),
            )
            .await?;

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
        println!("Downloaded \"{}\"...", result.filename.filename());
        return Ok(());
    }

    let results = client
        .search(&query, Duration::from_secs(args.timeout))
        .await?;

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    // --- Display top results ---
    let display = results.len().min(15);
    println!("\nTop {} results (sorted by match score):\n", display);
    for (i, r) in results.iter().take(display).enumerate() {
        let size_mb = r.size as f64 / 1_048_576.0;
        let dur = r.duration.map(|d| format!(", {}s", d)).unwrap_or_default();
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
    let (_dl, mut rx) = client
        .download(result, args.download_dir.to_string_lossy().to_string())
        .await?;

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

    Ok(())
}

fn prompt(msg: &str) -> io::Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}
