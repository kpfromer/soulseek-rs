use crate::Error;
use backon::{ExponentialBuilder, Retryable};
use musicbrainz_rs::Fetch;
use musicbrainz_rs::entity::recording::Recording;
use musicbrainz_rs::entity::release::Release;

pub async fn fetch_recording(id: &str) -> Result<Recording, Error> {
    (|| async {
        Recording::fetch()
            .id(id)
            .with_artists()
            .with_releases()
            .execute()
            .await
    })
    .retry(ExponentialBuilder::default())
    .await
    .map_err(|e| Error::MusicBrainz(e.to_string()))
}

pub async fn fetch_release(id: &str) -> Result<Release, Error> {
    (|| async {
        Release::fetch()
            .id(id)
            .with_release_groups()
            .with_artists()
            .with_recordings()
            .with_media()
            .execute()
            .await
    })
    .retry(ExponentialBuilder::default())
    .await
    .map_err(|e| Error::MusicBrainz(e.to_string()))
}
