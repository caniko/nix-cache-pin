use crate::config::{PinConfig, VersionConstraint};
use crate::error::{Error, Result};
use crate::ext::{EvalAttrRequest, ExternalCommands};
use crate::flakeref::append_rev;
use crate::version;
use reqwest::Client;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PackageCheckResult {
    pub package: String,
    pub cached: bool,
    pub store_path: Option<String>,
    pub error: Option<String>,
    pub version: Option<String>,
    pub version_error: Option<String>,
    pub version_rejected_by: Vec<String>,
}

impl PackageCheckResult {
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.cached
            && self.error.is_none()
            && self.version_error.is_none()
            && self.version_rejected_by.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub rev: String,
    pub all_cached: bool,
    pub results: Vec<PackageCheckResult>,
}

#[derive(Clone)]
struct PackageEvalContext {
    flake_ref: String,
    input_name: String,
    arch: String,
    flake_output: String,
    full_attr_prefix: String,
    caches: Vec<String>,
    version_constraints: HashMap<String, VersionConstraint>,
    consumer_flake_ref: Option<String>,
    consumer_targets: std::collections::BTreeMap<String, String>,
    verify_closure: bool,
}

/// Build the qualified attribute string from an optional prefix and a package name.
#[must_use]
pub(crate) fn build_attr(attr_prefix: &str, pkg: &str) -> String {
    if attr_prefix.is_empty() {
        pkg.to_string()
    } else {
        format!("{attr_prefix}.{pkg}")
    }
}

/// Build the full nix eval reference string for a package at a given revision.
#[must_use]
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

/// Build the full nix eval reference string for an arbitrary package attr.
#[must_use]
pub(crate) fn build_eval_attr_ref(
    flake_ref: &str,
    rev: &str,
    arch: &str,
    flake_output: &str,
    attr_prefix: &str,
    pkg: &str,
    attr: &str,
) -> String {
    let package_attr = build_attr(attr_prefix, pkg);
    format!(
        "{}#{flake_output}.{arch}.{package_attr}.{attr}",
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

/// Build the output reference for a target in a consuming flake.
#[must_use]
pub(crate) fn build_consumer_eval_ref(consumer_flake_ref: &str, target: &str) -> String {
    format!(
        "{}#{}.outPath",
        consumer_flake_ref.trim_end_matches('#'),
        target
    )
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

/// Evaluate a target from the consuming flake with one input overridden to a
/// candidate revision. The lock file is never written during this probe.
pub async fn eval_consumer_store_path(
    consumer_flake_ref: &str,
    input_name: &str,
    source_flake_ref: &str,
    rev: &str,
    target: &str,
) -> Result<String> {
    let consumer_target = build_consumer_eval_ref(consumer_flake_ref, target);
    let candidate = append_rev(source_flake_ref, rev);
    let output = tokio::process::Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--no-write-lock-file",
            "--raw",
            "--override-input",
            input_name,
            &candidate,
            &consumer_target,
        ])
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(Error::NixEval {
            package: target.to_string(),
            stderr,
        })
    }
}

/// Evaluate an arbitrary package attribute via `nix eval`.
pub async fn eval_attr_value(
    flake_ref: &str,
    rev: &str,
    arch: &str,
    flake_output: &str,
    attr_prefix: &str,
    pkg: &str,
    attr: &str,
) -> Result<String> {
    let ref_str = build_eval_attr_ref(flake_ref, rev, arch, flake_output, attr_prefix, pkg, attr);

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
            package: format!("{pkg}.{attr}"),
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

async fn fetch_narinfo_references(
    client: &Client,
    hash: &str,
    caches: &[String],
) -> Option<Vec<String>> {
    for cache in caches {
        let url = format!("{cache}/{hash}.narinfo");
        let response = match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            _ => continue,
        };
        let body = match response.text().await {
            Ok(body) => body,
            Err(_) => continue,
        };
        let references = body
            .lines()
            .find_map(|line| line.strip_prefix("References:"))
            .unwrap_or_default()
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect();
        return Some(references);
    }
    None
}

/// Verify the complete closure referenced by a store path is present in the
/// configured binary caches. Narinfo references contain store hashes, so the
/// same cache endpoint can be queried recursively without realizing the path.
pub async fn check_narinfo_closure(client: &Client, store_path: &str, caches: &[String]) -> bool {
    let mut pending = vec![store_path_narinfo_hash(store_path).to_string()];
    let mut checked = std::collections::HashSet::new();

    while let Some(hash) = pending.pop() {
        if !checked.insert(hash.clone()) {
            continue;
        }
        let Some(references) = fetch_narinfo_references(client, &hash, caches).await else {
            return false;
        };
        pending.extend(references);
    }
    true
}

/// Verify that all packages at a given revision have narinfo cache hits.
pub async fn verify_narinfo_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    packages: &[String],
    ext: &Arc<E>,
) -> VerifyResult {
    let context = PackageEvalContext {
        flake_ref: cfg.flake_ref.clone(),
        input_name: cfg.input_name.clone(),
        arch: cfg.arch.clone(),
        flake_output: cfg.flake_output.clone(),
        full_attr_prefix: cfg.full_attr_prefix().to_string(),
        caches: cfg.caches.clone(),
        version_constraints: cfg.version_constraints.clone(),
        consumer_flake_ref: cfg.consumer_flake_ref.clone(),
        consumer_targets: cfg.consumer_targets.clone(),
        verify_closure: cfg.verify_closure,
    };
    let results = if cfg.fail_fast {
        let mut res = Vec::with_capacity(packages.len());
        for pkg in packages {
            let result = check_package_at_rev(client, &context, rev, pkg, ext).await;
            let accepted = result.accepted();
            res.push(result);
            if !accepted {
                break;
            }
        }
        res
    } else {
        let mut handles = Vec::with_capacity(packages.len());
        for pkg in packages {
            let client = client.clone();
            let ext = Arc::clone(ext);
            let rev = rev.to_string();
            let context = context.clone();
            let pkg = pkg.clone();
            let task_package = pkg.clone();

            handles.push((
                task_package,
                tokio::spawn(async move {
                    check_package_at_rev(&client, &context, &rev, &pkg, &ext).await
                }),
            ));
        }

        let mut results = Vec::with_capacity(packages.len());
        for (package, handle) in handles {
            match handle.await {
                Ok(r) => results.push(r),
                Err(e) => {
                    results.push(PackageCheckResult {
                        package,
                        cached: false,
                        store_path: None,
                        error: Some(format!("task join error: {e}")),
                        version: None,
                        version_error: None,
                        version_rejected_by: Vec::new(),
                    });
                }
            }
        }
        results
    };

    let all_cached = !results.is_empty() && results.iter().all(PackageCheckResult::accepted);
    VerifyResult {
        rev: rev.to_string(),
        all_cached,
        results,
    }
}

async fn check_package_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    context: &PackageEvalContext,
    rev: &str,
    pkg: &str,
    ext: &Arc<E>,
) -> PackageCheckResult {
    let store_path = if let (Some(consumer_flake_ref), Some(target)) = (
        context.consumer_flake_ref.as_deref(),
        context.consumer_targets.get(pkg),
    ) {
        ext.eval_consumer_store_path(
            consumer_flake_ref,
            &context.input_name,
            &context.flake_ref,
            rev,
            target,
        )
        .await
    } else {
        ext.eval_store_path(
            &context.flake_ref,
            rev,
            &context.arch,
            &context.flake_output,
            &context.full_attr_prefix,
            pkg,
        )
        .await
    };

    match store_path {
        Ok(store_path) => {
            let cached = if context.verify_closure {
                check_narinfo_closure(client, &store_path, &context.caches).await
            } else {
                check_narinfo(client, &store_path, &context.caches).await
            };
            let (version_value, version_error, version_rejected_by) =
                if context.consumer_targets.contains_key(pkg) {
                    // Consumer targets are already resolved through the complete
                    // consuming flake. Version gates currently apply only to
                    // direct source-flake package attributes.
                    (None, None, Vec::new())
                } else {
                    check_version_constraint(context, rev, pkg, ext).await
                };
            PackageCheckResult {
                package: pkg.to_string(),
                cached,
                store_path: Some(store_path),
                error: None,
                version: version_value,
                version_error,
                version_rejected_by,
            }
        }
        Err(e) => PackageCheckResult {
            package: pkg.to_string(),
            cached: false,
            store_path: None,
            error: Some(e.to_string()),
            version: None,
            version_error: None,
            version_rejected_by: Vec::new(),
        },
    }
}

async fn check_version_constraint<E: ExternalCommands + 'static>(
    context: &PackageEvalContext,
    rev: &str,
    pkg: &str,
    ext: &Arc<E>,
) -> (Option<String>, Option<String>, Vec<String>) {
    let Some(rule) = context.version_constraints.get(pkg) else {
        return (None, None, Vec::new());
    };

    match ext
        .eval_attr_value(EvalAttrRequest {
            flake_ref: &context.flake_ref,
            rev,
            arch: &context.arch,
            flake_output: &context.flake_output,
            attr_prefix: &context.full_attr_prefix,
            pkg,
            attr: &rule.version_attr,
        })
        .await
    {
        Ok(version_value) => match version::evaluate_version_rule(&version_value, rule) {
            Ok(decision) => (Some(decision.version), None, decision.rejected_by),
            Err(e) => (Some(version_value), Some(e.to_string()), Vec::new()),
        },
        Err(e) => (None, Some(e.to_string()), Vec::new()),
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
        let hash =
            store_path_narinfo_hash("/nix/store/0i1nnqfsc8c39sq0xbmqcfpfzigkbw0z-hello-2.12.1");
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
    fn test_build_consumer_eval_ref() {
        assert_eq!(
            build_consumer_eval_ref(".", "cachePinTargets.aarch64.rauthy"),
            ".#cachePinTargets.aarch64.rauthy.outPath"
        );
        assert_eq!(
            build_consumer_eval_ref("flake:#", "packages.aarch64-linux.rauthy"),
            "flake:#packages.aarch64-linux.rauthy.outPath"
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

    #[tokio::test]
    async fn test_check_narinfo_closure_follows_references() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/root.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("StorePath: /nix/store/root-app\nReferences: dep\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dep.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("StorePath: /nix/store/dep\nReferences:\n"),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        assert!(check_narinfo_closure(&client, "/nix/store/root-app", &[server.uri()]).await);
    }

    #[tokio::test]
    async fn test_check_narinfo_closure_rejects_missing_reference() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/root.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("StorePath: /nix/store/root-app\nReferences: missing\n"),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        assert!(!check_narinfo_closure(&client, "/nix/store/root-app", &[server.uri()]).await);
    }
}
