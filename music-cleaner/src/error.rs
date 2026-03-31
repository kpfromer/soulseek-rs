use thiserror::Error;

/// Typed error variants for music-cleaner operations.
///
/// The processor uses `anyhow::Result` for flexibility, but this enum is
/// available for callers that need to match on specific failure modes.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum MusicCleanerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MusicBrainz lookup failed: {0}")]
    MusicBrainz(#[from] musicbrainz::Error),

    #[error("Cover Art Archive error: {0}")]
    CoverArt(#[from] cover_art_archive::CoverArtArchiveError),

    #[error("Audio tag error: {0}")]
    Tag(#[from] lofty::error::LoftyError),

    #[error("Template rendering error: {0}")]
    Template(String),

    #[error("Interactive prompt error: {0}")]
    Prompt(String),
}
