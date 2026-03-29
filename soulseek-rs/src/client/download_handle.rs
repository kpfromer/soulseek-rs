use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;

use crate::types::DownloadStatus;

const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_mins(10);

/// Handle returned by [`Client::download`] for receiving progress and cancelling a download.
///
/// Dropping this handle automatically cancels the download.
pub struct DownloadHandle {
    receiver: UnboundedReceiver<DownloadStatus>,
    cancel: Arc<AtomicBool>,
    /// Passed to the download loop — cancels if no bytes arrive within this window.
    #[allow(dead_code)]
    progress_timeout: Option<Duration>,
    /// Used only by [`recv`](Self::recv) — cancels the wait if no status update arrives.
    recv_timeout: Option<Duration>,
}

impl DownloadHandle {
    pub(super) fn new(
        receiver: UnboundedReceiver<DownloadStatus>,
        cancel: Arc<AtomicBool>,
        progress_timeout: Option<Duration>,
        recv_timeout: Option<Duration>,
    ) -> Self {
        Self {
            receiver,
            cancel,
            progress_timeout,
            recv_timeout,
        }
    }

    /// Signal the download to cancel. The next [`DownloadStatus::Cancelled`] update will arrive
    /// shortly after via [`recv`](Self::recv).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Receive the next status update, or `None` if the channel is closed.
    ///
    /// Times out after `recv_timeout` (default 10 minutes) and returns
    /// `Some(DownloadStatus::Cancelled)` if no update arrives in time.
    pub async fn recv(&mut self) -> Option<DownloadStatus> {
        let recv_timeout = self.recv_timeout.unwrap_or(DEFAULT_RECV_TIMEOUT);
        tokio::select! {
            result = self.receiver.recv() => {
                result
            }
            _ = sleep(recv_timeout) => {
                self.cancel();
                Some(DownloadStatus::Cancelled)
            }
        }
    }

    /// Non-blocking receive — returns `None` if no update is available yet.
    pub fn try_recv(&mut self) -> Option<DownloadStatus> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for DownloadHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
