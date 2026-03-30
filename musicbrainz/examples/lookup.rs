use clap::Parser;
use reqwest::redirect::Policy;
use std::path::PathBuf;
use url::Url;

// TODO: extract this out to it's own lib
#[derive(Debug)]
enum CoverArtArchiveError {
    InvalidMBID,
    NotFound,
    RateLimitExceeded,
    UnexpectedStatus(u16),
    UnexpectedImageStatus(u16),
    NetworkError,
    UnexpectedError,
}

#[derive(Debug)]
struct AlbumArt {
    data: Vec<u8>,
    extension: String,
}

async fn get_album_art(release_mbid: &str) -> Result<AlbumArt, CoverArtArchiveError> {
    let url = Url::parse("https://coverartarchive.org/release/")
        .map_err(|_| CoverArtArchiveError::UnexpectedError)?
        .join(&format!("{}/front", release_mbid))
        .map_err(|_| CoverArtArchiveError::UnexpectedError)?;

    println!("Release MBID: {}", release_mbid);
    println!("URL: {}", url);

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
            // This is a temporary redirect to the image URL.
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

#[derive(Parser)]
#[command(about = "Identify a music file via Chromaprint + AcoustID + MusicBrainz")]
struct Args {
    /// Path to the audio file
    file: PathBuf,

    /// AcoustID API key (https://acoustid.org/api-key)
    #[arg(long, env = "ACOUSTID_API_KEY")]
    acoustid_api_key: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    println!("Looking up: {}", args.file.display());

    match musicbrainz::lookup_track(&args.file, &args.acoustid_api_key).await {
        Ok(meta) => {
            println!("\n--- Track Metadata ---");
            println!("SHA-256:              {}", meta.sha256);
            println!("Title:                {}", meta.track_title);
            println!("Track number:         {}", meta.track_number);
            println!("Duration:             {:.1}s", meta.duration.as_secs_f64());
            println!(
                "Track MusicBrainz ID: {}",
                meta.track_musicbrainz_id.as_deref().unwrap_or("(none)")
            );
            println!("Release MusicBrainz ID: {}", meta.release_mbid);

            println!("\n--- Album ---");
            println!("Title:                {}", meta.album_title);
            println!(
                "MusicBrainz ID:       {}",
                meta.album_musicbrainz_id.as_deref().unwrap_or("(none)")
            );
            println!(
                "Year:                 {}",
                meta.album_year
                    .map(|y| y.to_string())
                    .as_deref()
                    .unwrap_or("(none)")
            );
            println!("\n--- Track Artists ---");
            for artist in &meta.track_artists {
                println!(
                    "  {} ({})",
                    artist.name,
                    artist.musicbrainz_id.as_deref().unwrap_or("no ID")
                );
            }
            println!("\n--- Album Artists ---");
            for artist in &meta.album_artists {
                println!(
                    "  {} ({})",
                    artist.name,
                    artist.musicbrainz_id.as_deref().unwrap_or("no ID")
                );
            }

            match get_album_art(&meta.release_mbid).await {
                Ok(album_art) => {
                    let extension = album_art.extension;
                    std::fs::write(
                        format!("./album-{}.{}", meta.release_mbid, extension),
                        album_art.data,
                    )
                    .unwrap();
                }
                Err(e) => eprintln!("Error getting album art: {e:?}"),
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
