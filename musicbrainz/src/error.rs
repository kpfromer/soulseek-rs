use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("fpcalc not found in PATH — install Chromaprint and ensure fpcalc is available")]
    FpcalcNotFound,

    #[error("fpcalc failed: {0}")]
    FpcalcFailed(#[from] std::io::Error),

    #[error("fpcalc output was not valid UTF-8: {0}")]
    FpcalcOutputEncoding(#[from] std::string::FromUtf8Error),

    #[error("fpcalc output missing DURATION field")]
    FpcalcMissingDuration,

    #[error("fpcalc DURATION value could not be parsed: {0}")]
    FpcalcBadDuration(#[from] std::num::ParseIntError),

    #[error("fpcalc output missing FINGERPRINT field")]
    FpcalcMissingFingerprint,

    #[error("failed to compute SHA-256: {0}")]
    Sha256(String),

    #[error("AcoustID request failed: {0}")]
    AcoustId(String),

    #[error("AcoustID returned no results")]
    AcoustIdNoResults,

    #[error("AcoustID results contained no recordings")]
    AcoustIdNoRecordings,

    #[error("MusicBrainz request failed: {0}")]
    MusicBrainz(String),

    #[error("MusicBrainz recording has no releases")]
    MusicBrainzNoReleases,

    #[error("track number not found in MusicBrainz release")]
    MusicBrainzNoTrackNumber,
}
