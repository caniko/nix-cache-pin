use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{config::PinConfig, ext::RealCommands, narinfo};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "narinfo-check",
    about = "Check if nix store paths have binary cache hits"
)]
struct Cli {
    /// Store path(s) to check
    #[arg(group = "mode")]
    store_paths: Vec<String>,

    /// Binary cache URL(s) to check against
    #[arg(short, long, default_value = "https://cache.nixos.org")]
    cache: Vec<String>,

    /// Path to JSON config file (check all packages at a revision)
    #[arg(long, group = "mode")]
    config: Option<PathBuf>,

    /// Revision to check (requires --config)
    #[arg(long, requires = "config")]
    rev: Option<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    if let Some(config_path) = &cli.config {
        // Config mode: eval all packages at a revision
        let cfg = PinConfig::from_file(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;
        let rev = cli
            .rev
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--rev is required when using --config"))?;

        let ext = Arc::new(RealCommands);
        let result = narinfo::verify_narinfo_at_rev(&client, &cfg, rev, &cfg.packages, &ext).await;

        if cli.json {
            let json_results: Vec<serde_json::Value> = result
                .results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "package": r.package,
                        "cached": r.cached,
                        "store_path": r.store_path,
                        "error": r.error,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "rev": result.rev,
                    "all_cached": result.all_cached,
                    "results": json_results,
                }))?
            );
        } else {
            let full_attr_prefix = cfg.full_attr_prefix();
            for r in &result.results {
                let marker = if r.cached {
                    "cached".green().to_string()
                } else {
                    "miss".red().to_string()
                };
                let prefix = if full_attr_prefix.is_empty() {
                    r.package.clone()
                } else {
                    format!("{full_attr_prefix}.{}", r.package)
                };
                eprintln!("  {prefix}: {marker}");
                if let Some(sp) = &r.store_path {
                    eprintln!("    {sp}");
                }
                if let Some(err) = &r.error {
                    eprintln!("    {}", err.red());
                }
            }
        }

        if !result.all_cached {
            std::process::exit(1);
        }
    } else {
        // Direct store path mode
        if cli.store_paths.is_empty() {
            anyhow::bail!("provide at least one store path or use --config with --rev");
        }

        let mut all_cached = true;
        for store_path in &cli.store_paths {
            let cached = narinfo::check_narinfo(&client, store_path, &cli.cache).await;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "store_path": store_path,
                        "cached": cached,
                    })
                );
            } else {
                let marker = if cached {
                    "cached".green().to_string()
                } else {
                    "miss".red().to_string()
                };
                eprintln!("{store_path}: {marker}");
            }
            if !cached {
                all_cached = false;
            }
        }

        if !all_cached {
            std::process::exit(1);
        }
    }

    // Force-exit to avoid hanging on tokio runtime shutdown.
    std::process::exit(0);
}
