#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("soulseek error: {0}")]
    Soulseek(#[from] soulseek_rs::SoulseekRs),
    #[error("no results found for query")]
    NoResults,
    #[error("download failed")]
    DownloadFailed,
}
