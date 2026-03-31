use reqwest::redirect::Policy;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum CoverArtArchiveError {
    #[error("invalid MBID")]
    InvalidMBID,
    #[error("not found")]
    NotFound,
    #[error("rate limit exceeded")]
    RateLimitExceeded,
    #[error("unexpected status: {0}")]
    UnexpectedStatus(u16),
    #[error("unexpected image status: {0}")]
    UnexpectedImageStatus(u16),
    #[error("network error")]
    NetworkError,
    #[error("unexpected error")]
    UnexpectedError,
}

#[derive(Debug)]
pub struct AlbumArt {
    pub data: Vec<u8>,
    pub extension: String,
}

pub async fn get_album_art(release_mbid: &str) -> Result<AlbumArt, CoverArtArchiveError> {
    let url = Url::parse("https://coverartarchive.org/release/")
        .map_err(|_| CoverArtArchiveError::UnexpectedError)?
        .join(&format!("{}/front", release_mbid))
        .map_err(|_| CoverArtArchiveError::UnexpectedError)?;

    let response = reqwest::Client::builder()
        // Don't follow redirects, we want to handle that ourselves.
        .redirect(Policy::none())
        .build()
        .map_err(|_| CoverArtArchiveError::UnexpectedError)?
        .get(url)
        .send()
        .await
        .map_err(|_| CoverArtArchiveError::NetworkError)?;

    // 307 if the community have decided upon a "front" image for this release.
    // 400 if {mbid} cannot be parsed as a valid UUID.
    // 404 if there is either no release with this MBID, or the community have not chosen an image to represent the front of a release.
    // 405 if the request method is not GET or HEAD.
    // 503 if the user has exceeded their rate limit.
    match response.status().as_u16() {
        307 => {
            let image_url = response
                .headers()
                .get("Location")
                .ok_or(CoverArtArchiveError::UnexpectedError)?
                .to_str()
                .map_err(|_| CoverArtArchiveError::UnexpectedError)?;

            let image_response = reqwest::get(image_url)
                .await
                .map_err(|_| CoverArtArchiveError::NetworkError)?;

            match image_response.status().as_u16() {
                200 => {
                    let extension = image_url
                        .split('.')
                        .last()
                        .ok_or(CoverArtArchiveError::UnexpectedError)?;

                    let bytes = image_response
                        .bytes()
                        .await
                        .map_err(|_| CoverArtArchiveError::UnexpectedError)?
                        .to_vec();
                    Ok(AlbumArt {
                        data: bytes,
                        extension: extension.to_string(),
                    })
                }
                404 => Err(CoverArtArchiveError::NotFound),
                _ => Err(CoverArtArchiveError::UnexpectedImageStatus(
                    image_response.status().as_u16(),
                )),
            }
        }
        400 => Err(CoverArtArchiveError::InvalidMBID),
        404 => Err(CoverArtArchiveError::NotFound),
        503 => Err(CoverArtArchiveError::RateLimitExceeded),
        _ => Err(CoverArtArchiveError::UnexpectedStatus(
            response.status().as_u16(),
        )),
    }
}
