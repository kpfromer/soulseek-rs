set dotenv-load := true

default:
  just --list

check:
  cargo clippy --all-targets --all-features

alias c := check

format:
  cargo fmt

test: 
  cargo test --all-features

fix: 
  cargo fix --all-features

run:
  cargo run --example search_and_download --features=tracing --release