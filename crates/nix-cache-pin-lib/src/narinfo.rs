use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::ext::ExternalCommands;
use crate::flakeref::append_rev;
use reqwest::Client;
use std::path::Path;
use std::sync::Arc;

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

/// Build the qualified attribute string from an optional prefix and a package name.
pub(crate) fn build_attr(attr_prefix: &str, pkg: &str) -> String {
    if attr_prefix.is_empty() {
        pkg.to_string()
    } else {
        format!("{attr_prefix}.{pkg}")
    }
}

/// Build the full nix eval reference string for a package at a given revision.
pub(crate) fn build_eval_ref(
    flake_ref: &str,
    rev: &str,
    arch: &str,
    flake_output: &str,
    attr_prefix: &str,
    pkg: &str,
) -> String {
    let attr = build_attr(attr_prefix, pkg);
    format!(
        "{}#{flake_output}.{arch}.{attr}.outPath",
        append_rev(flake_ref, rev)
    )
}

/// Extract the narinfo hash from a Nix store path.
///
/// Given `/nix/store/aaaa...-hello-2.12`, returns `"aaaa..."`.
pub(crate) fn store_path_narinfo_hash(store_path: &str) -> &str {
    let basename = Path::new(store_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    basename.split('-').next().unwrap_or("")
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
    let ref_str = build_eval_ref(flake_ref, rev, arch, flake_output, attr_prefix, pkg);

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
    let hash = store_path_narinfo_hash(store_path);

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
pub async fn verify_narinfo_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    packages: &[String],
    ext: &Arc<E>,
) -> VerifyResult {
    let full_attr_prefix = cfg.full_attr_prefix().to_string();
    let results = if cfg.fail_fast {
        let mut res = Vec::new();
        for pkg in packages {
            match ext
                .eval_store_path(
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
            let ext = Arc::clone(ext);
            let flake_ref = cfg.flake_ref.clone();
            let rev = rev.to_string();
            let arch = cfg.arch.clone();
            let flake_output = cfg.flake_output.clone();
            let full_attr_prefix = full_attr_prefix.clone();
            let caches = cfg.caches.clone();
            let pkg = pkg.clone();

            handles.push(tokio::spawn(async move {
                match ext
                    .eval_store_path(
                        &flake_ref, &rev, &arch, &flake_output, &full_attr_prefix, &pkg,
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- store_path_narinfo_hash ---

    #[test]
    fn test_narinfo_hash_standard_path() {
        let hash = store_path_narinfo_hash("/nix/store/aaaa1234-hello-2.12");
        assert_eq!(hash, "aaaa1234");
    }

    #[test]
    fn test_narinfo_hash_long_hash() {
        let hash = store_path_narinfo_hash(
            "/nix/store/0i1nnqfsc8c39sq0xbmqcfpfzigkbw0z-hello-2.12.1",
        );
        assert_eq!(hash, "0i1nnqfsc8c39sq0xbmqcfpfzigkbw0z");
    }

    #[test]
    fn test_narinfo_hash_no_dash() {
        // A basename with no dash returns the whole basename
        let hash = store_path_narinfo_hash("/nix/store/nodash");
        assert_eq!(hash, "nodash");
    }

    #[test]
    fn test_narinfo_hash_empty() {
        let hash = store_path_narinfo_hash("");
        assert_eq!(hash, "");
    }

    #[test]
    fn test_narinfo_hash_bare_filename() {
        let hash = store_path_narinfo_hash("abc123-pkg");
        assert_eq!(hash, "abc123");
    }

    // --- build_attr ---

    #[test]
    fn test_build_attr_empty_prefix() {
        assert_eq!(build_attr("", "hello"), "hello");
    }

    #[test]
    fn test_build_attr_with_prefix() {
        assert_eq!(build_attr("pkgsRocm", "rocblas"), "pkgsRocm.rocblas");
    }

    #[test]
    fn test_build_attr_python_packages() {
        assert_eq!(
            build_attr("python313Packages", "torch"),
            "python313Packages.torch"
        );
    }

    // --- build_eval_ref ---

    #[test]
    fn test_build_eval_ref_no_prefix() {
        let r = build_eval_ref(
            "github:NixOS/nixpkgs",
            "abc123",
            "x86_64-linux",
            "legacyPackages",
            "",
            "hello",
        );
        assert_eq!(
            r,
            "github:NixOS/nixpkgs/abc123#legacyPackages.x86_64-linux.hello.outPath"
        );
    }

    #[test]
    fn test_build_eval_ref_with_prefix() {
        let r = build_eval_ref(
            "github:NixOS/nixpkgs",
            "abc123",
            "x86_64-linux",
            "legacyPackages",
            "python313Packages",
            "torch",
        );
        assert_eq!(
            r,
            "github:NixOS/nixpkgs/abc123#legacyPackages.x86_64-linux.python313Packages.torch.outPath"
        );
    }

    #[test]
    fn test_build_eval_ref_git_https() {
        let r = build_eval_ref(
            "git+https://gitlab.com/foo/bar",
            "def456",
            "aarch64-linux",
            "packages",
            "",
            "mypkg",
        );
        assert_eq!(
            r,
            "git+https://gitlab.com/foo/bar?rev=def456#packages.aarch64-linux.mypkg.outPath"
        );
    }

    // --- check_narinfo (HTTP integration tests with wiremock) ---

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_check_narinfo_cache_hit() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = Client::new();
        let caches = vec![server.uri()];
        assert!(check_narinfo(&client, "/nix/store/abc123-hello-2.12", &caches).await);
    }

    #[tokio::test]
    async fn test_check_narinfo_cache_miss() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = Client::new();
        let caches = vec![server.uri()];
        assert!(!check_narinfo(&client, "/nix/store/abc123-hello-2.12", &caches).await);
    }

    #[tokio::test]
    async fn test_check_narinfo_multiple_caches_second_hit() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server1)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server2)
            .await;

        let client = Client::new();
        let caches = vec![server1.uri(), server2.uri()];
        assert!(check_narinfo(&client, "/nix/store/abc123-hello-2.12", &caches).await);
    }

    #[tokio::test]
    async fn test_check_narinfo_all_caches_miss() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server1)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server2)
            .await;

        let client = Client::new();
        let caches = vec![server1.uri(), server2.uri()];
        assert!(!check_narinfo(&client, "/nix/store/abc123-hello-2.12", &caches).await);
    }

    #[tokio::test]
    async fn test_check_narinfo_empty_caches() {
        let client = Client::new();
        assert!(!check_narinfo(&client, "/nix/store/abc123-hello", &[]).await);
    }
}
