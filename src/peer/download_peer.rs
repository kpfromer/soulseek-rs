use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::message::server::MessageFactory;
use crate::token::DownloadToken;
use crate::trace;
use crate::types::{Download, DownloadStatus};

const START_DOWNLOAD: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
const READ_BUFFER_SIZE: usize = 8192;
const PROGRESS_UPDATE_CHUNKS: usize = 15; // ~120KB (15 * 8192 bytes)

#[derive(Debug)]
pub enum DownloadError {
    ConnectionFailed(io::Error),
    InvalidAddress(String),
    HandshakeFailed(io::Error),
    StreamReadError(io::Error),
    StreamWriteError(io::Error),
    TokenNotFound(DownloadToken),
    DownloadInfoMissing,
    FileWriteError(io::Error),
    PathResolutionError(String),
    InvalidTokenBytes,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionFailed(e) => write!(f, "Connection failed: {}", e),
            Self::InvalidAddress(addr) => write!(f, "Invalid address: {}", addr),
            Self::HandshakeFailed(e) => write!(f, "Handshake failed: {}", e),
            Self::StreamReadError(e) => write!(f, "Stream read error: {}", e),
            Self::StreamWriteError(e) => write!(f, "Stream write error: {}", e),
            Self::TokenNotFound(token) => write!(f, "Token not found: {}", token),
            Self::DownloadInfoMissing => write!(f, "Download info missing"),
            Self::FileWriteError(e) => write!(f, "File write error: {}", e),
            Self::PathResolutionError(msg) => write!(f, "Path resolution error: {}", msg),
            Self::InvalidTokenBytes => write!(f, "Invalid token bytes received"),
        }
    }
}

impl std::error::Error for DownloadError {}

impl From<io::Error> for DownloadError {
    fn from(error: io::Error) -> Self {
        Self::StreamReadError(error)
    }
}

struct FileManager;

impl FileManager {
    fn expand_path(path: &str) -> PathBuf {
        if let Some(stripped) = path.strip_prefix('~') {
            if let Ok(home) = env::var("HOME") {
                PathBuf::from(home).join(stripped.trim_start_matches('/'))
            } else {
                PathBuf::from(path)
            }
        } else {
            PathBuf::from(path)
        }
    }
}

pub struct DownloadPeer {
    username: String,
    host: String,
    port: u32,
    #[allow(dead_code)]
    own_username: String,
    /// Token sent in the pierce-firewall handshake (pierce path only).
    token: u32,
}

impl DownloadPeer {
    #[must_use]
    pub fn new(
        username: String,
        host: String,
        port: u32,
        token: u32,
        own_username: String,
    ) -> Self {
        Self {
            username,
            host,
            port,
            own_username,
            token,
        }
    }

    fn establish_connection(&self) -> Result<TcpStream, DownloadError> {
        let socket_address = format!("{}:{}", self.host, self.port)
            .to_socket_addrs()
            .map_err(DownloadError::ConnectionFailed)?
            .next()
            .ok_or_else(|| DownloadError::InvalidAddress(format!("{}:{}", self.host, self.port)))?;

        let stream = TcpStream::connect_timeout(&socket_address, Duration::from_secs(20))
            .map_err(DownloadError::ConnectionFailed)?;

        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(DownloadError::ConnectionFailed)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(DownloadError::ConnectionFailed)?;
        stream
            .set_nodelay(true)
            .map_err(DownloadError::ConnectionFailed)?;

        Ok(stream)
    }

    fn open_output_file(path: &str) -> Result<io::BufWriter<fs::File>, DownloadError> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).map_err(DownloadError::FileWriteError)?;
        }
        let f = fs::File::create(path).map_err(DownloadError::FileWriteError)?;
        Ok(io::BufWriter::new(f))
    }

    fn resolve_download_path(download: &Download) -> Result<String, DownloadError> {
        let download_directory = &download.download_directory;
        let mut expanded_path = FileManager::expand_path(download_directory);

        if !expanded_path.is_dir() {
            expanded_path = expanded_path
                .parent()
                .ok_or_else(|| {
                    DownloadError::PathResolutionError(format!(
                        "Cannot resolve parent directory for: {}",
                        expanded_path.display()
                    ))
                })?
                .to_path_buf();
        }

        let final_path = expanded_path.join(download.filename.filename());

        final_path
            .to_str()
            .ok_or_else(|| {
                DownloadError::PathResolutionError(format!(
                    "Path contains invalid UTF-8: {}",
                    final_path.display()
                ))
            })
            .map(String::from)
    }

    /// Download a file where the download info is known upfront (direct connection).
    ///
    /// Sends `START_DOWNLOAD`, then streams data to disk. Progress is reported
    /// via `download.sender` — no shared context lock is held during I/O.
    pub fn download_direct(
        self,
        download: Download,
        stream: Option<TcpStream>,
    ) -> Result<(Download, String), DownloadError> {
        trace!(
            "[download_peer:{}] download_direct, stream present: {}",
            self.username,
            stream.is_some()
        );

        let mut stream = match stream {
            Some(s) => s,
            None => self.establish_connection()?,
        };

        let path = Self::resolve_download_path(&download)?;
        let mut writer = Self::open_output_file(&path)?;

        stream
            .write_all(&START_DOWNLOAD)
            .map_err(DownloadError::StreamWriteError)?;

        trace!("[download_peer:{}] sent START_DOWNLOAD", self.username);

        let total_bytes = Self::read_stream(
            &self.username,
            &mut stream,
            &mut writer,
            &download,
        )?;

        writer.flush().map_err(DownloadError::FileWriteError)?;

        trace!(
            "[download_peer:{}] download_direct complete: {} bytes → {}",
            self.username, total_bytes, path
        );

        Ok((download, path))
    }

    /// Download a file where the download info is resolved from the first bytes of the stream
    /// (pierce-firewall path).
    ///
    /// Sends a pierce-firewall handshake, reads a 4-byte token from the peer,
    /// calls `resolve_download` to look up the corresponding `Download`,
    /// then streams data to disk.
    pub fn download_pierced(
        self,
        resolve_download: impl Fn(DownloadToken) -> Option<Download>,
        stream: Option<TcpStream>,
    ) -> Result<(Download, String), DownloadError> {
        trace!(
            "[download_peer:{}] download_pierced, stream present: {}",
            self.username,
            stream.is_some()
        );

        let mut stream = match stream {
            Some(s) => s,
            None => self.establish_connection()?,
        };

        // Send pierce-firewall message so the peer can identify this connection.
        let message = MessageFactory::build_pierce_firewall_message(self.token);
        stream
            .write_all(&message.get_buffer())
            .map_err(DownloadError::HandshakeFailed)?;
        stream.flush().map_err(DownloadError::HandshakeFailed)?;

        trace!(
            "[download_peer:{}] sent pierce firewall token: {}",
            self.username, self.token
        );

        // Read the first chunk — first 4 bytes are the download token.
        let mut first_buf = [0u8; READ_BUFFER_SIZE];
        let first_read = stream
            .read(&mut first_buf)
            .map_err(DownloadError::StreamReadError)?;

        if first_read < 4 {
            return Err(DownloadError::InvalidTokenBytes);
        }

        let token = DownloadToken(u32::from_le_bytes(
            first_buf[..4].try_into().map_err(|_| DownloadError::InvalidTokenBytes)?,
        ));

        trace!(
            "[download_peer:{}] resolved download token: {}",
            self.username, token
        );

        let download = resolve_download(token).ok_or(DownloadError::TokenNotFound(token))?;

        stream
            .write_all(&START_DOWNLOAD)
            .map_err(DownloadError::StreamWriteError)?;

        let path = Self::resolve_download_path(&download)?;
        let mut writer = Self::open_output_file(&path)?;

        // Any bytes after the 4-byte token in the first chunk are the start of file data.
        let mut total_bytes: usize = 0;
        if first_read > 4 {
            let initial_data = &first_buf[4..first_read];
            writer
                .write_all(initial_data)
                .map_err(DownloadError::FileWriteError)?;
            total_bytes += initial_data.len();

            // Send progress if the initial chunk is already large enough.
            if total_bytes >= PROGRESS_UPDATE_CHUNKS * READ_BUFFER_SIZE {
                let _ = download.sender.send(DownloadStatus::InProgress {
                    bytes_downloaded: total_bytes as u64,
                    total_bytes: download.size,
                    speed_bytes_per_sec: 0.0,
                });
            }
        }

        total_bytes += Self::read_stream_with_offset(
            &self.username,
            &mut stream,
            &mut writer,
            &download,
            total_bytes,
        )?;

        writer.flush().map_err(DownloadError::FileWriteError)?;

        trace!(
            "[download_peer:{}] download_pierced complete: {} bytes → {}",
            self.username, total_bytes, path
        );

        Ok((download, path))
    }

    /// Inner loop: read data from `stream`, write to `writer`, send progress via `download.sender`.
    fn read_stream(
        username: &str,
        stream: &mut TcpStream,
        writer: &mut io::BufWriter<fs::File>,
        download: &Download,
    ) -> Result<usize, DownloadError> {
        Self::read_stream_with_offset(username, stream, writer, download, 0)
    }

    fn read_stream_with_offset(
        username: &str,
        stream: &mut TcpStream,
        writer: &mut io::BufWriter<fs::File>,
        download: &Download,
        initial_bytes: usize,
    ) -> Result<usize, DownloadError> {
        let mut total_bytes = initial_bytes;
        let mut chunk_counter = 0usize;
        let mut last_update_time = Instant::now();
        let mut read_buffer = [0u8; READ_BUFFER_SIZE];

        trace!("[download_peer:{}] reading stream data", username);

        loop {
            match stream.read(&mut read_buffer) {
                Ok(0) => {
                    trace!(
                        "[download_peer:{}] connection closed by peer, {} bytes read",
                        username, total_bytes
                    );
                    break;
                }
                Ok(bytes_read) => {
                    writer
                        .write_all(&read_buffer[..bytes_read])
                        .map_err(DownloadError::FileWriteError)?;
                    total_bytes += bytes_read;
                    chunk_counter += 1;

                    if chunk_counter % PROGRESS_UPDATE_CHUNKS == 0 {
                        let elapsed = last_update_time.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 {
                            (PROGRESS_UPDATE_CHUNKS * READ_BUFFER_SIZE) as f64 / elapsed
                        } else {
                            0.0
                        };
                        let _ = download.sender.send(DownloadStatus::InProgress {
                            bytes_downloaded: total_bytes as u64,
                            total_bytes: download.size,
                            speed_bytes_per_sec: speed,
                        });
                        last_update_time = Instant::now();
                    }

                    if total_bytes >= download.size as usize {
                        break;
                    }
                }
                Err(e) => return Err(DownloadError::StreamReadError(e)),
            }
        }

        Ok(total_bytes - initial_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::DownloadPeer;

    #[test]
    fn test_establish_connection_invalid_address() {
        let download_peer = DownloadPeer::new(
            "user".to_string(),
            "invalid-host".to_string(),
            9999,
            123,
            "own_user".to_string(),
        );
        let result = download_peer.establish_connection();
        assert!(result.is_err());
    }
}
