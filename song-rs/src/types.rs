use std::{collections::HashSet, fmt};

use soulseek_rs::SoulseekPath;

#[derive(Debug, Clone, Default)]
pub struct SongQuery {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Target duration. `None` skips the duration term in the ranker (the
    /// score falls back to a neutral 0.5).
    pub duration_secs: Option<u32>,
    /// Filter results below this bitrate (kbps). `None` (default) accepts
    /// any bitrate. Lossless files (FLAC/ALAC/WAV/AIFF) are always kept.
    pub min_bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SongResult {
    pub username: String,
    pub filename: SoulseekPath,
    pub file_type: FileType,
    pub size: u64,
    pub bitrate: Option<u32>,
    pub duration: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub vbr: Option<bool>,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileType {
    Mp3,
    Flac,
    Wav,
    Aac,
    M4a,
    Ogg,
    Alac,
    Aiff,
    Opus,
    Other(String),
}

impl FileType {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "mp3" => Self::Mp3,
            "flac" => Self::Flac,
            "wav" => Self::Wav,
            "aac" => Self::Aac,
            "m4a" => Self::M4a,
            "ogg" => Self::Ogg,
            "alac" => Self::Alac,
            "aiff" | "aif" => Self::Aiff,
            "opus" => Self::Opus,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_lossless(&self) -> bool {
        matches!(self, Self::Flac | Self::Alac | Self::Wav | Self::Aiff)
    }
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mp3 => write!(f, "mp3"),
            Self::Flac => write!(f, "flac"),
            Self::Wav => write!(f, "wav"),
            Self::Aac => write!(f, "aac"),
            Self::M4a => write!(f, "m4a"),
            Self::Ogg => write!(f, "ogg"),
            Self::Alac => write!(f, "alac"),
            Self::Aiff => write!(f, "aiff"),
            Self::Opus => write!(f, "opus"),
            Self::Other(ext) => write!(f, "{}", ext),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WantedFileTypes {
    All,
    Specific(HashSet<FileType>),
    /// Ordered priority list. Earlier entries score higher in the ranker
    /// (smooth nudge, not a hard sort) so callers' format preference is
    /// honoured without a second post-pass. `is_compatible` accepts any
    /// type in the list; a type not in the list is rejected.
    Ordered(Vec<FileType>),
}

impl WantedFileTypes {
    pub fn lossless() -> Self {
        Self::Specific(HashSet::from([
            FileType::Flac,
            FileType::Alac,
            FileType::Wav,
            FileType::Aiff,
        ]))
    }
    pub fn lossy() -> Self {
        Self::Specific(HashSet::from([
            FileType::Mp3,
            FileType::Aac,
            FileType::M4a,
            FileType::Ogg,
            FileType::Opus,
        ]))
    }
    pub fn all() -> Self {
        Self::All
    }
    pub fn specific(file_types: HashSet<FileType>) -> Self {
        Self::Specific(file_types)
    }
    pub fn ordered(file_types: Vec<FileType>) -> Self {
        Self::Ordered(file_types)
    }

    pub(crate) fn is_compatible(&self, file_type: &FileType) -> bool {
        match self {
            Self::All => true,
            Self::Specific(file_types) => file_types.contains(file_type),
            Self::Ordered(file_types) => file_types.contains(file_type),
        }
    }

    /// Position of `file_type` in an `Ordered` list (0 = highest priority),
    /// or `None` for `All` / `Specific` / not-in-list. The ranker uses this
    /// for a small score nudge so user-specified format priority breaks ties.
    pub(crate) fn priority_index(&self, file_type: &FileType) -> Option<usize> {
        match self {
            Self::Ordered(types) => types.iter().position(|t| t == file_type),
            _ => None,
        }
    }
}
