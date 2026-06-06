use crate::config::PinConfig;
use crate::error::Error;
use crate::ext::ExternalCommands;
use crate::flake_update;
use crate::orchestrate;
use crate::output::Output;
use colored::Colorize;
use indicatif::MultiProgress;
use std::path::Path;
use std::sync::Arc;

/// Result of the find phase for a single pin.
pub struct FindResult {
    pub config: PinConfig,
    pub output: Output,
    pub target_rev: Result<Option<String>, Error>,
}

/// Result of applying a pin update.
pub struct ApplyOutcome {
    pub name: String,
    pub updated: bool,
    pub from_rev: Option<String>,
    pub to_rev: Option<String>,
    pub error: Option<String>,
}

/// Run `find_target_rev` for all configs in parallel.
///
/// When `use_spinner` is true, creates a `MultiProgress` with one spinner per
/// pin (for TTY). Otherwise, falls back to buffered output.
pub async fn find_all<E: ExternalCommands + 'static>(
    configs: Vec<PinConfig>,
    use_spinner: bool,
    ext: &Arc<E>,
) -> Vec<FindResult> {
    let mp = if use_spinner {
        Some(MultiProgress::new())
    } else {
        None
    };

    let mut handles = Vec::with_capacity(configs.len());
    for cfg in configs {
        let mp = mp.clone();
        let ext = Arc::clone(ext);
        let fallback_config = cfg.clone();
        let handle = tokio::spawn(async move {
            let mut out = match &mp {
                Some(mp) => Output::spinner_in(&cfg.name, mp),
                None => Output::buffered(&cfg.name),
            };
            print_config_header(&mut out, &cfg);
            let client = reqwest::Client::new();
            let result = orchestrate::find_target_rev(&client, &cfg, &mut out, &ext).await;

            // Finish spinner with result status
            match &result {
                Ok(Some(rev)) => {
                    let short = &rev[..12.min(rev.len())];
                    out.finish_ok(format!("{}: pinned to {short}", cfg.name));
                }
                Ok(None) => {
                    out.finish_err(format!("{}: no cached revision found", cfg.name));
                }
                Err(e) => {
                    out.finish_err(format!("{}: {e}", cfg.name));
                }
            }

            FindResult {
                config: cfg,
                output: out,
                target_rev: result,
            }
        });
        handles.push((fallback_config, handle));
    }

    let mut results = Vec::with_capacity(handles.len());
    for (config, handle) in handles {
        let join_result = handle.await;
        let fr = match join_result {
            Ok(fr) => fr,
            Err(source) => FindResult {
                config,
                output: Output::buffered("find_target_rev"),
                target_rev: Err(Error::TaskJoin {
                    task: "find_target_rev",
                    source,
                }),
            },
        };
        // Flush is only relevant for buffered (non-TTY) mode
        fr.output.flush();
        results.push(fr);
    }
    results
}

/// Apply a found revision: update flake.nix and optionally run `nix flake lock`.
pub async fn apply<E: ExternalCommands + 'static>(
    cfg: &PinConfig,
    target_rev: &str,
    dry_run: bool,
    no_lock: bool,
    ext: &Arc<E>,
) -> ApplyOutcome {
    let flake_nix_path = Path::new("flake.nix");

    let current_rev = match tokio::fs::read_to_string(flake_nix_path)
        .await
        .map_err(Error::from)
        .and_then(|content| {
            flake_update::read_current_rev(&content, &cfg.input_name, &cfg.flake_ref)
        }) {
        Ok(rev) => rev,
        Err(e) => {
            return ApplyOutcome {
                name: cfg.name.clone(),
                updated: false,
                from_rev: None,
                to_rev: Some(target_rev.to_string()),
                error: Some(format!("failed to read current rev: {e}")),
            };
        }
    };

    eprintln!("\n{}", format!("  {} pin:", cfg.name).cyan().bold());
    eprintln!("  Current: {current_rev}");
    eprintln!("  Target:  {target_rev}");

    if current_rev == target_rev {
        eprintln!("  {}", "Already up to date.".green());
        return ApplyOutcome {
            name: cfg.name.clone(),
            updated: false,
            from_rev: Some(current_rev),
            to_rev: Some(target_rev.to_string()),
            error: None,
        };
    }

    if dry_run {
        eprintln!(
            "  {}",
            format!("Would update {} to {target_rev} (dry run)", cfg.input_name).magenta()
        );
        return ApplyOutcome {
            name: cfg.name.clone(),
            updated: false,
            from_rev: Some(current_rev),
            to_rev: Some(target_rev.to_string()),
            error: None,
        };
    }

    if let Err(e) = flake_update::update_flake_nix_async(
        flake_nix_path,
        &cfg.flake_ref,
        &current_rev,
        target_rev,
    )
    .await
    {
        return ApplyOutcome {
            name: cfg.name.clone(),
            updated: false,
            from_rev: Some(current_rev),
            to_rev: Some(target_rev.to_string()),
            error: Some(format!("failed to update flake.nix: {e}")),
        };
    }
    eprintln!(
        "  {}",
        format!("Updated {} to {target_rev}", cfg.input_name).green()
    );

    if !no_lock {
        eprintln!(
            "  Running nix flake lock --update-input {}...",
            cfg.input_name
        );
        if let Err(e) = ext.run_flake_lock(&cfg.input_name).await {
            return ApplyOutcome {
                name: cfg.name.clone(),
                updated: true,
                from_rev: Some(current_rev),
                to_rev: Some(target_rev.to_string()),
                error: Some(format!("flake lock failed: {e}")),
            };
        }
        eprintln!("  {}", "Lock file updated.".green());
    }

    ApplyOutcome {
        name: cfg.name.clone(),
        updated: true,
        from_rev: Some(current_rev),
        to_rev: Some(target_rev.to_string()),
        error: None,
    }
}

fn print_config_header(out: &mut Output, cfg: &PinConfig) {
    let full_attr_prefix = cfg.full_attr_prefix().to_string();
    out.milestone(format!(
        "{}",
        format!("nix-cache-pin: {}", cfg.name).cyan().bold()
    ));
    out.milestone(format!(
        "  input: {} | hydra: {} | attr: {} | arch: {}",
        cfg.input_name, cfg.hydra_url, full_attr_prefix, cfg.arch
    ));
    out.milestone(format!("  packages: {}\n", cfg.packages.join(", ")));
}
