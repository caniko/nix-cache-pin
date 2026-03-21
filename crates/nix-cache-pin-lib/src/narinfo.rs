use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::flakeref::append_rev;
use reqwest::Client;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PackageCheckResult {
    pub package: String,
    pub cached: bool,
    pub store_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub rev: String,
    pub all_cached: bool,
    pub results: Vec<PackageCheckResult>,
}

/// Evaluate the nix store path for a flake ref + attribute via `nix eval`.
pub async fn eval_store_path(
    flake_ref: &str,
    rev: &str,
    arch: &str,
    flake_output: &str,
    attr_prefix: &str,
    pkg: &str,
) -> Result<String> {
    let attr = if attr_prefix.is_empty() {
        pkg.to_string()
    } else {
        format!("{attr_prefix}.{pkg}")
    };
    let ref_str = format!(
        "{}#{flake_output}.{arch}.{attr}.outPath",
        append_rev(flake_ref, rev)
    );

    let output = tokio::process::Command::new("nix")
        .args(["eval", "--impure", "--raw", &ref_str])
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(Error::NixEval {
            package: pkg.to_string(),
            stderr,
        })
    }
}

/// Check if a store path has a `.narinfo` entry in any of the given caches.
pub async fn check_narinfo(client: &Client, store_path: &str, caches: &[String]) -> bool {
    let basename = Path::new(store_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let hash = basename.split('-').next().unwrap_or("");

    for cache in caches {
        let url = format!("{cache}/{hash}.narinfo");
        match client.head(&url).send().await {
            Ok(resp) if resp.status().is_success() => return true,
            _ => continue,
        }
    }
    false
}

/// Verify that all packages at a given revision have narinfo cache hits.
pub async fn verify_narinfo_at_rev(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    packages: &[String],
) -> VerifyResult {
    let full_attr_prefix = cfg.full_attr_prefix().to_string();
    let results = if cfg.fail_fast {
        let mut res = Vec::new();
        for pkg in packages {
            match eval_store_path(
                &cfg.flake_ref,
                rev,
                &cfg.arch,
                &cfg.flake_output,
                &full_attr_prefix,
                pkg,
            )
            .await
            {
                Ok(store_path) => {
                    let cached = check_narinfo(client, &store_path, &cfg.caches).await;
                    res.push(PackageCheckResult {
                        package: pkg.clone(),
                        cached,
                        store_path: Some(store_path),
                        error: None,
                    });
                    if !cached {
                        break;
                    }
                }
                Err(e) => {
                    res.push(PackageCheckResult {
                        package: pkg.clone(),
                        cached: false,
                        store_path: None,
                        error: Some(e.to_string()),
                    });
                    break;
                }
            }
        }
        res
    } else {
        let mut handles = Vec::new();
        for pkg in packages {
            let client = client.clone();
            let flake_ref = cfg.flake_ref.clone();
            let rev = rev.to_string();
            let arch = cfg.arch.clone();
            let flake_output = cfg.flake_output.clone();
            let full_attr_prefix = full_attr_prefix.clone();
            let caches = cfg.caches.clone();
            let pkg = pkg.clone();

            handles.push(tokio::spawn(async move {
                match eval_store_path(
                    &flake_ref,
                    &rev,
                    &arch,
                    &flake_output,
                    &full_attr_prefix,
                    &pkg,
                )
                .await
                {
                    Ok(store_path) => {
                        let cached = check_narinfo(&client, &store_path, &caches).await;
                        PackageCheckResult {
                            package: pkg,
                            cached,
                            store_path: Some(store_path),
                            error: None,
                        }
                    }
                    Err(e) => PackageCheckResult {
                        package: pkg,
                        cached: false,
                        store_path: None,
                        error: Some(e.to_string()),
                    },
                }
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(r) => results.push(r),
                Err(e) => {
                    results.push(PackageCheckResult {
                        package: "unknown".into(),
                        cached: false,
                        store_path: None,
                        error: Some(format!("task join error: {e}")),
                    });
                }
            }
        }
        results
    };

    let all_cached = !results.is_empty() && results.iter().all(|r| r.cached);
    VerifyResult {
        rev: rev.to_string(),
        all_cached,
        results,
    }
}
