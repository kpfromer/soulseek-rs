use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::client::ClientContext;
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
    TokenNotFound(u32),
    DownloadInfoMissing(u32),
    FileWriteError(io::Error),
    PathResolutionError(String),
    InvalidTokenBytes,
    LockPoisoned,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionFailed(e) => write!(f, "Connection failed: {}", e),
            Self::InvalidAddress(addr) => {
                write!(f, "Invalid address: {}", addr)
            }
            Self::HandshakeFailed(e) => write!(f, "Handshake failed: {}", e),
            Self::StreamReadError(e) => write!(f, "Stream read error: {}", e),
            Self::StreamWriteError(e) => write!(f, "Stream write error: {}", e),
            Self::TokenNotFound(token) => {
                write!(f, "Token not found: {}", token)
            }
            Self::DownloadInfoMissing(token) => {
                write!(f, "Download info missing for token: {}", token)
            }
            Self::FileWriteError(e) => write!(f, "File write error: {}", e),
            Self::PathResolutionError(msg) => {
                write!(f, "Path resolution error: {}", msg)
            }
            Self::InvalidTokenBytes => {
                write!(f, "Invalid token bytes received")
            }
            Self::LockPoisoned => write!(f, "Lock poisoned"),
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
    token: u32,
    no_pierce: bool,
}

impl DownloadPeer {
    #[must_use]
    pub fn new(
        username: String,
        host: String,
        port: u32,
        token: u32,
        no_pierce: bool,
        own_username: String,
    ) -> Self {
        Self {
            username,
            host,
            port,
            own_username,
            token,
            no_pierce,
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

    fn perform_handshake(&self, stream: &mut TcpStream) -> Result<(), DownloadError> {
        trace!(
            "[download_peer:{}] performing handshake no_pierce: {}",
            self.username, self.no_pierce
        );

        if !self.no_pierce {
            let message = MessageFactory::build_pierce_firewall_message(self.token);
            stream
                .write_all(&message.get_buffer())
                .map_err(DownloadError::HandshakeFailed)?;
            trace!(
                "[download_peer:{}] sending pierce firewall message token: {}: {:?}",
                self.username,
                self.token,
                &message.get_buffer()
            );
            stream.flush().map_err(DownloadError::HandshakeFailed)?;
        }
        Ok(())
    }

    fn handle_pierce_firewall_response(
        &self,
        data: &[u8],
        stream: &mut TcpStream,
        client_context: &Arc<RwLock<ClientContext>>,
    ) -> Result<Download, DownloadError> {
        let token_bytes = data.get(0..4).ok_or(DownloadError::InvalidTokenBytes)?;
        let token_array: [u8; 4] = token_bytes
            .try_into()
            .map_err(|_| DownloadError::InvalidTokenBytes)?;
        let token = DownloadToken(u32::from_le_bytes(token_array));

        trace!(
            "[download_peer:{}] got token: {} from data chunk",
            self.username, token
        );

        stream
            .write_all(&START_DOWNLOAD)
            .map_err(DownloadError::StreamWriteError)?;

        let client_guard = client_context
            .read()
            .map_err(|_| DownloadError::LockPoisoned)?;
        let download_info = client_guard.get_download_by_token(token).cloned();
        drop(client_guard);

        download_info.ok_or(DownloadError::TokenNotFound(token.0))
    }

    fn open_output_file(path: &str) -> Result<io::BufWriter<fs::File>, DownloadError> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).map_err(DownloadError::FileWriteError)?;
        }
        let f = fs::File::create(path).map_err(DownloadError::FileWriteError)?;
        Ok(io::BufWriter::new(f))
    }

    fn read_download_stream(
        &self,
        stream: &mut TcpStream,
        client_context: &Arc<RwLock<ClientContext>>,
        mut download: Option<Download>,
    ) -> Result<(usize, Download, String), DownloadError> {
        let mut total_bytes: usize = 0;
        let mut received_handshake = false;
        let mut read_buffer = [0u8; READ_BUFFER_SIZE];
        let mut chunk_counter = 0;
        let mut last_update_time = Instant::now();

        trace!(
            "[download_peer:{}] Starting to read data from peer",
            self.username
        );

        if download.is_some() {
            stream
                .write_all(&START_DOWNLOAD)
                .map_err(DownloadError::StreamWriteError)?;
        }

        // Open file early if download target is already known (no_pierce=true path).
        let (mut writer, mut final_path) = if let Some(ref dl) = download {
            let path = self.resolve_download_path(dl)?;
            (Some(Self::open_output_file(&path)?), Some(path))
        } else {
            (None, None)
        };

        loop {
            match stream.read(&mut read_buffer) {
                Ok(0) => {
                    trace!(
                        "[download_peer:{}] connection closed by peer. bytes read: {}",
                        self.username, total_bytes
                    );
                    break;
                }
                Ok(bytes_read) => {
                    let data = &read_buffer[..bytes_read];

                    if !self.no_pierce && !received_handshake {
                        let new_download =
                            self.handle_pierce_firewall_response(data, stream, client_context)?;
                        trace!(
                            "[download_peer:{}] got download info for token: {} - filename: {}",
                            self.username, self.token, new_download.filename
                        );
                        // Now that we know the target, open the file.
                        let path = self.resolve_download_path(&new_download)?;
                        writer = Some(Self::open_output_file(&path)?);
                        final_path = Some(path);
                        download = Some(new_download);
                        received_handshake = true;

                        // Any bytes after the 4-byte token are the start of file data.
                        if data.len() > 4 {
                            let chunk = &data[4..];
                            writer
                                .as_mut()
                                .unwrap()
                                .write_all(chunk)
                                .map_err(DownloadError::FileWriteError)?;
                            total_bytes += chunk.len();
                            chunk_counter += 1;
                        }
                        continue;
                    }

                    writer
                        .as_mut()
                        .unwrap()
                        .write_all(data)
                        .map_err(DownloadError::FileWriteError)?;
                    total_bytes += bytes_read;
                    chunk_counter += 1;

                    if let Some(ref dl) = download
                        && chunk_counter % PROGRESS_UPDATE_CHUNKS == 0
                    {
                        let elapsed = last_update_time.elapsed().as_secs_f64();
                        let bytes_since_last_update = PROGRESS_UPDATE_CHUNKS * READ_BUFFER_SIZE;
                        let speed = if elapsed > 0.0 {
                            bytes_since_last_update as f64 / elapsed
                        } else {
                            0.0
                        };

                        let status = DownloadStatus::InProgress {
                            bytes_downloaded: total_bytes as u64,
                            total_bytes: dl.size,
                            speed_bytes_per_sec: speed,
                        };
                        let _ = dl.sender.send(status.clone());
                        client_context
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .update_download_with_status(dl.token, status);

                        last_update_time = Instant::now();
                    }

                    let expected_size = download
                        .as_ref()
                        .ok_or(DownloadError::DownloadInfoMissing(self.token))?
                        .size as usize;

                    if total_bytes >= expected_size {
                        break;
                    }
                }
                Err(e) => {
                    return Err(DownloadError::StreamReadError(e));
                }
            }
        }

        if let Some(mut w) = writer {
            w.flush().map_err(DownloadError::FileWriteError)?;
        }

        trace!(
            "[download_peer:{}] finished reading data from peer",
            self.username
        );

        let download = download.ok_or(DownloadError::DownloadInfoMissing(self.token))?;
        let final_path = final_path.ok_or(DownloadError::DownloadInfoMissing(self.token))?;
        Ok((total_bytes, download, final_path))
    }

    fn resolve_download_path(&self, download: &Download) -> Result<String, DownloadError> {
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

    pub fn download_file(
        self,
        client_context: Arc<RwLock<ClientContext>>,
        download: Option<Download>,
        stream: Option<TcpStream>,
    ) -> Result<(Download, String), DownloadError> {
        trace!(
            "[download_peer:{}] download_file: download is present?: {:?}, stream is present?: {:?}, no_pierce: {}",
            self.username,
            download.is_some(),
            stream.is_some(),
            self.no_pierce
        );

        if let Some(ref dl) = download {
            let _ = dl.sender.send(DownloadStatus::Queued);
            client_context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .update_download_with_status(dl.token, DownloadStatus::Queued);
        }

        let mut stream = match stream {
            Some(s) => s,
            None => self.establish_connection()?,
        };

        trace!("[download_peer:{}] connected", self.username);

        self.perform_handshake(&mut stream)?;
        trace!("[download_peer:{}] handshake completed", self.username);

        let (total_bytes, download, final_path) =
            self.read_download_stream(&mut stream, &client_context, download)?;

        trace!(
            "[download_peer:{}] download completed successfully: {} bytes, saved to: {}",
            self.username, total_bytes, final_path
        );

        Ok((download, final_path))
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
            false,
            "own_user".to_string(),
        );
        let result = download_peer.establish_connection();
        assert!(result.is_err());
    }
}
