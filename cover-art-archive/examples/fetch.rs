use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Fetch album art from the Cover Art Archive by identifying an audio file")]
struct Args {
    /// Path to the audio file
    file: PathBuf,

    /// AcoustID API key (https://acoustid.org/api-key)
    #[arg(long, env = "ACOUSTID_API_KEY")]
    acoustid_api_key: String,

    /// Output directory (defaults to current directory)
    #[arg(short, long, default_value = ".")]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if !args.output_dir.is_dir() {
        return Err(format!("{} is not a directory", args.output_dir.display()).into());
    }

    println!("Identifying: {}", args.file.display());

    let meta = musicbrainz::lookup_track(&args.file, &args.acoustid_api_key)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Error identifying track: {e}");
            std::process::exit(1);
        });

    println!("Release MBID: {}", meta.release_mbid);
    println!("Fetching album art...");

    match cover_art_archive::get_album_art(&meta.release_mbid).await {
        Ok(album_art) => {
            let output = args
                .output_dir
                .join(format!("{}.{}", meta.release_mbid, album_art.extension));

            std::fs::write(&output, &album_art.data).unwrap_or_else(|e| {
                eprintln!("Error writing file: {e}");
                std::process::exit(1);
            });

            println!(
                "Saved {} bytes to {}",
                album_art.data.len(),
                output.display()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Error fetching album art: {e}");
            Err(format!("Error fetching album art: {e}").into())
        }
    }
}
