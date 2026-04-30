use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;

use crate::client::ClientOperation;
use crate::token::DownloadToken;
use crate::types::DownloadStatus;

const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(180);

/// Outcome of [`DownloadHandle::recv_typed`]: distinguishes a peer/channel
/// state from a caller-side `recv_timeout` firing. The legacy
/// [`DownloadHandle::recv`] collapses `Timeout` and peer-cancel into one
/// `Some(DownloadStatus::Cancelled)`.
#[derive(Debug, Clone)]
pub enum RecvOutcome {
    /// A status update arrived from the download task.
    Status(DownloadStatus),
    /// The channel closed (download task exited without sending).
    Closed,
    /// `recv_timeout` fired before any status update arrived. The handle
    /// has already signalled cancel internally.
    Timeout,
}

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
    /// Sender to the worker for cancellation cleanup (None if disconnected at download time).
    op_tx: Option<UnboundedSender<ClientOperation>>,
    token: DownloadToken,
}

impl DownloadHandle {
    pub(super) fn new(
        receiver: UnboundedReceiver<DownloadStatus>,
        cancel: Arc<AtomicBool>,
        progress_timeout: Option<Duration>,
        recv_timeout: Option<Duration>,
        op_tx: Option<UnboundedSender<ClientOperation>>,
        token: DownloadToken,
    ) -> Self {
        Self {
            receiver,
            cancel,
            progress_timeout,
            recv_timeout,
            op_tx,
            token,
        }
    }

    /// Signal the download to cancel. The next [`DownloadStatus::Cancelled`] update will arrive
    /// shortly after via [`recv`](Self::recv).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Receive the next status update, or `None` if the channel is closed.
    ///
    /// Times out after `recv_timeout` (default 3 minutes) and returns
    /// `Some(DownloadStatus::Cancelled)` if no update arrives in time. This
    /// makes the timeout case indistinguishable from a peer-side cancel —
    /// callers that need to tell them apart should use [`Self::recv_typed`].
    pub async fn recv(&mut self) -> Option<DownloadStatus> {
        match self.recv_typed().await {
            RecvOutcome::Status(s) => Some(s),
            RecvOutcome::Closed => None,
            RecvOutcome::Timeout => Some(DownloadStatus::Cancelled),
        }
    }

    /// Like [`Self::recv`] but distinguishes a `recv_timeout`-fired cancel
    /// from peer-driven `Cancelled` and from channel close. Callers that
    /// branch on cause (e.g. for retry policies) should prefer this.
    pub async fn recv_typed(&mut self) -> RecvOutcome {
        let recv_timeout = self.recv_timeout.unwrap_or(DEFAULT_RECV_TIMEOUT);
        tokio::select! {
            result = self.receiver.recv() => {
                match result {
                    Some(s) => RecvOutcome::Status(s),
                    None => RecvOutcome::Closed,
                }
            }
            _ = sleep(recv_timeout) => {
                self.cancel();
                RecvOutcome::Timeout
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
        if let Some(ref tx) = self.op_tx {
            let _ = tx.send(ClientOperation::CancelDownload(self.token));
        }
    }
}
