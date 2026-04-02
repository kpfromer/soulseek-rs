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
    ///
    /// Required unless --no-musicbrainz is set.
    #[arg(
        long,
        env = "ACOUSTID_API_KEY",
        value_name = "KEY",
        required_unless_present = "no_musicbrainz"
    )]
    pub acoustid_api_key: Option<String>,

    /// Skip MusicBrainz/AcoustID lookups entirely.
    ///
    /// When set, audio file tags are never written. Files are still renamed
    /// and copied/moved using whatever tags are already embedded in the file.
    /// Files with no embedded tags are routed to the unknown directory as
    /// usual. The --acoustid-api-key is not required when this flag is set.
    #[arg(long)]
    pub no_musicbrainz: bool,

    /// Destination directory for audio files that have no MusicBrainz match and no embedded tags.
    ///
    /// Defaults to an `unknown/` subdirectory within the output root.
    /// Files placed here are never renamed — the original filename is preserved.
    #[arg(long, value_name = "DIR")]
    pub unknown_dir: Option<PathBuf>,

    /// Maximum width for embedded cover art in pixels.
    /// Larger images are scaled down, maintaining aspect ratio.
    #[arg(long, value_name = "PIXELS")]
    pub max_image_width: Option<u32>,

    /// Maximum height for embedded cover art in pixels.
    /// Larger images are scaled down, maintaining aspect ratio.
    #[arg(long, value_name = "PIXELS")]
    pub max_image_height: Option<u32>,
}
