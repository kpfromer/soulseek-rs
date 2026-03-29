use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::types::DownloadStatus;

/// Handle returned by [`Client::download`] for receiving progress and cancelling a download.
pub struct DownloadHandle {
    receiver: UnboundedReceiver<DownloadStatus>,
    cancel: Arc<AtomicBool>,
}

impl DownloadHandle {
    pub(super) fn new(receiver: UnboundedReceiver<DownloadStatus>, cancel: Arc<AtomicBool>) -> Self {
        Self { receiver, cancel }
    }

    /// Signal the download to cancel. The next [`DownloadStatus::Cancelled`] update will arrive
    /// shortly after via [`recv`](Self::recv).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Receive the next status update, or `None` if the channel is closed.
    pub async fn recv(&mut self) -> Option<DownloadStatus> {
        self.receiver.recv().await
    }

    /// Non-blocking receive — returns `None` if no update is available yet.
    pub fn try_recv(&mut self) -> Option<DownloadStatus> {
        self.receiver.try_recv().ok()
    }
}
