//! File discovery and per-file processing pipeline.

use std::fs;
use std::path::{Path, PathBuf};

use cover_art_archive::AlbumArt;
use dialoguer::Select;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use crate::cli::Args;
use crate::metadata::ResolvedTrackMetadata;
use crate::{metadata, template};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "m4a", "wav", "aiff", "aif", "opus"];

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
    let mut bad_files: u64 = 0;
    let mut unknown_count: u64 = 0;

    for file in &files {
        let display = file.display().to_string();
        pb.set_message(format!("Processing: {display}"));

        match process_file(file, &args).await {
            Ok(ProcessOutcome::Done) => succeeded += 1,
            Ok(ProcessOutcome::BadFile(e)) => {
                pb.println(format!("  Bad audio file: {display}: {e:#}"));
                bad_files += 1;
            }
            Ok(ProcessOutcome::Skipped(reason)) => {
                pb.println(format!("  Skipped {display}: {reason}"));
                skipped += 1;
            }
            Ok(ProcessOutcome::RoutedToUnknownDirectory) => {
                pb.println(format!(
                    "  No metadata for {display}: moved to unknown directory"
                ));
                unknown_count += 1;
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
    println!("Found {bad_files} bad files.");
    if unknown_count > 0 {
        println!("{unknown_count} file(s) with no metadata copied to unknown directory.");
    }

    Ok(())
}

// ─── Per-file pipeline ───────────────────────────────────────────────────────

enum ProcessOutcome {
    Done,
    Skipped(String),
    RoutedToUnknownDirectory,
    BadFile(audio_check::AudioError),
}

#[allow(clippy::large_enum_variant)]
enum LookupOutcome {
    Resolved(ResolvedTrackMetadata),
    NoMetadataAvailable,
}

async fn process_file(path: &Path, args: &Args) -> anyhow::Result<ProcessOutcome> {
    {
        let path_clone = path.to_path_buf();
        if let Err(e) =
            tokio::task::spawn_blocking(move || audio_check::check_file(&path_clone)).await?
        {
            return Ok(ProcessOutcome::BadFile(e));
        }
    }

    // ── 1. Already-processed guard ──────────────────────────────────────────
    let already_processed = metadata::is_already_processed(path)?;
    if already_processed && !args.overwrite_metadata {
        return Ok(ProcessOutcome::Skipped(
            "already processed (KYLE_PROCESSED_AT present; use --overwrite-metadata to re-run)"
                .into(),
        ));
    }

    let output_root = args
        .output_dir
        .clone()
        .unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")).to_path_buf());

    // ── 2. MusicBrainz lookup (with fallback to existing tags) ──────────────
    let display = path.display().to_string();
    let lookup = if args.no_musicbrainz {
        match metadata::read_existing_tags(path)? {
            Some(tags) => LookupOutcome::Resolved(ResolvedTrackMetadata::ExistingFile(tags)),
            None => LookupOutcome::NoMetadataAvailable,
        }
    } else {
        let api_key = args.acoustid_api_key.as_deref().unwrap_or_default();
        match musicbrainz::lookup_track(path, api_key).await {
            Ok(meta) => LookupOutcome::Resolved(ResolvedTrackMetadata::MusicBrainz(meta)),
            Err(e) => {
                eprintln!("  [warn] MusicBrainz lookup failed for {display}: {e}");
                match metadata::read_existing_tags(path)? {
                    Some(tags) => {
                        LookupOutcome::Resolved(ResolvedTrackMetadata::ExistingFile(tags))
                    }
                    None => LookupOutcome::NoMetadataAvailable,
                }
            }
        }
    };

    // ── 2a. No metadata at all → route to unknown dir ───────────────────────
    if let LookupOutcome::NoMetadataAvailable = lookup {
        let unknown_dir = args
            .unknown_dir
            .clone()
            .unwrap_or_else(|| output_root.join("unknown"));
        fs::create_dir_all(&unknown_dir)?;
        let dest = unknown_dir.join(path.file_name().unwrap_or_default());
        if args.move_files {
            fs::rename(path, &dest)?;
        } else {
            fs::copy(path, &dest)?;
        }
        return Ok(ProcessOutcome::RoutedToUnknownDirectory);
    }

    let LookupOutcome::Resolved(track_meta) = lookup else {
        unreachable!()
    };

    // ── 3. Cover art (non-fatal, MusicBrainz only) ─────────────────────────
    let art: Option<AlbumArt> = if !args.no_musicbrainz {
        if let ResolvedTrackMetadata::MusicBrainz(m) = &track_meta {
            match cover_art_archive::get_album_art(&m.release_mbid).await {
                Ok(art) => Some(art),
                Err(e) => {
                    eprintln!("  [warn] Cover art unavailable for {}: {e}", path.display());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── 3a. Optionally resize cover art ─────────────────────────────────────
    let used_musicbrainz = matches!(&track_meta, ResolvedTrackMetadata::MusicBrainz(_));
    let safe_to_resize = used_musicbrainz || !args.move_files;

    let art = if safe_to_resize {
        match (art, args.max_image_width, args.max_image_height) {
            (Some(a), Some(max_w), Some(max_h)) => Some(maybe_resize_art(a, max_w, max_h)?),
            (a, _, _) => a,
        }
    } else {
        art
    };

    // ── 4. Write metadata to the source file in-place ───────────────────────
    if !args.no_musicbrainz {
        metadata::apply_metadata(path, &track_meta, art.as_ref(), already_processed)?;
    }

    // ── 5. Render output path ────────────────────────────────────────────────
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");

    let relative = template::render(&args.template, &track_meta, ext);

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

// ─── Image resize ─────────────────────────────────────────────────────────────

fn maybe_resize_art(art: AlbumArt, max_w: u32, max_h: u32) -> anyhow::Result<AlbumArt> {
    let img = image::load_from_memory(&art.data)?;
    if img.width() <= max_w && img.height() <= max_h {
        return Ok(art);
    }
    let resized = img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3);
    let mut buf = Vec::new();
    resized.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    )?;
    Ok(AlbumArt {
        data: buf,
        extension: "jpg".to_string(),
    })
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
