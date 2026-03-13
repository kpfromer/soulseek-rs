use clap::{Parser, ValueEnum};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;
use song_rs::{Client, DownloadStatus, SongQuery, WantedFileTypes};
use std::path::PathBuf;
use std::time::Duration;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(ValueEnum, Debug, Clone, Copy)]
enum FileTypeArg {
    Lossless,
    Lossy,
    All,
}

impl From<FileTypeArg> for WantedFileTypes {
    fn from(v: FileTypeArg) -> Self {
        match v {
            FileTypeArg::Lossless => WantedFileTypes::lossless(),
            FileTypeArg::Lossy => WantedFileTypes::lossy(),
            FileTypeArg::All => WantedFileTypes::all(),
        }
    }
}

#[derive(Parser)]
#[command(about = "Batch-download tracks listed in a CSV via Soulseek")]
struct Args {
    /// CSV file with header: title,album,artist,duration_seconds
    csv: PathBuf,

    /// Directory to save downloaded files into
    #[arg(long, default_value = "./downloads")]
    download_dir: PathBuf,

    #[arg(long, env = "SOULSEEK_USERNAME")]
    username: String,

    #[arg(long, env = "SOULSEEK_PASSWORD")]
    password: String,

    /// How long to wait for search results per track (seconds)
    #[arg(long, default_value = "15")]
    timeout: u64,

    /// Preferred file format(s)
    #[arg(long, default_value = "all")]
    file_type: FileTypeArg,
}

// ── CSV row ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TrackRow {
    title: String,
    album: String,
    artist: String,
    duration_seconds: u32,
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Parse CSV
    let tracks: Vec<TrackRow> = csv::Reader::from_path(&args.csv)?
        .deserialize()
        .collect::<Result<_, _>>()?;

    if tracks.is_empty() {
        eprintln!("CSV contains no tracks.");
        return Ok(());
    }

    std::fs::create_dir_all(&args.download_dir)?;

    let mut client = Client::new(&args.username, &args.password);
    let wanted = WantedFileTypes::from(args.file_type);
    let timeout = Duration::from_secs(args.timeout);
    let download_dir = args.download_dir.to_string_lossy().to_string();
    let total = tracks.len() as u64;

    // ── Progress bars ────────────────────────────────────────────────────────

    let mp = MultiProgress::new();

    let overall = mp.add(ProgressBar::new(total));
    overall.set_style(
        ProgressStyle::with_template(
            "{prefix:.bold.cyan} [{bar:40.cyan/blue}] {pos}/{len}  {msg}",
        )?
        .progress_chars("█▓░"),
    );
    overall.set_prefix("Tracks");

    let track_bar = mp.add(ProgressBar::new(1));
    track_bar.set_style(
        ProgressStyle::with_template(
            "  {msg}\n  [{bar:40.green/white}] {bytes}/{total_bytes}  {binary_bytes_per_sec}  eta {eta}",
        )?
        .progress_chars("█▓░"),
    );

    // ── Per-track loop ───────────────────────────────────────────────────────

    for (i, row) in tracks.iter().enumerate() {
        let label = format!("\"{}\" — {}", row.title, row.artist);
        overall.set_message(label.clone());
        track_bar.reset();
        track_bar.set_message(format!("Searching {label}…"));

        let query = SongQuery {
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: Some(row.album.clone()),
            duration_secs: row.duration_seconds,
        };

        let results = match client.search(&query, timeout, &wanted).await {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => {
                track_bar.set_message(format!("✗ No results  — {label}"));
                overall.inc(1);
                continue;
            }
            Err(e) => {
                track_bar.set_message(format!("✗ Search error ({e})  — {label}"));
                overall.inc(1);
                continue;
            }
        };

        let candidate_count = results.len();
        let mut succeeded = false;

        'candidates: for (attempt, result) in results.iter().enumerate() {
            let attempt_label = format!(
                "[{}/{}] {label}  (candidate {}/{})",
                i + 1,
                total,
                attempt + 1,
                candidate_count,
            );
            track_bar.set_message(attempt_label.clone());
            track_bar.set_position(0);

            let (_dl, mut rx) = match client.download(result, &download_dir).await {
                Ok(pair) => pair,
                Err(_) => continue,
            };

            while let Some(status) = rx.recv().await {
                match status {
                    DownloadStatus::Queued => {
                        track_bar.set_message(format!("{attempt_label}  [queued]"));
                    }
                    DownloadStatus::InProgress {
                        bytes_downloaded,
                        total_bytes,
                        ..
                    } => {
                        track_bar.set_length(total_bytes);
                        track_bar.set_position(bytes_downloaded);
                    }
                    DownloadStatus::Completed => {
                        succeeded = true;
                        track_bar.set_length(track_bar.length().unwrap_or(1));
                        track_bar.finish_with_message(format!("✓ {label}"));
                        break 'candidates;
                    }
                    DownloadStatus::Failed | DownloadStatus::TimedOut => {
                        break;
                    }
                }
            }
        }

        if !succeeded {
            track_bar.abandon_with_message(format!(
                "✗ All {candidate_count} candidates failed  — {label}"
            ));
        }

        overall.inc(1);
    }

    overall.finish_with_message("complete");

    Ok(())
}
