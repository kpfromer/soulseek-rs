use clap::{Parser, ValueEnum};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use song_rs::{Client, DownloadStatus, SongQuery, WantedFileTypes};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── CSV helpers ──────────────────────────────────────────────────────────────

fn write_tracks_csv(path: &Path, tracks: &[TrackRow]) -> Result<(), Box<dyn std::error::Error>> {
    let tmp = path.with_extension("csv.tmp");
    let mut writer = csv::Writer::from_path(&tmp)?;
    for row in tracks {
        writer.serialize(row)?;
    }
    writer.flush()?;
    drop(writer);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

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
    /// CSV file with columns: title,album,artist,duration_seconds[,is_downloaded]
    /// An optional `is_downloaded` column is added automatically; rows already
    /// marked true are skipped so reruns resume where they left off.
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

    /// Cancel a download if no bytes arrive within this window during active transfer (seconds)
    #[arg(long, default_value = "30")]
    stall_timeout: u64,

    /// Per-status-update timeout while waiting in queue (seconds); re-searched tracks reset this
    #[arg(long, default_value = "300")]
    queue_timeout: u64,

    /// How many times to re-search and retry candidates for a track before skipping it
    #[arg(long, default_value = "3")]
    max_retries: u32,
}

// ── CSV row ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct TrackRow {
    title: String,
    album: String,
    artist: String,
    duration_seconds: u32,
    #[serde(default)]
    is_downloaded: bool,
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Parse CSV
    let mut tracks: Vec<TrackRow> = csv::Reader::from_path(&args.csv)?
        .deserialize()
        .collect::<Result<_, _>>()?;

    if tracks.is_empty() {
        eprintln!("CSV contains no tracks.");
        return Ok(());
    }

    std::fs::create_dir_all(&args.download_dir)?;

    let client = Client::new(&args.username, &args.password);
    let wanted = WantedFileTypes::from(args.file_type);
    let timeout = Duration::from_secs(args.timeout);
    let download_dir = args.download_dir.to_string_lossy().to_string();
    let total = tracks.len() as u64;
    let progress_timeout = Some(Duration::from_secs(args.stall_timeout));
    let recv_timeout = Some(Duration::from_secs(args.queue_timeout));

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

    // ── Per-track loop ───────────────────────────────────────────────────────

    'tracks: for i in 0..tracks.len() {
        let label = format!("\"{}\" — {}", tracks[i].title, tracks[i].artist);
        overall.set_message(label.clone());

        if tracks[i].is_downloaded {
            overall.inc(1);
            continue;
        }

        let query = SongQuery {
            title: tracks[i].title.clone(),
            artist: tracks[i].artist.clone(),
            album: Some(tracks[i].album.clone()),
            duration_secs: tracks[i].duration_seconds,
        };

        for retry in 0..args.max_retries {
            let search_spinner = mp.add(ProgressBar::new_spinner());
            search_spinner.enable_steady_tick(Duration::from_millis(100));
            if retry == 0 {
                search_spinner.set_message(format!("Searching for {label}"));
            } else {
                search_spinner.set_message(format!("Retry {retry}/{} for {label}", args.max_retries - 1));
            }

            let results = match client.search(&query, timeout, &wanted).await {
                Ok(r) if !r.is_empty() => r,
                Ok(_) => {
                    search_spinner.finish_and_clear();
                    if retry + 1 == args.max_retries {
                        overall.println(format!("✗ No results  — {label}"));
                    }
                    continue;
                }
                Err(e) => {
                    search_spinner.finish_and_clear();
                    if retry + 1 == args.max_retries {
                        overall.println(format!("✗ Search error ({e})  — {label}"));
                    }
                    continue;
                }
            };

            let candidate_count = results.len();

            'candidates: for (attempt, result) in results.iter().enumerate() {
                let attempt_label = format!(
                    "[{}/{}] {label}  (candidate {}/{}{})",
                    i + 1,
                    total,
                    attempt + 1,
                    candidate_count,
                    if retry > 0 { format!(", retry {retry}") } else { String::new() },
                );
                search_spinner.set_message(attempt_label.clone());

                let (_dl, mut handle) = match client
                    .download(result, &download_dir, progress_timeout, recv_timeout)
                    .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        search_spinner.println(format!("  ↳ skip {} (could not initiate download: {e})", result.filename));
                        continue;
                    }
                };

                let mut track_bar: Option<ProgressBar> = None;

                while let Some(status) = handle.recv().await {
                    match status {
                        DownloadStatus::QueuedLocally => {
                            search_spinner.set_message(format!("{attempt_label}  [queued locally]"));
                        }
                        DownloadStatus::QueuedRemotely { place } => {
                            search_spinner.set_message(format!(
                                "{attempt_label}  [queued remotely: {:?}]",
                                place
                            ));
                        }
                        DownloadStatus::InProgress {
                            bytes_downloaded,
                            total_bytes,
                            ..
                        } => match track_bar.as_mut() {
                            Some(bar) => {
                                bar.set_length(total_bytes);
                                bar.set_position(bytes_downloaded);
                            }
                            None => {
                                let new_track_bar = mp.add(ProgressBar::new(1));
                                new_track_bar.set_style(
                                    ProgressStyle::with_template(
                                        "  {msg}\n  [{bar:40.green/white}] {bytes}/{total_bytes}  {binary_bytes_per_sec}  eta {eta}",
                                    )?
                                    .progress_chars("█▓░"),
                                );
                                track_bar = Some(new_track_bar);
                            }
                        },
                        DownloadStatus::Completed => {
                            tracks[i].is_downloaded = true;
                            if let Err(e) = write_tracks_csv(&args.csv, &tracks) {
                                if let Some(bar) = &track_bar {
                                    bar.println(format!("  Warning: could not update CSV: {e}"));
                                }
                            }
                            if let Some(bar) = track_bar.take() {
                                bar.set_length(bar.length().unwrap_or(1));
                                bar.finish_with_message(format!("✓ {label}"));
                            }
                            search_spinner.finish_and_clear();
                            overall.inc(1);
                            continue 'tracks;
                        }
                        DownloadStatus::Failed => {
                            search_spinner.println(format!("  ↳ skip {} (download failed)", result.filename));
                            if let Some(bar) = track_bar.take() {
                                bar.finish_and_clear();
                            }
                            break 'candidates;
                        }
                        DownloadStatus::TimedOut => {
                            search_spinner.println(format!("  ↳ skip {} (timed out)", result.filename));
                            if let Some(bar) = track_bar.take() {
                                bar.finish_and_clear();
                            }
                            break;
                        }
                        DownloadStatus::Cancelled => {
                            search_spinner.println(format!("  ↳ skip {} (cancelled)", result.filename));
                            if let Some(bar) = track_bar.take() {
                                bar.finish_and_clear();
                            }
                            break;
                        }
                    }
                }
            }

            search_spinner.finish_and_clear();
        }

        overall.inc(1);
    }

    overall.finish_with_message("complete");

    Ok(())
}
