use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::ext::ExternalCommands;
use crate::flakeref;
use crate::hydra::{self, HydraStatus};
use crate::narinfo::{self, PackageCheckResult};
use crate::output::Output;
use colored::Colorize;
use reqwest::Client;
use std::collections::HashSet;
use std::sync::Arc;

mod report;
use report::{format_rev_summary, warn_never_cached, warn_version_rejections};

async fn reject_wishes_built_on_hydra(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
) -> Result<()> {
    if cfg.wish_packages.is_empty() {
        return Ok(());
    }

    out.set_action(format!(
        "{}",
        format!(
            "Checking {} wish package(s) on Hydra...",
            cfg.wish_packages.len()
        )
        .cyan()
    ));

    let mut handles = Vec::with_capacity(cfg.wish_packages.len());
    for pkg in &cfg.wish_packages {
        let client = client.clone();
        let cfg = cfg.clone();
        let pkg = pkg.clone();
        handles.push(tokio::spawn(async move {
            hydra::query_hydra_build(&client, &cfg, &pkg).await
        }));
    }

    let mut built = Vec::new();
    for handle in handles {
        let result = handle.await.map_err(|source| Error::TaskJoin {
            task: "query_wish_package_on_hydra",
            source,
        })?;
        if result.status == HydraStatus::OnHydra {
            built.push(result.package);
        }
    }

    if built.is_empty() {
        out.milestone(format!(
            "{}",
            "  Wish packages: none built on Hydra yet".dimmed()
        ));
        Ok(())
    } else {
        Err(Error::WishPackagesBuilt {
            location: "Hydra latest-finished".to_string(),
            packages: built.join(", "),
        })
    }
}

async fn reject_wishes_cached_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    out: &mut Output,
    ext: &Arc<E>,
) -> Result<()> {
    if cfg.wish_packages.is_empty() {
        return Ok(());
    }

    let short = &rev[..12.min(rev.len())];
    out.set_action(format!(
        "{}",
        format!("Checking wish packages in caches at rev {short}...").cyan()
    ));

    // Required-package fail-fast semantics would stop at the first miss. A
    // promotion check must inspect every wish so a later cache hit is not
    // hidden by an earlier expected miss.
    let mut wish_cfg = cfg.clone();
    wish_cfg.fail_fast = false;
    let check =
        narinfo::verify_narinfo_at_rev(client, &wish_cfg, rev, &wish_cfg.wish_packages, ext).await;

    let built: Vec<String> = check
        .results
        .iter()
        .filter(|result| result.cached)
        .map(|result| result.package.clone())
        .collect();
    for result in &check.results {
        if let Some(error) = &result.error {
            out.milestone(format!(
                "{}",
                format!(
                    "  Warning: could not check wish package {} at rev {short}: {error}",
                    result.package
                )
                .yellow()
            ));
        }
    }

    if built.is_empty() {
        out.milestone(format!(
            "{}",
            format!("  Wish packages: no cache hits at rev {short}").dimmed()
        ));
        Ok(())
    } else {
        Err(Error::WishPackagesBuilt {
            location: format!("configured cache at rev {short}"),
            packages: built.join(", "),
        })
    }
}

/// Try a list of Hydra eval records, verifying narinfo at each.
/// Returns the first revision where all packages are cached.
pub async fn try_eval_revisions<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    evals: &[hydra::HydraEval],
    skip_ids: &HashSet<i64>,
    prior_results: &mut Vec<PackageCheckResult>,
    out: &mut Output,
    ext: &Arc<E>,
) -> Result<Option<String>> {
    try_eval_revisions_with_current(client, cfg, evals, skip_ids, prior_results, out, ext, None)
        .await
}

/// Try eval records while refusing candidates older than the current pin.
async fn try_eval_revisions_with_current<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    evals: &[hydra::HydraEval],
    skip_ids: &HashSet<i64>,
    prior_results: &mut Vec<PackageCheckResult>,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<Option<String>> {
    let mut seen_revs = HashSet::new();

    for eval_entry in evals {
        if skip_ids.contains(&eval_entry.id) {
            continue;
        }

        out.set_action(format!(
            "{}",
            format!("Fetching eval {}...", eval_entry.id).cyan()
        ));

        let eval_data = match hydra::fetch_eval(client, &cfg.hydra_url, eval_entry.id).await {
            Ok(e) => e,
            Err(e) => {
                out.milestone(format!(
                    "{}",
                    format!("  Failed to fetch eval {}: {e}", eval_entry.id).red()
                ));
                continue;
            }
        };

        let rev = match hydra::extract_eval_rev(cfg, &eval_data) {
            Some(r) => r,
            None => {
                out.milestone(format!(
                    "{}",
                    format!("  Failed to extract rev from eval {}", eval_entry.id).red()
                ));
                continue;
            }
        };

        if !seen_revs.insert(rev.clone()) {
            continue;
        }

        if !candidate_is_allowed(cfg, current_rev, &rev, out, ext).await? {
            continue;
        }

        out.set_action(format!(
            "{}",
            format!(
                "Verifying narinfo at rev {} (eval {})...",
                &rev[..12.min(rev.len())],
                eval_entry.id
            )
            .cyan()
        ));

        let check = narinfo::verify_narinfo_at_rev(client, cfg, &rev, &cfg.packages, ext).await;
        out.milestone(format_rev_summary(
            &rev,
            Some(eval_entry.id),
            &check.results,
        ));
        prior_results.extend(check.results.clone());

        if check.all_cached {
            return Ok(Some(rev));
        }

        if cfg.fail_fast {
            out.finish_err("Fail-fast: aborting");
            return Err(Error::FailFast { rev });
        }
    }

    warn_never_cached(out, cfg, prior_results);
    warn_version_rejections(out, prior_results);
    Ok(None)
}

/// Pure narinfo scan using GitHub commits (fallback when nothing is on Hydra).
pub async fn narinfo_scan<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
) -> Result<Option<String>> {
    narinfo_scan_with_current(client, cfg, out, ext, None).await
}

/// Pure narinfo scan while enforcing the current revision as a monotonic floor.
async fn narinfo_scan_with_current<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<Option<String>> {
    let github_repo = match flakeref::extract_github_repo(&cfg.flake_ref) {
        Some(r) => r.to_string(),
        None => {
            out.milestone(format!(
                "{}",
                "  Narinfo scan requires a github: flake ref for commit listing.".red()
            ));
            return Ok(None);
        }
    };

    out.set_action(format!(
        "{}",
        format!("Fetching recent {} commits...", cfg.branch).cyan()
    ));
    let commits = ext
        .list_commits(&github_repo, &cfg.branch, cfg.depth)
        .await?;

    out.set_action(format!(
        "{}",
        format!("Scanning {} revisions via narinfo...", commits.len()).cyan()
    ));

    let mut all_check_results = Vec::new();

    for (i, rev) in commits.iter().enumerate() {
        let short = &rev[..12.min(rev.len())];
        if !candidate_is_allowed(cfg, current_rev, rev, out, ext).await? {
            continue;
        }
        out.set_action(format!(
            "{}",
            format!("Checking rev {short} ({}/{})...", i + 1, commits.len()).cyan()
        ));

        let check = narinfo::verify_narinfo_at_rev(client, cfg, rev, &cfg.packages, ext).await;
        all_check_results.extend(check.results.clone());

        if check.all_cached {
            out.milestone(format_rev_summary(rev, None, &check.results));
            return Ok(Some(rev.clone()));
        }

        // Only milestone misses every few revs or on last to avoid spam
        out.milestone(format_rev_summary(rev, None, &check.results));

        if cfg.fail_fast {
            out.finish_err(format!("Fail-fast: not cached at rev {short}"));
            return Err(Error::FailFast { rev: rev.clone() });
        }
    }

    out.milestone(format!(
        "{}",
        format!(
            "  No revision found with all packages cached in {} commits.",
            cfg.depth
        )
        .red()
    ));
    warn_never_cached(out, cfg, &all_check_results);
    warn_version_rejections(out, &all_check_results);
    Ok(None)
}

/// Main orchestration with wish-package promotion gates around target search.
pub async fn find_target_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
) -> Result<Option<String>> {
    find_target_rev_with_current(client, cfg, out, ext, None).await
}

/// Main orchestration with a known current pin. Every candidate is checked
/// against the current revision before any cache verification is attempted.
pub async fn find_target_rev_with_current<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<Option<String>> {
    reject_wishes_built_on_hydra(client, cfg, out).await?;

    let target = find_target_rev_inner(client, cfg, out, ext, current_rev).await?;
    if let Some(rev) = &target {
        reject_wishes_cached_at_rev(client, cfg, rev, out, ext).await?;
    }
    Ok(target)
}

/// Find the newest revision satisfying all required packages.
async fn find_target_rev_inner<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<Option<String>> {
    let full_attr_prefix = cfg.full_attr_prefix().to_string();

    // Step 1: Try Hydra for all packages
    out.set_action(format!(
        "{}",
        format!(
            "Querying {} for {full_attr_prefix} builds...",
            cfg.hydra_url
        )
        .cyan()
    ));

    let mut handles = Vec::with_capacity(cfg.packages.len());
    for pkg in &cfg.packages {
        let client = client.clone();
        let cfg = cfg.clone();
        let pkg = pkg.clone();
        handles.push(tokio::spawn(async move {
            hydra::query_hydra_build(&client, &cfg, &pkg).await
        }));
    }

    let mut results = Vec::with_capacity(cfg.packages.len());
    for handle in handles {
        results.push(handle.await.map_err(|source| Error::TaskJoin {
            task: "query_hydra_build",
            source,
        })?);
    }

    let on_hydra: Vec<_> = results
        .iter()
        .filter(|r| r.status == HydraStatus::OnHydra)
        .collect();
    let not_on_hydra: Vec<_> = results
        .iter()
        .filter(|r| r.status == HydraStatus::NotOnHydra)
        .collect();

    // Aggregate Hydra results into one milestone line
    {
        let mut parts = Vec::with_capacity(results.len());
        for r in &on_hydra {
            parts.push(format!("{} {}", r.package, "✓".green()));
        }
        for r in &not_on_hydra {
            parts.push(format!("{} {}", r.package, "✗".yellow()));
        }
        out.set_action(format!("{}", format!("Hydra: {}", parts.join(", ")).cyan()));
    }

    // If nothing is on Hydra, fall back to pure narinfo scan
    if on_hydra.is_empty() {
        out.set_action(format!(
            "{}",
            "No packages on Hydra, falling back to narinfo scan...".cyan()
        ));
        return narinfo_scan_with_current(client, cfg, out, ext, current_rev).await;
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
        out.set_action(format!(
            "{}",
            "No common eval has all Hydra packages — using bottleneck".cyan()
        ));
        let bottleneck = on_hydra
            .iter()
            .filter(|r| !r.evals.is_empty())
            .min_by_key(|r| r.evals[0])
            .unwrap();
        out.set_action(format!(
            "{}",
            format!(
                "Bottleneck: {} at eval {}",
                bottleneck.package, bottleneck.evals[0]
            )
            .cyan()
        ));
        vec![bottleneck.evals[0]]
    };

    // Step 3: Try each candidate eval, verify ALL packages via narinfo
    let mut target_rev = None;
    let mut fast_path_results = Vec::new();

    for eval_id in &candidate_evals {
        out.set_action(format!("{}", format!("Fetching eval {eval_id}...").cyan()));

        let eval_data = match hydra::fetch_eval(client, &cfg.hydra_url, *eval_id).await {
            Ok(e) => e,
            Err(e) => {
                out.milestone(format!(
                    "{}",
                    format!("  Failed to fetch eval {eval_id}: {e}").red()
                ));
                continue;
            }
        };

        let rev = match hydra::extract_eval_rev(cfg, &eval_data) {
            Some(r) => r,
            None => {
                out.milestone(format!(
                    "{}",
                    format!("  Failed to extract rev from eval {eval_id}").red()
                ));
                continue;
            }
        };

        if !candidate_is_allowed(cfg, current_rev, &rev, out, ext).await? {
            continue;
        }

        out.set_action(format!(
            "{}",
            format!("Verifying narinfo at rev {}...", &rev[..12.min(rev.len())]).cyan()
        ));

        let check = narinfo::verify_narinfo_at_rev(client, cfg, &rev, &cfg.packages, ext).await;
        out.milestone(format_rev_summary(&rev, Some(*eval_id), &check.results));
        fast_path_results.extend(check.results.clone());

        if check.all_cached {
            target_rev = Some(rev);
            break;
        }

        if cfg.fail_fast {
            out.finish_err("Fail-fast: aborting");
            return Err(Error::FailFast { rev });
        }
    }

    // Step 4: Broaden search to Hydra jobset eval history
    if target_rev.is_none() {
        out.set_action(format!(
            "{}",
            "No common Hydra eval has all packages cached.".cyan()
        ));
        out.set_action(format!(
            "{}",
            "Broadening search to Hydra jobset eval history...".cyan()
        ));

        let jobset_evals = hydra::query_hydra_jobset_evals(client, cfg, cfg.depth, out).await;
        if !jobset_evals.is_empty() {
            let skip_ids: HashSet<i64> = candidate_evals.iter().copied().collect();
            let broad_result = try_eval_revisions_with_current(
                client,
                cfg,
                &jobset_evals,
                &skip_ids,
                &mut fast_path_results,
                out,
                ext,
                current_rev,
            )
            .await?;
            if broad_result.is_some() {
                return Ok(broad_result);
            }
        }

        out.set_action(format!(
            "{}",
            "No Hydra eval in binary cache, falling back to narinfo scan...".cyan()
        ));
        return narinfo_scan_with_current(client, cfg, out, ext, current_rev).await;
    }

    Ok(target_rev)
}

/// Return whether a candidate is equal to or newer than the current pin.
/// Unknown or divergent history is an error: a cache pin must never silently
/// move to an unproven or older revision.
async fn candidate_is_allowed<E: ExternalCommands + 'static>(
    cfg: &PinConfig,
    current_rev: Option<&str>,
    candidate: &str,
    out: &mut Output,
    ext: &Arc<E>,
) -> Result<bool> {
    let Some(current) = current_rev else {
        return Ok(true);
    };
    if current == candidate {
        return Ok(true);
    }

    let relation = ext
        .compare_revisions(&cfg.flake_ref, &cfg.branch, current, candidate, cfg.depth)
        .await?;
    match relation {
        crate::ext::RevisionOrder::Newer => Ok(true),
        crate::ext::RevisionOrder::Older => {
            out.milestone(format!(
                "  Skipping older candidate {} (current pin is {})",
                &candidate[..12.min(candidate.len())],
                &current[..12.min(current.len())]
            ));
            Ok(false)
        }
        crate::ext::RevisionOrder::Equal => Ok(true),
        crate::ext::RevisionOrder::Divergent => Err(Error::RevisionPolicy {
            current: current.to_string(),
            candidate: candidate.to_string(),
            relation: "divergent history".to_string(),
        }),
        crate::ext::RevisionOrder::Unknown => Err(Error::RevisionOrderUnknown {
            current: current.to_string(),
            candidate: candidate.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::narinfo::PackageCheckResult;
    use crate::output::Output;

    fn cached_result(pkg: &str) -> PackageCheckResult {
        PackageCheckResult {
            package: pkg.to_string(),
            cached: true,
            store_path: Some("/nix/store/hash-pkg".into()),
            error: None,
            version: None,
            version_error: None,
            version_rejected_by: Vec::new(),
        }
    }

    fn miss_result(pkg: &str) -> PackageCheckResult {
        PackageCheckResult {
            package: pkg.to_string(),
            cached: false,
            store_path: Some("/nix/store/hash-pkg".into()),
            error: None,
            version: None,
            version_error: None,
            version_rejected_by: Vec::new(),
        }
    }

    // --- format_rev_summary ---

    #[test]
    fn test_format_rev_summary_all_cached() {
        let results = vec![cached_result("hello"), cached_result("curl")];
        let summary = format_rev_summary("abcdef123456789", Some(42), &results);
        assert!(summary.contains("2/2"));
        assert!(summary.contains("all cached"));
        assert!(summary.contains("abcdef123456"));
        assert!(summary.contains("eval 42"));
    }

    #[test]
    fn test_format_rev_summary_partial_miss() {
        let results = vec![cached_result("hello"), miss_result("blender")];
        let summary = format_rev_summary("abcdef123456789", Some(10), &results);
        assert!(summary.contains("1/2"));
        assert!(summary.contains("blender"));
        assert!(summary.contains("miss"));
    }

    #[test]
    fn test_format_rev_summary_no_eval_id() {
        let results = vec![cached_result("hello")];
        let summary = format_rev_summary("abcdef123456789", None, &results);
        assert!(!summary.contains("eval"));
        assert!(summary.contains("1/1"));
    }

    #[test]
    fn test_format_rev_summary_short_rev() {
        // Rev shorter than 12 chars should not panic
        let results = vec![cached_result("hello")];
        let summary = format_rev_summary("abc", None, &results);
        assert!(summary.contains("abc"));
    }

    #[test]
    fn test_format_rev_summary_all_miss() {
        let results = vec![miss_result("a"), miss_result("b")];
        let summary = format_rev_summary("abcdef123456789", None, &results);
        assert!(summary.contains("0/2"));
        assert!(summary.contains("a"));
        assert!(summary.contains("b"));
    }

    // --- warn_never_cached ---

    fn test_cfg() -> PinConfig {
        PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": ["hello", "blender"],
                "inputName": "nixpkgs",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": ["https://cache.nixos.org"],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn test_warn_never_cached_warns_for_missing_pkg() {
        let cfg = test_cfg();
        let mut out = Output::buffered("test");
        // Only "hello" was ever cached, "blender" was never cached
        let results = vec![cached_result("hello"), miss_result("hello")];
        warn_never_cached(&mut out, &cfg, &results);
        let buf = out.test_buffer();
        // Should warn about "blender" (never appeared as cached)
        assert!(buf.iter().any(|line| line.contains("blender")));
        // Should NOT warn about "hello" (was cached at least once)
        assert!(!buf.iter().any(|line| line.contains("hello")));
    }

    #[test]
    fn test_warn_never_cached_no_warning_when_all_cached() {
        let cfg = test_cfg();
        let mut out = Output::buffered("test");
        let results = vec![cached_result("hello"), cached_result("blender")];
        warn_never_cached(&mut out, &cfg, &results);
        assert!(out.test_buffer().is_empty());
    }

    #[test]
    fn test_warn_never_cached_empty_results() {
        let cfg = test_cfg();
        let mut out = Output::buffered("test");
        warn_never_cached(&mut out, &cfg, &[]);
        assert!(out.test_buffer().is_empty());
    }

    // --- Integration tests with MockCommands + wiremock ---

    use crate::config::VersionConstraint;
    use crate::error::Error;
    use crate::ext::{EvalAttrRequest, ExternalCommands, RevisionOrder};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    type MockKey = (String, String, String);
    type MockValue = std::result::Result<String, String>;
    type MockResultMap = Mutex<HashMap<MockKey, MockValue>>;

    /// Mock implementation of ExternalCommands for testing.
    struct MockCommands {
        /// Maps (flake_ref, rev, pkg) -> store_path or error message
        eval_results: MockResultMap,
        /// Maps (rev, pkg, attr) -> attr value or error message
        attr_results: MockResultMap,
        /// Maps (owner_repo, branch) -> list of commit SHAs
        commit_results: Mutex<HashMap<(String, String), Vec<String>>>,
        /// Maps (current, candidate) -> proven revision relationship.
        revision_results: Mutex<HashMap<(String, String), RevisionOrder>>,
    }

    impl MockCommands {
        fn new() -> Self {
            Self {
                eval_results: Mutex::new(HashMap::new()),
                attr_results: Mutex::new(HashMap::new()),
                commit_results: Mutex::new(HashMap::new()),
                revision_results: Mutex::new(HashMap::new()),
            }
        }

        fn add_eval(&self, rev: &str, pkg: &str, store_path: &str) {
            self.eval_results.lock().unwrap().insert(
                (String::new(), rev.to_string(), pkg.to_string()),
                Ok(store_path.to_string()),
            );
        }

        fn add_attr(&self, rev: &str, pkg: &str, attr: &str, value: &str) {
            self.attr_results.lock().unwrap().insert(
                (rev.to_string(), pkg.to_string(), attr.to_string()),
                Ok(value.to_string()),
            );
        }

        fn add_attr_error(&self, rev: &str, pkg: &str, attr: &str, err: &str) {
            self.attr_results.lock().unwrap().insert(
                (rev.to_string(), pkg.to_string(), attr.to_string()),
                Err(err.to_string()),
            );
        }

        fn add_eval_error(&self, rev: &str, pkg: &str, err: &str) {
            self.eval_results.lock().unwrap().insert(
                (String::new(), rev.to_string(), pkg.to_string()),
                Err(err.to_string()),
            );
        }

        fn add_commits(&self, owner_repo: &str, branch: &str, commits: Vec<String>) {
            self.commit_results
                .lock()
                .unwrap()
                .insert((owner_repo.to_string(), branch.to_string()), commits);
        }

        fn add_revision_order(&self, current: &str, candidate: &str, order: RevisionOrder) {
            self.revision_results
                .lock()
                .unwrap()
                .insert((current.to_string(), candidate.to_string()), order);
        }
    }

    impl ExternalCommands for MockCommands {
        async fn eval_store_path(
            &self,
            _flake_ref: &str,
            rev: &str,
            _arch: &str,
            _flake_output: &str,
            _attr_prefix: &str,
            pkg: &str,
        ) -> crate::error::Result<String> {
            let key = (String::new(), rev.to_string(), pkg.to_string());
            match self.eval_results.lock().unwrap().get(&key) {
                Some(Ok(path)) => Ok(path.clone()),
                Some(Err(msg)) => Err(Error::NixEval {
                    package: pkg.to_string(),
                    stderr: msg.clone(),
                }),
                None => Err(Error::NixEval {
                    package: pkg.to_string(),
                    stderr: format!("no mock eval result for rev={rev} pkg={pkg}"),
                }),
            }
        }

        async fn eval_consumer_store_path(
            &self,
            _consumer_flake_ref: &str,
            _input_name: &str,
            _source_flake_ref: &str,
            rev: &str,
            target: &str,
        ) -> crate::error::Result<String> {
            let key = (String::new(), rev.to_string(), target.to_string());
            match self.eval_results.lock().unwrap().get(&key) {
                Some(Ok(path)) => Ok(path.clone()),
                Some(Err(msg)) => Err(Error::NixEval {
                    package: target.to_string(),
                    stderr: msg.clone(),
                }),
                None => Err(Error::NixEval {
                    package: target.to_string(),
                    stderr: format!("no mock consumer eval result for rev={rev} target={target}"),
                }),
            }
        }

        async fn eval_attr_value(
            &self,
            request: EvalAttrRequest<'_>,
        ) -> crate::error::Result<String> {
            let key = (
                request.rev.to_string(),
                request.pkg.to_string(),
                request.attr.to_string(),
            );
            match self.attr_results.lock().unwrap().get(&key) {
                Some(Ok(value)) => Ok(value.clone()),
                Some(Err(msg)) => Err(Error::NixEval {
                    package: format!("{}.{}", request.pkg, request.attr),
                    stderr: msg.clone(),
                }),
                None => Err(Error::NixEval {
                    package: format!("{}.{}", request.pkg, request.attr),
                    stderr: format!(
                        "no mock attr result for rev={} pkg={} attr={}",
                        request.rev, request.pkg, request.attr
                    ),
                }),
            }
        }

        async fn list_commits(
            &self,
            owner_repo: &str,
            branch: &str,
            _depth: usize,
        ) -> crate::error::Result<Vec<String>> {
            let key = (owner_repo.to_string(), branch.to_string());
            match self.commit_results.lock().unwrap().get(&key) {
                Some(commits) => Ok(commits.clone()),
                None => Ok(vec![]),
            }
        }

        async fn compare_revisions(
            &self,
            _flake_ref: &str,
            _branch: &str,
            current: &str,
            candidate: &str,
            _depth: usize,
        ) -> crate::error::Result<RevisionOrder> {
            Ok(self
                .revision_results
                .lock()
                .unwrap()
                .get(&(current.to_string(), candidate.to_string()))
                .copied()
                .unwrap_or(RevisionOrder::Unknown))
        }

        async fn run_flake_lock(&self, _input_name: &str) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn cfg_with_url(hydra_url: &str, caches: Vec<String>) -> PinConfig {
        PinConfig::from_json(&format!(
            r#"{{
                "name": "test",
                "packages": ["hello"],
                "inputName": "nixpkgs",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": {caches},
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "{hydra_url}",
                "hydraJobPattern": "{{jobset}}/{{pkg}}.{{arch}}",
                "hydraRevInput": "nixpkgs",
                "depth": 5,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }}"#,
            caches = serde_json::to_string(&caches).unwrap(),
        ))
        .unwrap()
    }

    const REV: &str = "abcdef1234567890abcdef1234567890abcdef12";

    #[tokio::test]
    async fn current_pin_rejects_older_candidate_before_cache_checks() {
        let cfg = cfg_with_url("https://hydra.nixos.org", vec![]);
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_revision_order("current", "older", RevisionOrder::Older);
        let mut out = Output::buffered("test");

        let allowed = candidate_is_allowed(&cfg, Some("current"), "older", &mut out, &mock_ext)
            .await
            .unwrap();

        assert!(!allowed);
    }

    #[tokio::test]
    async fn current_pin_refuses_unknown_revision_relationship() {
        let cfg = cfg_with_url("https://hydra.nixos.org", vec![]);
        let mock_ext = Arc::new(MockCommands::new());
        let mut out = Output::buffered("test");

        let error = candidate_is_allowed(&cfg, Some("current"), "unknown", &mut out, &mock_ext)
            .await
            .unwrap_err();

        assert!(matches!(error, Error::RevisionOrderUnknown { .. }));
    }

    #[tokio::test]
    async fn test_find_target_rev_hydra_happy_path() {
        // Setup: Hydra says hello is built, eval returns our rev, narinfo confirms cache hit
        let hydra = MockServer::start().await;
        let cache = MockServer::start().await;

        // Hydra build query -> OnHydra with eval 100
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobsetevals": [100],
                "buildoutputs": { "out": { "path": "/nix/store/abc123-hello" } }
            })))
            .mount(&hydra)
            .await;

        // Hydra eval fetch -> returns our rev via flake field
        // Bounded-read path does not download jobsetevalinputs, so the
        // revision is extracted from the flake URI instead.
        Mock::given(method("GET"))
            .and(path("/eval/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100,
                "flake": format!("github:NixOS/nixpkgs/{REV}"),
            })))
            .mount(&hydra)
            .await;

        // Cache narinfo hit — hash is everything before first '-' in basename
        Mock::given(method("HEAD"))
            .and(path("/mockhash.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let cfg = cfg_with_url(&hydra.uri(), vec![cache.uri()]);
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval(REV, "hello", "/nix/store/mockhash-hello");

        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = find_target_rev(&client, &cfg, &mut out, &mock_ext).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(REV.to_string()));
    }

    #[tokio::test]
    async fn test_find_target_rev_all_miss_returns_none() {
        // Hydra says hello is built, but narinfo check fails (cache miss)
        let hydra = MockServer::start().await;
        let cache = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobsetevals": [100],
                "buildoutputs": { "out": { "path": "/nix/store/abc123-hello" } }
            })))
            .mount(&hydra)
            .await;

        Mock::given(method("GET"))
            .and(path("/eval/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100,
                "flake": format!("github:NixOS/nixpkgs/{REV}"),
            })))
            .mount(&hydra)
            .await;

        // Empty jobset evals for broadened search
        Mock::given(method("GET"))
            .and(path("/jobset/nixpkgs/trunk/evals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "evals": []
            })))
            .mount(&hydra)
            .await;

        // Cache always returns 404
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&cache)
            .await;

        let cfg = cfg_with_url(&hydra.uri(), vec![cache.uri()]);
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval(REV, "hello", "/nix/store/mock-hash-hello");
        // Also mock commits for the narinfo fallback scan
        mock_ext.add_commits(
            "NixOS/nixpkgs",
            "nixpkgs-unstable",
            vec![
                "1111111111111111111111111111111111111111".into(),
                "2222222222222222222222222222222222222222".into(),
            ],
        );
        mock_ext.add_eval(
            "1111111111111111111111111111111111111111",
            "hello",
            "/nix/store/hash1-hello",
        );
        mock_ext.add_eval(
            "2222222222222222222222222222222222222222",
            "hello",
            "/nix/store/hash2-hello",
        );

        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = find_target_rev(&client, &cfg, &mut out, &mock_ext).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_find_target_rev_narinfo_fallback_succeeds() {
        // Nothing on Hydra -> falls back to narinfo scan -> finds cached commit
        let hydra = MockServer::start().await;
        let cache = MockServer::start().await;

        // Hydra returns 404 for the build (not on Hydra)
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&hydra)
            .await;

        // Cache hit for the second commit's store path
        Mock::given(method("HEAD"))
            .and(path("/hash2.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;
        // Miss for first commit
        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&cache)
            .await;

        let cfg = cfg_with_url(&hydra.uri(), vec![cache.uri()]);
        let mock_ext = Arc::new(MockCommands::new());
        let commit1 = "1111111111111111111111111111111111111111";
        let commit2 = "2222222222222222222222222222222222222222";
        mock_ext.add_commits(
            "NixOS/nixpkgs",
            "nixpkgs-unstable",
            vec![commit1.into(), commit2.into()],
        );
        mock_ext.add_eval(commit1, "hello", "/nix/store/hash1-hello");
        mock_ext.add_eval(commit2, "hello", "/nix/store/hash2-hello");

        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = find_target_rev(&client, &cfg, &mut out, &mock_ext).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(commit2.to_string()));
    }

    #[tokio::test]
    async fn test_find_target_rev_fail_fast() {
        let hydra = MockServer::start().await;
        let cache = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobsetevals": [100],
                "buildoutputs": { "out": { "path": "/nix/store/abc123-hello" } }
            })))
            .mount(&hydra)
            .await;

        Mock::given(method("GET"))
            .and(path("/eval/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100,
                "flake": format!("github:NixOS/nixpkgs/{REV}"),
            })))
            .mount(&hydra)
            .await;

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&cache)
            .await;

        let mut cfg = cfg_with_url(&hydra.uri(), vec![cache.uri()]);
        cfg.fail_fast = true;

        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval(REV, "hello", "/nix/store/mock-hash-hello");

        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = find_target_rev(&client, &cfg, &mut out, &mock_ext).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::FailFast { rev } => assert_eq!(rev, REV),
            other => panic!("expected FailFast, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_wish_package_on_hydra_blocks_update() {
        let hydra = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/wished.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobsetevals": [100],
                "buildoutputs": { "out": { "path": "/nix/store/hash-wished" } }
            })))
            .mount(&hydra)
            .await;

        let mut cfg = cfg_with_url(&hydra.uri(), vec![]);
        cfg.wish_packages = vec!["wished".into()];
        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = reject_wishes_built_on_hydra(&client, &cfg, &mut out).await;

        match result.unwrap_err() {
            Error::WishPackagesBuilt { location, packages } => {
                assert_eq!(location, "Hydra latest-finished");
                assert_eq!(packages, "wished");
            }
            other => panic!("expected WishPackagesBuilt, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_wish_package_in_cache_blocks_even_with_fail_fast() {
        let cache = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/missinghash.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&cache)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/builthash.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let mut cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        cfg.fail_fast = true;
        cfg.wish_packages = vec!["missing".into(), "built".into()];
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "missing", "/nix/store/missinghash-pkg");
        mock_ext.add_eval("rev1", "built", "/nix/store/builthash-pkg");

        let client = reqwest::Client::new();
        let mut out = Output::buffered("test");
        let result = reject_wishes_cached_at_rev(&client, &cfg, "rev1", &mut out, &mock_ext).await;

        match result.unwrap_err() {
            Error::WishPackagesBuilt { location, packages } => {
                assert!(location.contains("rev1"));
                assert_eq!(packages, "built");
            }
            other => panic!("expected WishPackagesBuilt, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_verify_narinfo_all_cached() {
        let cache = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/hash2.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "hello", "/nix/store/hash1-hello");
        mock_ext.add_eval("rev1", "curl", "/nix/store/hash2-curl");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into(), "curl".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(result.all_cached);
        assert_eq!(result.results.len(), 2);
        assert!(result.results.iter().all(|r| r.cached));
    }

    #[tokio::test]
    async fn test_verify_narinfo_partial_miss() {
        let cache = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/hash2.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&cache)
            .await;

        let cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "hello", "/nix/store/hash1-hello");
        mock_ext.add_eval("rev1", "curl", "/nix/store/hash2-curl");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into(), "curl".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(!result.all_cached);
        let cached_count = result.results.iter().filter(|r| r.cached).count();
        assert_eq!(cached_count, 1);
    }

    #[tokio::test]
    async fn test_verify_narinfo_eval_error() {
        let cfg = cfg_with_url(
            "https://hydra.nixos.org",
            vec!["https://cache.nixos.org".into()],
        );
        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval_error("rev1", "hello", "attribute not found");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(!result.all_cached);
        assert!(!result.results.is_empty());
        assert!(result.results[0].error.is_some());
    }

    #[tokio::test]
    async fn test_verify_narinfo_version_target_rejects() {
        let cache = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let mut cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        cfg.version_constraints.insert(
            "hello".into(),
            VersionConstraint {
                target: Some("< 7.0.8".into()),
                taints: Vec::new(),
                version_attr: "version".into(),
            },
        );

        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "hello", "/nix/store/hash1-hello");
        mock_ext.add_attr("rev1", "hello", "version", "7.0.9");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(!result.all_cached);
        assert!(result.results[0].cached);
        assert_eq!(result.results[0].version.as_deref(), Some("7.0.9"));
        assert_eq!(result.results[0].version_rejected_by.len(), 1);
    }

    #[tokio::test]
    async fn test_verify_narinfo_version_taint_rejects() {
        let cache = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let mut cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        cfg.version_constraints.insert(
            "hello".into(),
            VersionConstraint {
                target: None,
                taints: vec![">= 7.0.8".into()],
                version_attr: "version".into(),
            },
        );

        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "hello", "/nix/store/hash1-hello");
        mock_ext.add_attr("rev1", "hello", "version", "7.0.9");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(!result.all_cached);
        assert!(result.results[0].cached);
        assert_eq!(result.results[0].version_rejected_by.len(), 1);
    }

    #[tokio::test]
    async fn test_verify_narinfo_version_eval_failure_rejects() {
        let cache = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/hash1.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cache)
            .await;

        let mut cfg = cfg_with_url("https://hydra.nixos.org", vec![cache.uri()]);
        cfg.version_constraints.insert(
            "hello".into(),
            VersionConstraint {
                target: Some("< 7.0.8".into()),
                taints: Vec::new(),
                version_attr: "version".into(),
            },
        );

        let mock_ext = Arc::new(MockCommands::new());
        mock_ext.add_eval("rev1", "hello", "/nix/store/hash1-hello");
        mock_ext.add_attr_error("rev1", "hello", "version", "attribute missing");

        let client = reqwest::Client::new();
        let packages = vec!["hello".into()];
        let result =
            narinfo::verify_narinfo_at_rev(&client, &cfg, "rev1", &packages, &mock_ext).await;

        assert!(!result.all_cached);
        assert!(result.results[0].cached);
        assert!(result.results[0].version_error.is_some());
    }
}
