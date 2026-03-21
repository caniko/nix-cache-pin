use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use nix_cache_pin_lib::{
    config::PinConfig,
    hydra::{self, HydraStatus},
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "hydra-query",
    about = "Query Hydra CI for package build status"
)]
struct Cli {
    /// Path to JSON config file
    #[arg(short, long, group = "mode")]
    config: Option<PathBuf>,

    /// Hydra URL (standalone mode)
    #[arg(long, requires = "job")]
    hydra_url: Option<String>,

    /// Hydra jobset (e.g. nixpkgs/trunk)
    #[arg(long)]
    jobset: Option<String>,

    /// Hydra job path (e.g. blender.x86_64-linux)
    #[arg(long, group = "mode")]
    job: Option<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    if let Some(config_path) = &cli.config {
        // Config mode: query all packages
        let cfg = PinConfig::from_file(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;

        let mut handles = Vec::new();
        for pkg in &cfg.packages {
            let client = client.clone();
            let cfg = cfg.clone();
            let pkg = pkg.clone();
            handles.push(tokio::spawn(async move {
                hydra::query_hydra_build(&client, &cfg, &pkg).await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await?);
        }

        if cli.json {
            let json_results: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "package": r.package,
                        "status": match r.status {
                            HydraStatus::OnHydra => "hydra",
                            HydraStatus::NotOnHydra => "not-on-hydra",
                        },
                        "evals": r.evals,
                        "store_path": r.store_path,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_results)?);
        } else {
            for r in &results {
                let status = match r.status {
                    HydraStatus::OnHydra => "on Hydra".green().to_string(),
                    HydraStatus::NotOnHydra => "not on Hydra".yellow().to_string(),
                };
                eprintln!("  {}: {status}", r.package);
                if !r.evals.is_empty() {
                    eprintln!("    evals: {:?}", &r.evals[..r.evals.len().min(5)]);
                }
                if let Some(sp) = &r.store_path {
                    eprintln!("    store_path: {sp}");
                }
            }
        }
    } else if let Some(job) = &cli.job {
        // Standalone mode: query a specific job
        let hydra_url = cli
            .hydra_url
            .as_deref()
            .unwrap_or("https://hydra.nixos.org");

        let url = format!("{hydra_url}/job/{job}/latest-finished");
        let body: serde_json::Value = client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if cli.json {
            println!("{}", serde_json::to_string_pretty(&body)?);
        } else {
            let store_path = body
                .get("buildoutputs")
                .and_then(|o| o.get("out"))
                .and_then(|o| o.get("path"))
                .and_then(|p| p.as_str());
            let evals = body
                .get("jobsetevals")
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .take(5)
                        .filter_map(|v| v.as_i64())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            eprintln!("  job: {job}");
            eprintln!("  status: {}", "on Hydra".green());
            eprintln!("  evals: {evals:?}");
            if let Some(sp) = store_path {
                eprintln!("  store_path: {sp}");
            }
        }
    } else {
        anyhow::bail!("provide --config or --job");
    }

    Ok(())
}
