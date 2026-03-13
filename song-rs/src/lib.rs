#[macro_use]
pub mod utils;

pub mod error;
mod parser;
mod ranker;
pub mod types;

pub use error::Error;
pub use types::{FileType, SongQuery, SongResult, WantedFileTypes};

pub use soulseek_rs::types::{Download, DownloadStatus};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

pub struct Client {
    inner: soulseek_rs::Client,
}

impl Client {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        let username = username.into();
        let password = password.into();
        Self {
            inner: soulseek_rs::Client::new(username, password),
        }
    }

    async fn connect(&mut self) -> Result<(), Error> {
        self.inner.connect().await?;
        Ok(())
    }

    /// Search Soulseek and return ranked results for the given query.
    pub async fn search(
        &mut self,
        query: &SongQuery,
        timeout: Duration,
        wanted_file_types: &WantedFileTypes,
    ) -> Result<Vec<SongResult>, Error> {
        info!("search: title={}, artist={}", query.title, query.artist);
        self.connect().await?;

        let text_query = format!("{} {}", query.title, query.artist);
        let search_results: Vec<soulseek_rs::SearchResult> =
            self.inner.search(&text_query, timeout).await?;

        let files: Vec<soulseek_rs::File> =
            search_results.into_iter().flat_map(|sr| sr.files).collect();

        let results = ranker::rank_results(query, &files, wanted_file_types);
        debug!("search returned {} ranked results", results.len());
        Ok(results)
    }

    /// Initiate a download for a specific result.
    pub async fn download(
        &mut self,
        result: &SongResult,
        // TODO: use a path instead of a string
        download_dir: impl Into<String>,
    ) -> Result<(Download, UnboundedReceiver<DownloadStatus>), Error> {
        info!(
            "download: filename={}, username={}",
            result.filename.as_str(),
            result.username
        );
        self.connect().await?;

        let (dl, rx) = self.inner.download(
            result.filename.clone(),
            result.username.clone(),
            result.size,
            // TODO: use a path instead of a string
            download_dir.into(),
        )?;
        Ok((dl, rx))
    }

    /// Search and immediately download the best matching result.
    pub async fn download_best(
        &mut self,
        query: &SongQuery,
        timeout: Duration,
        // TODO: use a path instead of a string
        download_dir: impl Into<String>,
        wanted_file_types: &WantedFileTypes,
    ) -> Result<(SongResult, Download, UnboundedReceiver<DownloadStatus>), Error> {
        self.connect().await?;

        let results = self.search(query, timeout, wanted_file_types).await?;
        let best = results.into_iter().next().ok_or(Error::NoResults)?;
        info!(
            "download_best: chosen filename={}, score={}",
            best.filename.as_str(),
            best.score
        );
        let (dl, rx) = self.download(&best, download_dir).await?;
        Ok((best, dl, rx))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}
