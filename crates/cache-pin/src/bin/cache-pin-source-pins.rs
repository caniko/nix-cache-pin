use anyhow::Result;
use clap::Parser;
use nix_cache_pin_lib::source_pins::{update, SourcePinsOptions, UpdateStatus};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Generate Nix hashes for Cargo git dependencies")]
struct Cli {
    #[arg(long)]
    name: String,
    #[arg(long)]
    lock_file: PathBuf,
    #[arg(long)]
    output_file: PathBuf,
    #[arg(long, default_value = "nix")]
    nix_bin: PathBuf,
    #[arg(long)]
    workers: Option<usize>,
    #[arg(short = 'n', long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    println!("=== cache-pin source-pins: {} ===", cli.name);
    println!("  Lock file:  {}", cli.lock_file.display());
    println!("  Output:     {}", cli.output_file.display());

    let status = update(&SourcePinsOptions {
        name: cli.name,
        lock_file: cli.lock_file,
        output_file: cli.output_file,
        nix_bin: cli.nix_bin,
        workers: cli
            .workers
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from)),
        dry_run: cli.dry_run,
    })?;

    match status {
        UpdateStatus::Current => println!("  Sidecar is current; no prefetch needed"),
        UpdateStatus::WouldUpdate { added, removed } => {
            println!("  Would refresh source pins: {added} added, {removed} removed")
        }
        UpdateStatus::Updated { sources } => {
            println!("  Updated source pins for {sources} source(s)")
        }
    }
    Ok(())
}
