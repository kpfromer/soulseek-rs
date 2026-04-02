mod cli;
mod error;
mod metadata;
mod processor;
mod template;

use clap::Parser;

use cli::Args;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    processor::run(args).await
}
