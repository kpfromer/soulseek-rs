use std::fmt;

use soulseek_rs::SoulseekPath;

#[derive(Debug, Clone)]
pub struct SongQuery {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: u32,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
