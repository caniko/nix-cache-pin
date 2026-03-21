use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{config::PinConfig, narinfo};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nix-eval-store-path",
    about = "Evaluate nix store paths for flake attributes"
)]
struct Cli {
    /// Path to JSON config file (eval all packages)
    #[arg(short, long, group = "mode")]
    config: Option<PathBuf>,

    /// Revision to evaluate at
    #[arg(long)]
    rev: String,

    /// Flake reference (standalone mode)
    #[arg(long, group = "mode")]
    flake_ref: Option<String>,

    /// System architecture
    #[arg(long, default_value = "x86_64-linux")]
    arch: String,

    /// Flake output attribute (e.g. legacyPackages, packages)
    #[arg(long, default_value = "legacyPackages")]
    flake_output: String,

    /// Attribute prefix
    #[arg(long, default_value = "")]
    attr_prefix: String,

    /// Attribute(s) to evaluate (standalone mode)
    #[arg(long)]
    attr: Vec<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(config_path) = &cli.config {
        // Config mode: eval all packages
        let cfg = PinConfig::from_file(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;

        let full_attr_prefix = cfg.full_attr_prefix().to_string();

        for pkg in &cfg.packages {
            match narinfo::eval_store_path(
                &cfg.flake_ref,
                &cli.rev,
                &cfg.arch,
                &cfg.flake_output,
                &full_attr_prefix,
                pkg,
            )
            .await
            {
                Ok(store_path) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "package": pkg,
                                "store_path": store_path,
                            })
                        );
                    } else {
                        println!("{pkg}: {store_path}");
                    }
                }
                Err(e) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "package": pkg,
                                "error": e.to_string(),
                            })
                        );
                    } else {
                        eprintln!("{}: {}", pkg, e.to_string().red());
                    }
                }
            }
        }
    } else if let Some(flake_ref) = &cli.flake_ref {
        // Standalone mode
        if cli.attr.is_empty() {
            anyhow::bail!("provide --attr when using --flake-ref");
        }

        for attr in &cli.attr {
            match narinfo::eval_store_path(
                flake_ref,
                &cli.rev,
                &cli.arch,
                &cli.flake_output,
                &cli.attr_prefix,
                attr,
            )
            .await
            {
                Ok(store_path) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "attr": attr,
                                "store_path": store_path,
                            })
                        );
                    } else {
                        println!("{attr}: {store_path}");
                    }
                }
                Err(e) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "attr": attr,
                                "error": e.to_string(),
                            })
                        );
                    } else {
                        eprintln!("{}: {}", attr, e.to_string().red());
                    }
                }
            }
        }
    } else {
        anyhow::bail!("provide --config or --flake-ref");
    }

    Ok(())
}
