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

run:
  cargo run -p soulseek-rs-lib --example search_and_download --features=tracing --release
