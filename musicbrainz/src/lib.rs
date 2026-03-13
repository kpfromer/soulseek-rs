mod acoustid;
mod chromaprint;
mod error;
mod file_hash;
mod musicbrainz;

pub use error::Error;

use std::path::Path;
use std::time::Duration;

/// An artist with an optional MusicBrainz ID.
#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    pub musicbrainz_id: Option<String>,
}

/// Metadata resolved for a track via Chromaprint + AcoustID + MusicBrainz.
#[derive(Debug)]
pub struct TrackMetadata {
    pub sha256: String,
    pub track_title: String,
    pub track_number: i32,
    pub duration: Duration,
    pub track_musicbrainz_id: Option<String>,
    pub album_title: String,
    pub album_musicbrainz_id: Option<String>,
    pub album_year: Option<i32>,
    pub track_artists: Vec<Artist>,
    pub album_artists: Vec<Artist>,
}

/// Identify a track at `path` and return its metadata.
///
/// Steps:
/// 1. Compute SHA-256 of the file.
/// 2. Run `fpcalc` to generate a Chromaprint fingerprint.
/// 3. Look up the fingerprint via AcoustID (requires `acoustid_api_key`).
/// 4. Fetch recording + release details from MusicBrainz.
pub async fn lookup_track(path: &Path, acoustid_api_key: &str) -> Result<TrackMetadata, Error> {
    let sha256 = file_hash::compute_sha256(path)?;
    let (fingerprint, duration_secs) = chromaprint::fingerprint(path)?;

    let client = reqwest::Client::new();
    let acoustid_resp =
        acoustid::lookup(&client, acoustid_api_key, &fingerprint, duration_secs).await?;

    let best_result = acoustid_resp
        .results
        .into_iter()
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or(Error::AcoustIdNoResults)?;

    let best_recording = best_result
        .recordings
        .into_iter()
        .min_by(|a, b| match (a.duration, b.duration) {
            (Some(ad), Some(bd)) => (ad - duration_secs as f64)
                .abs()
                .partial_cmp(&(bd - duration_secs as f64).abs())
                .unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
        .ok_or(Error::AcoustIdNoRecordings)?;

    let recording = musicbrainz::fetch_recording(&best_recording.id).await?;

    let first_release = recording
        .releases
        .as_ref()
        .and_then(|r| r.first())
        .ok_or(Error::MusicBrainzNoReleases)?;

    let release = musicbrainz::fetch_release(&first_release.id).await?;

    // Track artists
    let track_artists = recording
        .artist_credit
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|c| Artist {
            name: c.name.clone(),
            musicbrainz_id: Some(c.artist.id.clone()),
        })
        .collect::<Vec<_>>();

    // Album artists from release group
    let album_artists = release
        .release_group
        .as_ref()
        .and_then(|rg| rg.artist_credit.as_deref())
        .unwrap_or(&[])
        .iter()
        .map(|c| Artist {
            name: c.name.clone(),
            musicbrainz_id: Some(c.artist.id.clone()),
        })
        .collect::<Vec<_>>();

    // Album title: prefer release group title, fall back to release title
    let album_title = release
        .release_group
        .as_ref()
        .map(|rg| rg.title.clone())
        .unwrap_or_else(|| release.title.clone());

    // Album MusicBrainz ID from release group
    let album_musicbrainz_id = release.release_group.as_ref().map(|rg| rg.id.clone());

    // Album year: parse "YYYY" or "YYYY-MM-DD"
    let album_year = release.date.as_ref().and_then(|d| {
        d.0.split('-')
            .next()
            .and_then(|y| y.parse::<i32>().ok())
    });

    // Track number: global position across discs
    let track_number = release
        .media
        .as_ref()
        .and_then(|media| {
            let mut offset = 0u32;
            media.iter().find_map(|medium| {
                let tracks = medium.tracks.as_deref().unwrap_or(&[]);
                if let Some(track) = tracks
                    .iter()
                    .find(|t| t.recording.as_ref().is_some_and(|r| r.id == best_recording.id))
                {
                    Some((offset + track.position) as i32)
                } else {
                    offset += tracks.len() as u32;
                    None
                }
            })
        })
        .ok_or(Error::MusicBrainzNoTrackNumber)?;

    Ok(TrackMetadata {
        sha256,
        track_title: recording.title.clone(),
        track_number,
        duration: Duration::from_secs(duration_secs as u64),
        track_musicbrainz_id: Some(best_recording.id),
        album_title,
        album_musicbrainz_id,
        album_year,
        track_artists,
        album_artists,
    })
}
