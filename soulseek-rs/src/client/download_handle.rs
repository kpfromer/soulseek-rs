use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;

use crate::types::DownloadStatus;

const DEFAULT_PROGRESS_TIMEOUT: Duration = Duration::from_mins(10);

/// Handle returned by [`Client::download`] for receiving progress and cancelling a download.
pub struct DownloadHandle {
    receiver: UnboundedReceiver<DownloadStatus>,
    cancel: Arc<AtomicBool>,
    progress_timeout: Option<Duration>,
}

impl DownloadHandle {
    pub(super) fn new(
        receiver: UnboundedReceiver<DownloadStatus>,
        cancel: Arc<AtomicBool>,
        progress_timeout: Option<Duration>,
    ) -> Self {
        Self {
            receiver,
            cancel,
            progress_timeout,
        }
    }

    /// Signal the download to cancel. The next [`DownloadStatus::Cancelled`] update will arrive
    /// shortly after via [`recv`](Self::recv).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Receive the next status update, or `None` if the channel is closed.
    pub async fn recv(&mut self) -> Option<DownloadStatus> {
        let progress_timeout = self.progress_timeout.unwrap_or(DEFAULT_PROGRESS_TIMEOUT);
        tokio::select! {
            result = self.receiver.recv() => {
                result
            }
            _ = sleep(progress_timeout) => {
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
