set dotenv-load := true

default:
  just --list

check:
  cargo clippy --all-targets --all-features

alias c := check

format:
  cargo fmt

# Run tests for backend
test: 
  cargo test

# Fix frontend biome issues
fix: 
  cargo fix
