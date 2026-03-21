use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{config::PinConfig, flake_update, orchestrate};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cache-pin",
    about = "Pin a flake input to a revision where all specified packages have binary cache hits"
)]
struct Cli {
    /// Path to JSON config file
    #[arg(short, long)]
    config: PathBuf,

    /// Don't actually update, just show what would change
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Don't run `nix flake lock` after updating
    #[arg(long)]
    no_lock: bool,

    /// Exit immediately on first cache miss
    #[arg(short, long)]
    fail_fast: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut cfg = PinConfig::from_file(&cli.config)
        .with_context(|| format!("failed to read config: {}", cli.config.display()))?;

    if cli.fail_fast {
        cfg.fail_fast = true;
    }

    let full_attr_prefix = cfg.full_attr_prefix().to_string();

    eprintln!("{}", format!("nix-cache-pin: {}", cfg.name).cyan().bold());
    eprintln!("  input:       {}", cfg.input_name);
    eprintln!("  flake ref:   {}", cfg.flake_ref);
    eprintln!("  hydra:       {}", cfg.hydra_url);
    eprintln!("  attr prefix: {full_attr_prefix}");
    eprintln!("  arch:        {}", cfg.arch);
    eprintln!("  packages:    {}\n", cfg.packages.join(", "));

    let client = reqwest::Client::new();
    let target_rev = orchestrate::find_target_rev(&client, &cfg)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no revision found with all packages cached"))?;

    // Compare with current pin in flake.nix
    let flake_nix_path = PathBuf::from("flake.nix");
    if !flake_nix_path.exists() {
        anyhow::bail!("flake.nix not found in current directory");
    }

    let flake_nix_content = std::fs::read_to_string(&flake_nix_path)?;
    let current_rev =
        flake_update::read_current_rev(&flake_nix_content, &cfg.input_name, &cfg.flake_ref)
            .context("failed to read current revision from flake.nix")?;

    eprintln!("\n  Current pin: {current_rev}");
    eprintln!("  Target rev:  {target_rev}");

    if current_rev == target_rev {
        eprintln!("{}", "Already up to date!".green());
        return Ok(());
    }

    eprintln!("\n{}", "New revision available".yellow());

    if cli.dry_run {
        eprintln!(
            "{}",
            format!("Would update {} to {target_rev} (dry run)", cfg.input_name).magenta()
        );
        return Ok(());
    }

    // Update the pinned rev in flake.nix
    eprintln!("Updating flake.nix...");
    flake_update::update_flake_nix(&flake_nix_path, &cfg.flake_ref, &current_rev, &target_rev)?;
    eprintln!(
        "{}",
        format!("Updated {} to {target_rev}", cfg.input_name).green()
    );

    // Update the flake lock file
    if !cli.no_lock {
        eprintln!(
            "Running nix flake lock --update-input {}...",
            cfg.input_name
        );
        match flake_update::run_flake_lock(&cfg.input_name).await {
            Ok(()) => eprintln!("{}", "Lock file updated.".green()),
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Warning: failed to update lock file: {e}").red()
                );
                eprintln!("  You may need to run 'nix flake lock' manually.");
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
