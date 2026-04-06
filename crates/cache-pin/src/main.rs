use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{
    config::PinConfig, flake_update, orchestrate, output::Output, runner,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cache-pin",
    about = "Pin a flake input to a revision where all specified packages have binary cache hits"
)]
struct Cli {
    /// Path to JSON config file(s). Pass multiple --config flags to run searches in parallel.
    #[arg(short, long, required = true)]
    config: Vec<PathBuf>,

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

    let mut configs: Vec<PinConfig> = cli
        .config
        .iter()
        .map(|path| {
            PinConfig::from_file(path)
                .with_context(|| format!("failed to read config: {}", path.display()))
        })
        .collect::<Result<_>>()?;

    if cli.fail_fast {
        for cfg in &mut configs {
            cfg.fail_fast = true;
        }
    }

    let result = if configs.len() == 1 {
        run_single(configs.remove(0), cli.dry_run, cli.no_lock).await
    } else {
        run_multi(configs, cli.dry_run, cli.no_lock).await
    };

    // Force-exit to avoid hanging on tokio runtime shutdown
    // (reqwest's HTTP/2 connection pool tasks can linger indefinitely).
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Single-config path: immediate output, same behavior as before.
async fn run_single(cfg: PinConfig, dry_run: bool, no_lock: bool) -> Result<()> {
    let mut out = Output::immediate(&cfg.name);
    let full_attr_prefix = cfg.full_attr_prefix().to_string();

    eprintln!("{}", format!("nix-cache-pin: {}", cfg.name).cyan().bold());
    eprintln!("  input:       {}", cfg.input_name);
    eprintln!("  flake ref:   {}", cfg.flake_ref);
    eprintln!("  hydra:       {}", cfg.hydra_url);
    eprintln!("  attr prefix: {full_attr_prefix}");
    eprintln!("  arch:        {}", cfg.arch);
    eprintln!("  packages:    {}\n", cfg.packages.join(", "));

    let client = reqwest::Client::new();
    let target_rev = orchestrate::find_target_rev(&client, &cfg, &mut out)
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

    if dry_run {
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
    if !no_lock {
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

/// Multi-config path: parallel search with buffered output, sequential apply.
async fn run_multi(configs: Vec<PinConfig>, dry_run: bool, no_lock: bool) -> Result<()> {
    let pin_count = configs.len();
    eprintln!(
        "{}",
        format!("Running {pin_count} cache-pin searches in parallel...\n")
            .cyan()
            .bold()
    );

    // Phase 1: Parallel find
    let find_results = runner::find_all(configs).await;

    // Separate successes from failures
    let mut successes = Vec::new();
    let mut failures = Vec::new();

    for fr in find_results {
        match fr.target_rev {
            Ok(Some(rev)) => successes.push((fr.config, rev)),
            Ok(None) => failures.push((
                fr.config.name.clone(),
                "no revision found with all packages cached".to_string(),
            )),
            Err(e) => failures.push((fr.config.name.clone(), e.to_string())),
        }
    }

    // Phase 2: Sequential apply
    if !successes.is_empty() {
        eprintln!(
            "\n{}",
            format!("Applying {} updates...", successes.len())
                .cyan()
                .bold()
        );
        for (cfg, target_rev) in &successes {
            let outcome = runner::apply(cfg, target_rev, dry_run, no_lock).await;
            if let Some(err) = outcome.error {
                failures.push((outcome.name, err));
            }
        }
    }

    // Summary
    if !failures.is_empty() {
        eprintln!("\n{}", "Failures:".red().bold());
        for (name, err) in &failures {
            eprintln!("  {}: {}", name.red(), err);
        }
        std::process::exit(1);
    }

    Ok(())
}
