set dotenv-load := true

default:
  just --list

check:
  cargo clippy --workspace --all-targets --all-features

alias c := check

format:
  cargo fmt --all

test:
  cargo test --workspace --all-features

fix:
  cargo fix --workspace --all-features

shear:
  cargo shear

shear-fix:
  cargo shear --fix

run:
  cargo run -p soulseek-rs-lib --example search_and_download --features=tracing --release

# Usage: just lookup /path/to/song.mp3
# Requires ACOUSTID_API_KEY env var (or pass via --acoustid-api-key)
lookup file:
  cargo run -p musicbrainz --example lookup -- "{{file}}"

# Usage: just download tracks.csv
# Requires SOULSEEK_USERNAME and SOULSEEK_PASSWORD env vars (or .env file)
download csv:
  cargo run -p downloader --release -- "{{csv}}"
