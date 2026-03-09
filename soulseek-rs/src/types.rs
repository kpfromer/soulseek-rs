use std::collections::HashMap;

use tokio::sync::mpsc::UnboundedSender;

use crate::token::{DownloadToken, SearchToken};
use crate::{error::Result, message::Message, path::SoulseekPath, utils::zlib::deflate};

#[derive(Debug, Clone, Default)]
pub struct FileAttributes {
    pub bitrate: Option<u32>,     // kbps (code 0)
    pub duration: Option<u32>,    // seconds (code 1)
    pub vbr: Option<bool>,        // variable bit rate flag (code 2)
    pub sample_rate: Option<u32>, // Hz (code 4)
    pub bit_depth: Option<u32>,   // bits (code 5)
}

impl FileAttributes {
    fn from_map(map: HashMap<u32, u32>) -> Self {
        Self {
            bitrate: map.get(&0).copied(),
            duration: map.get(&1).copied(),
            vbr: map.get(&2).map(|&v| v != 0),
            sample_rate: map.get(&4).copied(),
            bit_depth: map.get(&5).copied(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct File {
    pub username: String,
    pub name: SoulseekPath,
    pub size: u64,
    pub attributes: FileAttributes,
}
pub struct UploadFailed {
    pub filename: SoulseekPath,
}
impl UploadFailed {
    pub fn new_from_message(message: &mut Message) -> Self {
        let filename = SoulseekPath::from_wire(message.read_string());

        Self { filename }
    }
}
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SearchResult {
    pub token: SearchToken,
    pub files: Vec<File>,
    pub slots: u8,
    pub speed: u32,
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct Search {
    pub token: SearchToken,
    pub results: Vec<SearchResult>,
}

impl SearchResult {
    pub fn new_from_message(message: &mut Message) -> Result<Self> {
        let pointer = message.get_pointer();
        let size = message.get_size();
        let data: Vec<u8> = message.get_slice(pointer, size);
        let deflated = deflate(&data)?;
        let mut message = Message::new_with_data(deflated);

        let username = message.read_string();
        let token = SearchToken(message.read_int32());
        let n_files = message.read_int32();
        let mut files: Vec<File> = Vec::new();
        for _ in 0..n_files {
            message.read_int8();
            let name = message.read_string();
            let size = message.read_int64();
            message.read_string();
            let n_attribs = message.read_int32();
            let mut attribs: HashMap<u32, u32> = HashMap::new();

            for _ in 0..n_attribs {
                attribs.insert(message.read_int32(), message.read_int32());
            }
            files.push(File {
                username: username.clone(),
                name: SoulseekPath::from_wire(name),
                size,
                attributes: FileAttributes::from_map(attribs),
            });
        }
        let slots = message.read_int8();
        let speed = message.read_int32();

        Ok(Self {
            token,
            files,
            slots,
            speed,
            username,
        })
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Transfer {
    pub direction: u32,
    pub token: DownloadToken,
    pub filename: SoulseekPath,
    pub size: u64,
}
#[derive(Debug, Clone)]
pub struct Download {
    pub username: String,
    pub filename: SoulseekPath,
    pub token: DownloadToken,
    pub size: u64,
    pub download_directory: String,
    pub status: DownloadStatus,
    pub sender: UnboundedSender<DownloadStatus>,
}

impl Download {
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status,
            DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::TimedOut
        )
    }

    pub fn bytes_downloaded(&self) -> u64 {
        match &self.status {
            DownloadStatus::InProgress {
                bytes_downloaded, ..
            } => *bytes_downloaded,
            DownloadStatus::Completed => self.size,
            _ => 0,
        }
    }

    pub fn speed_bytes_per_sec(&self) -> f64 {
        match &self.status {
            DownloadStatus::InProgress {
                speed_bytes_per_sec,
                ..
            } => *speed_bytes_per_sec,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DownloadStatus {
    Queued,
    InProgress {
        bytes_downloaded: u64,
        total_bytes: u64,
        speed_bytes_per_sec: f64,
    },
    Completed,
    Failed,
    TimedOut,
}
impl Transfer {
    pub fn new_from_message(message: &mut Message) -> Self {
        let direction = message.read_int32();
        let token = DownloadToken(message.read_int32());
        let filename = SoulseekPath::from_wire(message.read_string());
        let size = message.read_int64();

        Self {
            direction,
            token,
            filename,
            size,
        }
    }
}
