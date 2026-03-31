use clap::Parser;
use std::path::PathBuf;

/// Clean and enrich audio file metadata using MusicBrainz and Cover Art Archive.
///
/// Discovers audio files, queries MusicBrainz via acoustic fingerprinting for
/// authoritative metadata, embeds cover art from the Cover Art Archive, backs
/// up original tag values under KYLE_-prefixed custom fields, and copies or
/// moves the enriched files to an output directory using a customisable naming
/// template.
#[derive(Parser, Debug)]
#[command(name = "music-cleaner", version, about)]
pub struct Args {
    /// Path to an audio file or directory to process.
    pub input: PathBuf,

    /// Output root directory.
    ///
    /// Defaults to the same directory as each source file when omitted.
    #[arg(short = 'o', long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,

    /// File naming template.
    ///
    /// Supported variables: {title}, {album}, {artist}, {album_artist},
    /// {track_number}, {disc_number}, {year}, {ext}.
    ///
    /// Forward slashes in the template create subdirectories under the output
    /// root. Example: "{artist}/{album}/{track_number} - {title}.{ext}"
    /// produces <output_dir>/Artist/Album/01 - Title.flac.
    #[arg(
        short = 't',
        long,
        default_value = "{track_number} - {title}.{ext}",
        value_name = "TEMPLATE",
        verbatim_doc_comment
    )]
    pub template: String,

    /// Recurse into subdirectories when input is a directory.
    #[arg(short = 'r', long)]
    pub recursive: bool,

    /// Move files to the output directory instead of copying them.
    #[arg(long = "move", id = "move_files")]
    pub move_files: bool,

    /// Overwrite existing output files without prompting.
    ///
    /// Without this flag, the tool prompts interactively (or skips in
    /// --non-interactive mode) when the destination file already exists.
    #[arg(long)]
    pub overwrite_files: bool,

    /// Re-process metadata even when KYLE_PROCESSED_AT is already present.
    ///
    /// By default files that were previously processed by music-cleaner are
    /// skipped. This flag forces a fresh MusicBrainz lookup and tag rewrite.
    /// Existing KYLE_* backup tags are always preserved regardless.
    #[arg(long)]
    pub overwrite_metadata: bool,

    /// Non-interactive mode: never show prompts.
    ///
    /// File conflicts are silently skipped unless --overwrite-files is also
    /// set, in which case they are silently overwritten.
    #[arg(long)]
    pub non_interactive: bool,

    /// AcoustID API key used for audio fingerprint lookups.
    #[arg(long, env = "ACOUSTID_API_KEY", value_name = "KEY")]
    pub acoustid_api_key: String,
}
