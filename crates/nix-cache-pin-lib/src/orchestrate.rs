use crate::config::PinConfig;
use crate::error::Result;
use crate::flakeref;
use crate::github;
use crate::hydra::{self, HydraStatus};
use crate::narinfo::{self, PackageCheckResult};
use colored::Colorize;
use reqwest::Client;
use std::collections::HashSet;

/// Print per-package cache status.
fn print_check_results(full_attr_prefix: &str, results: &[PackageCheckResult]) {
    for r in results {
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
    }
}

/// Warn about packages never cached at any revision.
fn warn_never_cached(cfg: &PinConfig, all_results: &[PackageCheckResult]) {
    if all_results.is_empty() {
        return;
    }
    let seen_cached: HashSet<&str> = all_results
        .iter()
        .filter(|r| r.cached)
        .map(|r| r.package.as_str())
        .collect();

    for pkg in &cfg.packages {
        if !seen_cached.contains(pkg.as_str()) {
            eprintln!(
                "{}",
                format!("  Warning: {pkg} was not cached at any of the revisions tried.").yellow()
            );
            eprintln!("    It may have been dropped from the configured caches, or its");
            eprintln!("    flake-evaluated derivation diverges from what the caches provide.");
        }
    }
}

/// Try a list of Hydra eval records, verifying narinfo at each.
/// Returns the first revision where all packages are cached.
pub async fn try_eval_revisions(
    client: &Client,
    cfg: &PinConfig,
    evals: &[hydra::HydraEval],
    skip_ids: &HashSet<i64>,
    prior_results: &mut Vec<PackageCheckResult>,
) -> Result<Option<String>> {
    let mut seen_revs = HashSet::new();

    for eval_entry in evals {
        if skip_ids.contains(&eval_entry.id) {
            continue;
        }

        let eval_data = match hydra::fetch_eval(client, &cfg.hydra_url, eval_entry.id).await {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Failed to fetch eval {}: {e}", eval_entry.id).red()
                );
                continue;
            }
        };

        let rev = match hydra::extract_eval_rev(cfg, &eval_data) {
            Some(r) => r,
            None => {
                eprintln!(
                    "{}",
                    format!("Failed to extract revision from eval {}", eval_entry.id).red()
                );
                continue;
            }
        };

        if !seen_revs.insert(rev.clone()) {
            continue;
        }

        eprintln!(
            "\n{}",
            format!(
                "Verifying narinfo at rev {} (eval {})...",
                &rev[..12.min(rev.len())],
                eval_entry.id
            )
            .cyan()
        );

        let check = narinfo::verify_narinfo_at_rev(client, cfg, &rev, &cfg.packages).await;
        print_check_results(cfg.full_attr_prefix(), &check.results);
        prior_results.extend(check.results.clone());

        if check.all_cached {
            return Ok(Some(rev));
        }

        let misses: Vec<&str> = check
            .results
            .iter()
            .filter(|r| !r.cached)
            .map(|r| r.package.as_str())
            .collect();
        eprintln!(
            "{}",
            format!("  Missing: {} — trying next eval...", misses.join(", ")).yellow()
        );

        if cfg.fail_fast {
            eprintln!("\n{}", "Fail-fast: aborting.".red());
            std::process::exit(1);
        }
    }

    warn_never_cached(cfg, prior_results);
    Ok(None)
}

/// Pure narinfo scan using GitHub commits (fallback when nothing is on Hydra).
pub async fn narinfo_scan(client: &Client, cfg: &PinConfig) -> Result<Option<String>> {
    let github_repo = match flakeref::extract_github_repo(&cfg.flake_ref) {
        Some(r) => r.to_string(),
        None => {
            eprintln!(
                "{}",
                "Narinfo scan requires a github: flake ref for commit listing.".red()
            );
            eprintln!("  flakeRef: {}", cfg.flake_ref);
            return Ok(None);
        }
    };

    eprintln!(
        "{}",
        format!("Fetching recent {} commits...", cfg.branch).cyan()
    );
    let commits = github::list_commits(&github_repo, &cfg.branch, cfg.depth).await?;

    let full_attr_prefix = cfg.full_attr_prefix();
    eprintln!(
        "{}\n",
        format!(
            "Checking {} revisions for {full_attr_prefix} cache hits...",
            commits.len()
        )
        .cyan()
    );

    let mut all_check_results = Vec::new();

    for rev in &commits {
        let short = &rev[..12.min(rev.len())];
        eprintln!("Checking rev {short}...");
        let check = narinfo::verify_narinfo_at_rev(client, cfg, rev, &cfg.packages).await;

        let cached_count = check.results.iter().filter(|r| r.cached).count();
        eprintln!("  {cached_count}/{} packages cached", check.results.len());
        all_check_results.extend(check.results.clone());

        if check.all_cached {
            eprintln!("  {}", "All packages cached!".green());
            eprintln!("\n{}", format!("Package status (rev {short}):").cyan());
            print_check_results(full_attr_prefix, &check.results);
            return Ok(Some(rev.clone()));
        }

        let misses: Vec<&str> = check
            .results
            .iter()
            .filter(|r| !r.cached)
            .map(|r| r.package.as_str())
            .collect();
        eprintln!("  {}", format!("Missing: {}", misses.join(", ")).yellow());

        if cfg.fail_fast {
            eprintln!(
                "\n{}",
                format!("Fail-fast: package(s) not cached at rev {short}, aborting.").red()
            );
            std::process::exit(1);
        }
    }

    eprintln!(
        "\n{}",
        format!(
            "No revision found with all packages cached in the last {} commits.",
            cfg.depth
        )
        .red()
    );
    warn_never_cached(cfg, &all_check_results);
    Ok(None)
}

/// Main orchestration: Hydra first, then narinfo fallback.
pub async fn find_target_rev(client: &Client, cfg: &PinConfig) -> Result<Option<String>> {
    let full_attr_prefix = cfg.full_attr_prefix().to_string();

    // Step 1: Try Hydra for all packages
    eprintln!(
        "{}",
        format!(
            "Querying {} for {full_attr_prefix} builds...",
            cfg.hydra_url
        )
        .cyan()
    );

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
        results.push(handle.await.unwrap());
    }

    let on_hydra: Vec<_> = results
        .iter()
        .filter(|r| r.status == HydraStatus::OnHydra)
        .collect();
    let not_on_hydra: Vec<_> = results
        .iter()
        .filter(|r| r.status == HydraStatus::NotOnHydra)
        .collect();

    if !on_hydra.is_empty() {
        eprintln!("  {}", "On Hydra:".green());
        for r in &on_hydra {
            eprintln!("    {}", r.package);
        }
    }
    if !not_on_hydra.is_empty() {
        eprintln!("  {}", "Not on Hydra:".yellow());
        for r in &not_on_hydra {
            eprintln!("    {}", r.package);
        }
    }

    // If nothing is on Hydra, fall back to pure narinfo scan
    if on_hydra.is_empty() {
        eprintln!(
            "\n{}",
            "No packages found on Hydra, falling back to narinfo scan...".yellow()
        );
        return narinfo_scan(client, cfg).await;
    }

    // Step 2: Find common evals (sorted newest first)
    let all_evals: Vec<&Vec<i64>> = on_hydra.iter().map(|r| &r.evals).collect();
    let common_evals: Vec<i64> = if all_evals.len() == 1 {
        all_evals[0].clone()
    } else {
        let first_set: HashSet<i64> = all_evals[0].iter().copied().collect();
        let common: HashSet<i64> = all_evals[1..].iter().fold(first_set, |acc, evals| {
            let set: HashSet<i64> = evals.iter().copied().collect();
            acc.intersection(&set).copied().collect()
        });
        let mut v: Vec<i64> = common.into_iter().collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };

    let candidate_evals: Vec<i64> = if !common_evals.is_empty() {
        common_evals
    } else {
        eprintln!(
            "\n{}",
            "Warning: no single eval has all Hydra packages — using bottleneck".yellow()
        );
        let bottleneck = on_hydra
            .iter()
            .filter(|r| !r.evals.is_empty())
            .min_by_key(|r| r.evals[0])
            .unwrap();
        eprintln!(
            "  Bottleneck: {} at eval {}",
            bottleneck.package, bottleneck.evals[0]
        );
        vec![bottleneck.evals[0]]
    };

    // Step 3: Try each candidate eval, verify ALL packages via narinfo
    let mut target_rev = None;
    let mut fast_path_results = Vec::new();

    for eval_id in &candidate_evals {
        eprintln!("\n{}", format!("Hydra status (eval {eval_id}):").cyan());
        for r in &on_hydra {
            let in_eval = r.evals.contains(eval_id);
            if in_eval {
                eprintln!("  {full_attr_prefix}.{}: {}", r.package, "cached".green());
            } else {
                let latest = r.evals.first().map(|e| e.to_string()).unwrap_or_default();
                eprintln!(
                    "  {full_attr_prefix}.{}: {} (latest: eval {latest})",
                    r.package,
                    "miss".red()
                );
            }
        }

        let eval_data = match hydra::fetch_eval(client, &cfg.hydra_url, *eval_id).await {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Failed to fetch eval {eval_id} from Hydra: {e}").red()
                );
                continue;
            }
        };

        let rev = match hydra::extract_eval_rev(cfg, &eval_data) {
            Some(r) => r,
            None => {
                eprintln!(
                    "{}",
                    format!("Failed to extract revision from eval {eval_id}").red()
                );
                continue;
            }
        };

        eprintln!(
            "\n{}",
            format!("Verifying narinfo at rev {}...", &rev[..12.min(rev.len())]).cyan()
        );

        let check = narinfo::verify_narinfo_at_rev(client, cfg, &rev, &cfg.packages).await;
        print_check_results(&full_attr_prefix, &check.results);
        fast_path_results.extend(check.results.clone());

        if check.all_cached {
            target_rev = Some(rev);
            break;
        }

        let misses: Vec<&str> = check
            .results
            .iter()
            .filter(|r| !r.cached)
            .map(|r| r.package.as_str())
            .collect();
        eprintln!(
            "{}",
            format!("  Missing: {} — trying older eval...", misses.join(", ")).yellow()
        );

        if cfg.fail_fast {
            eprintln!("\n{}", "Fail-fast: aborting.".red());
            std::process::exit(1);
        }
    }

    // Step 4: Broaden search to Hydra jobset eval history
    if target_rev.is_none() {
        eprintln!(
            "\n{}",
            "No common Hydra eval has all packages cached.".yellow()
        );
        eprintln!(
            "{}",
            "Broadening search to Hydra jobset eval history...".cyan()
        );

        let jobset_evals = hydra::query_hydra_jobset_evals(client, cfg, cfg.depth).await;
        if !jobset_evals.is_empty() {
            let skip_ids: HashSet<i64> = candidate_evals.iter().copied().collect();
            let broad_result = try_eval_revisions(
                client,
                cfg,
                &jobset_evals,
                &skip_ids,
                &mut fast_path_results,
            )
            .await?;
            if broad_result.is_some() {
                return Ok(broad_result);
            }
        }

        eprintln!(
            "\n{}",
            "No Hydra eval has all packages in the binary cache.".red()
        );
        eprintln!("{}", "Falling back to narinfo scan...".yellow());
        return narinfo_scan(client, cfg).await;
    }

    Ok(target_rev)
}
