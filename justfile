set dotenv-load := true

default:
  just --list

check-clippy:
  cargo clippy --workspace --all-targets --all-features -- -D warnings

check-format:
  cargo fmt --all --check

check: check-format check-clippy

alias c := check

format:
  cargo fmt --all

test:
  cargo test --workspace --all-features

fix:
  cargo fix --workspace --all-features

run-soulseek:
  cargo run -p soulseek-rs-lib --example search_and_download --features=tracing --release

run-song:
  cargo run -p song-rs --example search_and_download -- --download-best --file-type lossless

audit:
  cargo audit