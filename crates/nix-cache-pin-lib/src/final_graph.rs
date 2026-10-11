//! Verify the exact combined lock before any consuming files are committed.
use crate::{
    config::PinConfig,
    error::{Error, Result},
    ext::ExternalCommands,
    narinfo,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A source-only package must also be evaluated through the consumer input graph.
pub fn consumer_config(cfg: &PinConfig, flake_ref: &str) -> PinConfig {
    let mut final_cfg = cfg.clone();
    final_cfg.consumer_flake_ref = Some(flake_ref.into());
    for package in cfg.packages.iter().chain(&cfg.wish_packages) {
        final_cfg
            .consumer_targets
            .entry(package.clone())
            .or_insert_with(|| {
                let attr = narinfo::build_attr(cfg.full_attr_prefix(), package);
                format!(
                    "inputs.{}.{}.{}.{attr}",
                    cfg.input_name, cfg.flake_output, cfg.arch
                )
            });
    }
    final_cfg.fail_fast = false;
    final_cfg
}

/// Returns typed evidence, including failed checks. Caller must refuse commit
/// unless every verification is accepted.
pub async fn verify<E: ExternalCommands + 'static>(
    configs: &[(PinConfig, String)],
    flake_ref: &str,
    ext: &Arc<E>,
) -> Vec<narinfo::VerifyResult> {
    let client = reqwest::Client::new();
    let mut evidence = Vec::new();
    for (cfg, rev) in configs {
        evidence.push(
            narinfo::verify_current(&client, &consumer_config(cfg, flake_ref), rev, ext).await,
        );
    }
    evidence
}

/// Snapshot only Git-selected source files; never copy ignored build artifacts.
/// The directory lives outside the checkout and is removed on drop.
pub(crate) fn snapshot(lock: &Path) -> Result<tempfile::TempDir> {
    let files = std::process::Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    if !files.status.success() {
        return Err(Error::FlakeNix(
            "cannot enumerate final candidate source".into(),
        ));
    }
    let temp_root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::Builder::new()
        .prefix("cache-pin-final-")
        .tempdir_in(temp_root)?;
    for name in files.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(name));
        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return Err(Error::FlakeNix(
                "invalid source path in Git inventory".into(),
            ));
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let destination = directory.path().join(&path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if metadata.is_file() {
            std::fs::copy(&path, &destination)?;
        } else if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&path)?, destination)?;
        } else {
            return Err(Error::FlakeNix(format!(
                "unsupported source entry {}",
                path.display()
            )));
        }
    }
    std::fs::copy(lock, directory.path().join("flake.lock"))?;
    Ok(directory)
}
