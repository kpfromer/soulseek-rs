use clap::Parser;
use std::path::PathBuf;

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
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
