use crate::config::PinConfig;
use crate::narinfo::PackageCheckResult;
use crate::output::Output;
use colored::Colorize;
use std::collections::HashSet;

/// Format a one-line summary of narinfo check results for a revision.
pub(super) fn format_rev_summary(
    rev: &str,
    eval_id: Option<i64>,
    results: &[PackageCheckResult],
) -> String {
    let cached = results.iter().filter(|r| r.cached).count();
    let total = results.len();
    let short = &rev[..12.min(rev.len())];

    let eval_tag = match eval_id {
        Some(id) => format!(" (eval {id})"),
        None => String::new(),
    };

    if cached == total {
        format!(
            "  Rev {short}{eval_tag}: {}/{total} {}",
            cached,
            "all cached".green()
        )
    } else {
        let misses: Vec<&str> = results
            .iter()
            .filter(|r| !r.accepted())
            .map(|r| r.package.as_str())
            .collect();
        let version_failures: Vec<String> = results
            .iter()
            .filter_map(|r| {
                if !r.version_rejected_by.is_empty() {
                    Some(format!(
                        "{} version {} rejected by {}",
                        r.package,
                        r.version.as_deref().unwrap_or("unknown"),
                        r.version_rejected_by.join("; ")
                    ))
                } else {
                    r.version_error
                        .as_ref()
                        .map(|err| format!("{} version check failed: {err}", r.package))
                }
            })
            .collect();
        let version_note = if version_failures.is_empty() {
            String::new()
        } else {
            format!(" — {}", version_failures.join("; "))
        };
        format!(
            "  Rev {short}{eval_tag}: {cached}/{total} cached — {}: {}{}",
            "miss".red(),
            misses.join(", "),
            version_note
        )
    }
}

/// Warn about packages never cached at any revision.
pub(super) fn warn_never_cached(
    out: &mut Output,
    cfg: &PinConfig,
    all_results: &[PackageCheckResult],
) {
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
            out.milestone(format!(
                "{}",
                format!(
                    "  Warning: {pkg} was not cached at any revision tried \
                     (may be dropped from caches or flake eval diverges)"
                )
                .yellow()
            ));
        }
    }
}

pub(super) fn warn_version_rejections(out: &mut Output, all_results: &[PackageCheckResult]) {
    let mut seen = HashSet::new();
    for result in all_results {
        if let Some(err) = &result.version_error {
            let message = format!("{} version check failed: {err}", result.package);
            if seen.insert(message.clone()) {
                out.milestone(format!("{}", format!("  Warning: {message}").yellow()));
            }
        }
        if !result.version_rejected_by.is_empty() {
            let message = format!(
                "{} version {} rejected by {}",
                result.package,
                result.version.as_deref().unwrap_or("unknown"),
                result.version_rejected_by.join("; ")
            );
            if seen.insert(message.clone()) {
                out.milestone(format!("{}", format!("  Warning: {message}").yellow()));
            }
        }
    }
}
