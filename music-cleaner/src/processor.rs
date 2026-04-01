//! File discovery and per-file processing pipeline.

use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::Select;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use crate::cli::Args;
use crate::{metadata, template};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "m4a", "wav", "aiff", "aif", "opus",
];

// ─── Entry point ────────────────────────────────────────────────────────────

/// Discover audio files under `args.input` and process each one in sequence.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let files = discover_files(&args.input, args.recursive);

    if files.is_empty() {
        println!("No audio files found at: {}", args.input.display());
        return Ok(());
    }

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
        )?
        .progress_chars("=>-"),
    );

    let mut succeeded: u64 = 0;
    let mut skipped: u64 = 0;
    let mut failed: u64 = 0;

    for file in &files {
        let display = file.display().to_string();
        pb.set_message(format!("Processing: {display}"));

        match process_file(file, &args).await {
            Ok(ProcessOutcome::Done) => succeeded += 1,
            Ok(ProcessOutcome::Skipped(reason)) => {
                pb.println(format!("  Skipped {display}: {reason}"));
                skipped += 1;
            }
            Err(e) => {
                pb.println(format!("  Error processing {display}: {e:#}"));
                failed += 1;
            }
        }

        pb.inc(1);
    }

    pb.finish_with_message("Done");
    println!("\nProcessed {succeeded} file(s), skipped {skipped}, failed {failed}.");

    Ok(())
}

// ─── Per-file pipeline ───────────────────────────────────────────────────────

enum ProcessOutcome {
    Done,
    Skipped(String),
}

async fn process_file(path: &Path, args: &Args) -> anyhow::Result<ProcessOutcome> {
    // ── 1. Already-processed guard ──────────────────────────────────────────
    let already_processed = metadata::is_already_processed(path)?;
    if already_processed && !args.overwrite_metadata {
        return Ok(ProcessOutcome::Skipped(
            "already processed (KYLE_PROCESSED_AT present; use --overwrite-metadata to re-run)"
                .into(),
        ));
    }

    // ── 2. MusicBrainz lookup ───────────────────────────────────────────────
    let track_meta = musicbrainz::lookup_track(path, &args.acoustid_api_key).await?;

    // ── 3. Cover art (non-fatal) ────────────────────────────────────────────
    let art = match cover_art_archive::get_album_art(&track_meta.release_mbid).await {
        Ok(art) => Some(art),
        Err(e) => {
            // Emit a warning line above the progress bar but continue.
            eprintln!("  [warn] Cover art unavailable for {}: {e}", path.display());
            None
        }
    };

    // ── 4. Write metadata to the source file in-place ───────────────────────
    metadata::apply_metadata(path, &track_meta, art.as_ref(), already_processed)?;

    // ── 5. Render output path ────────────────────────────────────────────────
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    let relative = template::render(&args.template, &track_meta, ext);

    let output_root = args
        .output_dir
        .clone()
        .unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")).to_path_buf());

    let output_path = output_root.join(&relative);

    // ── 6. No-op when source == destination ─────────────────────────────────
    if paths_are_same(path, &output_path) {
        return Ok(ProcessOutcome::Done);
    }

    // ── 7. Create subdirectory tree ──────────────────────────────────────────
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // ── 8. Resolve file conflict ─────────────────────────────────────────────
    if output_path.exists() && !resolve_file_conflict(&output_path, args)? {
        return Ok(ProcessOutcome::Skipped(format!(
            "output already exists: {}",
            output_path.display()
        )));
    }

    // ── 9. Copy or move ──────────────────────────────────────────────────────
    if args.move_files {
        fs::rename(path, &output_path)?;
    } else {
        fs::copy(path, &output_path)?;
    }

    Ok(ProcessOutcome::Done)
}

// ─── Conflict resolution ─────────────────────────────────────────────────────

/// Determine whether an existing output file should be overwritten.
///
/// Returns `true` → overwrite, `false` → skip.
fn resolve_file_conflict(output_path: &Path, args: &Args) -> anyhow::Result<bool> {
    if args.overwrite_files {
        return Ok(true);
    }

    if args.non_interactive {
        return Ok(false);
    }

    // Interactive prompt (only reached when neither flag is set).
    let options = ["Skip", "Overwrite"];
    let selection = Select::new()
        .with_prompt(format!(
            "Output already exists: {}\nWhat would you like to do?",
            output_path.display()
        ))
        .items(&options)
        .default(0)
        .interact()?;

    Ok(selection == 1)
}

// ─── File discovery ───────────────────────────────────────────────────────────

/// Collect all audio files reachable from `input`.
///
/// If `input` is a regular file it is returned directly (if it is an audio
/// file). If it is a directory, the tree is walked to at most depth 1 unless
/// `recursive` is `true`.
fn discover_files(input: &Path, recursive: bool) -> Vec<PathBuf> {
    if input.is_file() {
        return if is_audio_file(input) {
            vec![input.to_path_buf()]
        } else {
            vec![]
        };
    }

    let max_depth = if recursive { usize::MAX } else { 1 };

    WalkDir::new(input)
        .max_depth(max_depth)
        .follow_links(true)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|p| is_audio_file(p))
        .collect()
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Best-effort check for whether two paths refer to the same file.
///
/// Uses [`fs::canonicalize`] when the target already exists; falls back to a
/// simple byte comparison otherwise.
fn paths_are_same(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}
