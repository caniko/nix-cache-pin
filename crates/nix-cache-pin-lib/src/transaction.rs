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
    if dry_run {
        return Ok(());
    }

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
            staged_flake_nix = flake_update::replace_rev(
                &staged_flake_nix,
                &cfg.flake_ref,
                &current_rev,
                target_rev,
            );
        }

        if !no_lock {
            let staged_lock = ensure_staged_lock(&mut files, lock_path)?;
            let candidate = flakeref::append_rev(&cfg.flake_ref, target_rev);
            flake_update::update_flake_lock_only(staged_lock, &cfg.input_name, &candidate).await?;
        }
    }

    if staged_flake_nix != original_flake_nix {
        let staged = unique_path(flake_nix_path, "flake-nix")?;
        tokio::fs::write(&staged, staged_flake_nix).await?;
        staged_outputs.push((staged, flake_nix_path.to_path_buf()));
    }

    if let Some(staged_lock) = files.staged_lock.take() {
        staged_outputs.push((staged_lock, lock_path.to_path_buf()));
    }

    if !no_lock {
        let staged_manifest = unique_path(Path::new("cache-pin.lock.json"), "manifest")?;
        manifest::write(&staged_manifest, groups, &revisions(successes))?;
        staged_outputs.push((staged_manifest, PathBuf::from("cache-pin.lock.json")));
    }

    if !staged_outputs.is_empty() {
        commit(staged_outputs)?;
    }
    files.disarm();
    Ok(())
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
            let backup = unique_path(destination, "backup")?;
            std::fs::rename(destination, &backup)?;
            backups.push((destination.clone(), backup));
        }
    }

    for (staged, destination) in &files {
        if let Err(error) = std::fs::rename(staged, destination) {
            for (destination, backup) in backups.iter().rev() {
                let _ = std::fs::remove_file(destination);
                let _ = std::fs::rename(backup, destination);
            }
            return Err(error.into());
        }
    }

    for (_, backup) in backups {
        let _ = std::fs::remove_file(backup);
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
}
