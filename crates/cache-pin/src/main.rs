use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{
    config::PinConfig,
    ext::RealCommands,
    merge::group_configs,
    narinfo::{self, Availability, PackageCheckResult, VerifyResult},
    runner, transaction,
};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

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

    /// Apply all discovered pins in one transaction and write the derived manifest
    #[arg(long)]
    update: bool,

    /// Exit immediately on first cache miss
    #[arg(short, long)]
    fail_fast: bool,

    /// Verify the currently locked revision without searching or writing
    #[arg(long, conflicts_with_all = ["dry_run", "no_lock", "update"])]
    check_current: bool,

    /// Output current-check results as JSON
    #[arg(long, requires = "check_current")]
    json: bool,
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

    let result = if cli.check_current {
        check_current(configs, cli.json).await
    } else {
        run_multi(configs, cli.dry_run, cli.no_lock, cli.update).await
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

async fn check_current(configs: Vec<PinConfig>, json: bool) -> Result<()> {
    let groups = group_configs(configs)?;
    let client = reqwest::Client::new();
    let ext = Arc::new(RealCommands);
    let mut reports = Vec::with_capacity(groups.len());
    let mut all_cached = true;

    for group in groups {
        let mut cfg = group.merged;
        cfg.fail_fast = false;
        let revision = runner::current_locked_revision(&cfg)?;
        let result = narinfo::verify_current(&client, &cfg, &revision, &ext).await;
        all_cached &= result.all_cached;

        if json {
            reports.push(current_report(&cfg, &result));
        } else {
            eprintln!(
                "{}",
                format!("{} ({} @ {})", cfg.name, cfg.input_name, revision)
                    .cyan()
                    .bold()
            );
            for item in &result.results {
                let status = match item.availability {
                    Availability::Cached => "cached".green(),
                    Availability::Missing => "missing".red(),
                    Availability::Unknown => "unknown".yellow(),
                };
                eprintln!("  {}: {status}", item.package);
                if let Some(store_path) = &item.store_path {
                    eprintln!("    {store_path}");
                }
                if let Some(error) = item.failure() {
                    eprintln!("    {error}");
                }
            }
        }
    }

    if json {
        let report = if reports.len() == 1 {
            reports.pop().unwrap()
        } else {
            serde_json::Value::Array(reports)
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    if !all_cached {
        anyhow::bail!("current cache check failed");
    }
    Ok(())
}

fn current_report(cfg: &PinConfig, result: &VerifyResult) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "pin": cfg.name,
        "input": cfg.input_name,
        "revision": result.rev,
        "allCached": result.all_cached,
        "results": result.results.iter().map(current_result).collect::<Vec<_>>(),
    })
}

fn current_result(result: &PackageCheckResult) -> serde_json::Value {
    serde_json::json!({
        "package": result.package,
        "target": result.target,
        "storePath": result.store_path,
        "availability": result.availability.as_str(),
        "cache": result.cache,
        "error": result.failure(),
    })
}

/// Multi-config path: parallel search with spinners (or buffered fallback), sequential apply.
async fn run_multi(
    configs: Vec<PinConfig>,
    dry_run: bool,
    no_lock: bool,
    update: bool,
) -> Result<()> {
    let groups = group_configs(configs)?;
    let configs: Vec<PinConfig> = groups.iter().map(|group| group.merged.clone()).collect();

    let use_spinner = std::io::stderr().is_terminal();
    let pin_count = configs.len();
    eprintln!(
        "{}",
        format!("Running {pin_count} cache-pin searches in parallel...\n")
            .cyan()
            .bold()
    );

    let ext = Arc::new(RealCommands);

    // Phase 1: Parallel find
    let find_results = runner::find_all(configs, use_spinner, &ext).await;

    // Separate successes from failures
    let mut successes = Vec::with_capacity(find_results.len());
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

    // Never start writes if any search failed. This makes aggregate cache-pin
    // runs fail-before-write instead of applying a partial set of pins.
    if !failures.is_empty() {
        eprintln!("\n{}", "Failures (no updates applied):".red().bold());
        for (name, err) in &failures {
            eprintln!("  {}: {}", name.red(), err);
        }
        anyhow::bail!("cache-pin search failed; no updates applied");
    }

    // Phase 2: Validate and apply all files as one transaction.
    if !successes.is_empty() {
        eprintln!(
            "\n{}",
            format!("Applying {} updates...", successes.len())
                .cyan()
                .bold()
        );
        if let Err(error) =
            transaction::apply(&groups, &successes, dry_run, no_lock, update, &ext).await
        {
            failures.push(("transaction".to_string(), error.to_string()));
        }
    }

    // Summary
    if !failures.is_empty() {
        eprintln!("\n{}", "Failures:".red().bold());
        for (name, err) in &failures {
            eprintln!("  {}: {}", name.red(), err);
        }
        anyhow::bail!("cache-pin apply failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_json_has_versioned_shape() {
        let cfg = PinConfig::from_json(
            r#"{"name":"test","packages":["hello"],"inputName":"nixpkgs","attrPrefix":"pkgs","pythonPackages":null,"caches":[],"hydraJobset":"jobset","hydraUrl":"hydra","hydraJobPattern":"pattern","hydraRevInput":"nixpkgs","depth":1,"branch":"main","flakeRef":"github:NixOS/nixpkgs","flakeOutput":"legacyPackages","failFast":false,"arch":"x86_64-linux"}"#,
        )
        .unwrap();
        let report = current_report(
            &cfg,
            &VerifyResult {
                rev: "abc".into(),
                all_cached: true,
                results: vec![PackageCheckResult {
                    package: "hello".into(),
                    target: None,
                    cached: true,
                    availability: Availability::Cached,
                    cache: Some("https://cache.nixos.org".into()),
                    store_path: Some("/nix/store/hash-hello".into()),
                    error: None,
                    version: None,
                    version_error: None,
                    version_rejected_by: Vec::new(),
                }],
            },
        );

        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["input"], "nixpkgs");
        assert_eq!(report["results"][0]["availability"], "cached");
        assert!(report["results"][0].get("target").is_some());
    }
}
