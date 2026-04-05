use crate::config::PinConfig;
use crate::error::Error;
use crate::flake_update;
use crate::orchestrate;
use crate::output::Output;
use colored::Colorize;
use std::path::Path;
use tokio::task::JoinSet;

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

/// Run `find_target_rev` for all configs in parallel, flushing each pin's
/// buffered output to stderr in completion order.
pub async fn find_all(configs: Vec<PinConfig>) -> Vec<FindResult> {
    let mut set = JoinSet::new();
    for cfg in configs {
        set.spawn(async move {
            let mut out = Output::buffered(&cfg.name);
            print_config_header(&mut out, &cfg);
            let client = reqwest::Client::new();
            let result = orchestrate::find_target_rev(&client, &cfg, &mut out).await;
            FindResult {
                config: cfg,
                output: out,
                target_rev: result,
            }
        });
    }

    let mut results = Vec::new();
    while let Some(join_result) = set.join_next().await {
        let fr = join_result.expect("find_target_rev task panicked");
        fr.output.flush();
        results.push(fr);
    }
    results
}

/// Apply a found revision: update flake.nix and optionally run `nix flake lock`.
pub async fn apply(
    cfg: &PinConfig,
    target_rev: &str,
    dry_run: bool,
    no_lock: bool,
) -> ApplyOutcome {
    let flake_nix_path = Path::new("flake.nix");

    let current_rev = match std::fs::read_to_string(flake_nix_path)
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

    if let Err(e) =
        flake_update::update_flake_nix(flake_nix_path, &cfg.flake_ref, &current_rev, target_rev)
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
        if let Err(e) = flake_update::run_flake_lock(&cfg.input_name).await {
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
    out.println(format!(
        "{}",
        format!("nix-cache-pin: {}", cfg.name).cyan().bold()
    ));
    out.println(format!("  input:       {}", cfg.input_name));
    out.println(format!("  flake ref:   {}", cfg.flake_ref));
    out.println(format!("  hydra:       {}", cfg.hydra_url));
    out.println(format!("  attr prefix: {full_attr_prefix}"));
    out.println(format!("  arch:        {}", cfg.arch));
    out.println(format!("  packages:    {}\n", cfg.packages.join(", ")));
}
