use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::ext::ExternalCommands;
use crate::flake_update;
use crate::flakeref;
use crate::manifest;
use crate::merge::PinGroup;
use crate::runner;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Apply all found revisions as one recoverable filesystem transaction.
///
/// The individual file renames are atomic, while the backup/rollback phase
/// keeps a failed multi-file commit from leaving a half-updated consuming
/// flake behind.
pub async fn apply<E: ExternalCommands + 'static>(
    groups: &[PinGroup],
    successes: &[(PinConfig, String)],
    dry_run: bool,
    no_lock: bool,
    update: bool,
    ext: &Arc<E>,
) -> Result<()> {
    apply_with_evidence(groups, successes, dry_run, no_lock, update, ext)
        .await
        .map(|_| ())
}

/// The final-graph checks are returned on success; on failure they are emitted
/// as typed JSON before the error, without committing consuming files.
pub async fn apply_with_evidence<E: ExternalCommands + 'static>(
    groups: &[PinGroup],
    successes: &[(PinConfig, String)],
    dry_run: bool,
    no_lock: bool,
    update: bool,
    ext: &Arc<E>,
) -> Result<Vec<crate::narinfo::VerifyResult>> {
    if dry_run {
        return Ok(Vec::new());
    }
    let guard = crate::mutation::Mutation::acquire(Path::new("."))?;
    let baselines = ["flake.nix", "flake.lock", "cache-pin.lock.json"]
        .into_iter()
        .map(|p| Ok((PathBuf::from(p), crate::mutation::read(Path::new(p))?)))
        .collect::<std::io::Result<Vec<_>>>()?;

    if update {
        eprintln!("Applying cache pins transactionally...");
    }

    let flake_nix_path = Path::new("flake.nix");
    let lock_path = Path::new("flake.lock");
    let original_flake_nix = tokio::fs::read_to_string(flake_nix_path).await?;
    let mut staged_flake_nix = original_flake_nix.clone();
    let mut files = TempFiles::new();
    let mut staged_outputs = Vec::new();

    for (cfg, target_rev) in successes {
        let current_rev = runner::current_revision(cfg)?;
        eprintln!("\n  {} pin:", cfg.name);
        eprintln!("  Current: {current_rev}");
        eprintln!("  Target:  {target_rev}");

        if current_rev == *target_rev {
            eprintln!("  Already up to date.");
            continue;
        }

        runner::validate_revision_order(cfg, &current_rev, target_rev, ext).await?;

        if cfg.lock_only {
            if no_lock {
                eprintln!("  Skipping lock-only update because --no-lock was supplied.");
                continue;
            }
        } else {
            staged_flake_nix = flake_update::replace_input_rev(
                &staged_flake_nix,
                &cfg.input_name,
                &cfg.flake_ref,
                &current_rev,
                target_rev,
            )?;
        }

        if !no_lock {
            let staged_lock = ensure_staged_lock(&mut files, lock_path)?;
            let candidate = flakeref::append_rev(&cfg.flake_ref, target_rev);
            let _lease = ext.acquire_flake_lock_lease().await?;
            flake_update::update_flake_lock_held(
                staged_lock,
                &cfg.input_name,
                &candidate,
                (!cfg.lock_only).then_some(target_rev.as_str()),
                &guard,
            )
            .await?;
        }
    }

    if staged_flake_nix != original_flake_nix {
        let staged = unique_path(flake_nix_path, "flake-nix")?;
        tokio::fs::write(&staged, &staged_flake_nix).await?;
        files.cleanup.push(staged.clone());
        staged_outputs.push((staged, flake_nix_path.to_path_buf()));
    }

    let mut final_evidence = Vec::new();
    if !no_lock {
        let mut final_configs = Vec::new();
        for group in groups {
            let revision = successes
                .iter()
                .find(|(cfg, _)| cfg.input_name == group.input_name)
                .map(|(_, rev)| rev.clone())
                .ok_or_else(|| {
                    Error::Config(format!(
                        "missing selected revision for required input {}",
                        group.input_name
                    ))
                })?;
            final_configs.push((group.merged.clone(), revision));
        }
        let locked = files.staged_lock.as_deref().unwrap_or(lock_path);
        let snapshot = crate::final_graph::snapshot(locked)?;
        std::fs::write(snapshot.path().join("flake.nix"), &staged_flake_nix)?;
        let reference = format!("git+file://{}", snapshot.path().display());
        final_evidence = crate::final_graph::verify(&final_configs, &reference, ext).await;
        for check in &final_evidence {
            for result in &check.results {
                eprintln!(
                    "  Final graph {}: {} {} {}",
                    check.rev,
                    result.package,
                    result.availability.as_str(),
                    result.failure().unwrap_or_default()
                );
            }
        }
        eprintln!(
            "Final graph evidence: {}",
            serde_json::to_string(&final_evidence)?
        );
        if final_evidence.iter().any(|v| !v.all_cached) {
            return Err(Error::FinalGraph {
                evidence: final_evidence,
                candidate_lock: std::fs::read_to_string(locked)?,
            });
        }
    }

    if let Some(staged_lock) = files.staged_lock.take() {
        staged_outputs.push((staged_lock, lock_path.to_path_buf()));
    }

    if !no_lock {
        let staged_manifest = unique_path(Path::new("cache-pin.lock.json"), "manifest")?;
        files.cleanup.push(staged_manifest.clone());
        manifest::write_held(
            &staged_manifest,
            Path::new("cache-pin.lock.json"),
            groups,
            &revisions(successes),
            &guard,
        )?;
        staged_outputs.push((staged_manifest, PathBuf::from("cache-pin.lock.json")));
    }

    if !staged_outputs.is_empty() {
        for (path, before) in &baselines {
            crate::mutation::unchanged(path, before)?;
        }
        commit(staged_outputs)?;
    }
    files.disarm();
    Ok(final_evidence)
}

fn revisions(successes: &[(PinConfig, String)]) -> Vec<(String, String)> {
    successes
        .iter()
        .map(|(cfg, revision)| (cfg.input_name.clone(), revision.clone()))
        .collect()
}

struct TempFiles {
    staged_lock: Option<PathBuf>,
    cleanup: Vec<PathBuf>,
}

impl TempFiles {
    fn new() -> Self {
        Self {
            staged_lock: None,
            cleanup: Vec::new(),
        }
    }

    fn disarm(&mut self) {
        self.cleanup.clear();
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.cleanup {
            let _ = std::fs::remove_file(path);
        }
        if let Some(path) = &self.staged_lock {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn ensure_staged_lock<'a>(files: &'a mut TempFiles, lock_path: &Path) -> Result<&'a Path> {
    if files.staged_lock.is_none() {
        let staged = unique_path(lock_path, "lock")?;
        std::fs::copy(lock_path, &staged)?;
        files.cleanup.push(staged.clone());
        files.staged_lock = Some(staged);
    }
    Ok(files
        .staged_lock
        .as_deref()
        .expect("staged lock was just created"))
}

fn unique_path(path: &Path, kind: &str) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::FlakeNix(format!("system clock is before UNIX_EPOCH: {e}")))?
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("flake");
    let candidate = path.with_file_name(format!(
        ".{file_name}.cache-pin-{kind}-{stamp}-{}",
        std::process::id()
    ));
    if candidate.exists() {
        return Err(Error::FlakeNix(format!(
            "temporary cache-pin path already exists: {}",
            candidate.display()
        )));
    }
    Ok(candidate)
}

fn commit(files: Vec<(PathBuf, PathBuf)>) -> Result<()> {
    let mut backups = Vec::with_capacity(files.len());
    for (_, destination) in &files {
        if destination.exists() {
            let backup = match unique_path(destination, "backup") {
                Ok(backup) => backup,
                Err(error) => {
                    restore(&backups)?;
                    return Err(error);
                }
            };
            if let Err(error) = std::fs::rename(destination, &backup) {
                restore(&backups)?;
                return Err(error.into());
            }
            backups.push((destination.clone(), backup));
        }
    }

    let mut installed = Vec::new();
    for (staged, destination) in &files {
        if let Err(error) = std::fs::rename(staged, destination) {
            for path in installed.iter().rev() {
                std::fs::remove_file(path)?;
            }
            restore(&backups)?;
            return Err(error.into());
        }
        installed.push(destination.clone());
    }

    for (_, backup) in backups {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

fn restore(backups: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (destination, backup) in backups.iter().rev() {
        std::fs::rename(backup, destination)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_replaces_a_destination() {
        let directory =
            std::env::temp_dir().join(format!("cache-pin-transaction-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("destination");
        let staged = directory.join("staged");
        std::fs::write(&destination, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();

        commit(vec![(staged, destination.clone())]).unwrap();

        assert_eq!(std::fs::read_to_string(destination).unwrap(), "new");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_install_restores_existing_files_and_removes_new_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("existing");
        let new_output = directory.path().join("new-output");
        let staged_existing = directory.path().join("staged-existing");
        let staged_new = directory.path().join("staged-new");
        std::fs::write(&existing, "user baseline").unwrap();
        std::fs::write(&staged_existing, "replacement").unwrap();
        std::fs::write(&staged_new, "new").unwrap();
        assert!(commit(vec![
            (staged_existing, existing.clone()),
            (staged_new, new_output.clone()),
            (
                directory.path().join("missing"),
                directory.path().join("last")
            ),
        ])
        .is_err());
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "user baseline");
        assert!(!new_output.exists());
    }
}
