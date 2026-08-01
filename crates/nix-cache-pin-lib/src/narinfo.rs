use crate::config::{PinConfig, VersionConstraint};
use crate::error::{Error, Result};
use crate::ext::{EvalAttrRequest, ExternalCommands};
use crate::flakeref::append_rev;
use crate::version;
use reqwest::Client;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Cached,
    Missing,
    Unknown,
}

impl Availability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheCheck {
    pub availability: Availability,
    pub cache: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PackageCheckResult {
    pub package: String,
    pub target: Option<String>,
    pub cached: bool,
    pub availability: Availability,
    pub cache: Option<String>,
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

    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.error.clone().or_else(|| {
            self.version_error.clone().or_else(|| {
                (!self.version_rejected_by.is_empty()).then(|| {
                    format!(
                        "version {} rejected by {}",
                        self.version.as_deref().unwrap_or("unknown"),
                        self.version_rejected_by.join("; ")
                    )
                })
            })
        })
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
    required_consumer_targets: std::collections::BTreeMap<String, String>,
    verify_closure: bool,
    current_consumer: bool,
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

/// Evaluate a target from the consuming flake without changing its locked
/// inputs.
pub async fn eval_current_consumer_store_path(
    consumer_flake_ref: &str,
    target: &str,
) -> Result<String> {
    let consumer_target = build_consumer_eval_ref(consumer_flake_ref, target);
    let output = tokio::process::Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--no-write-lock-file",
            "--raw",
            &consumer_target,
        ])
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(Error::NixEval {
            package: target.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
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
    check_narinfo_availability(client, store_path, caches)
        .await
        .availability
        == Availability::Cached
}

/// Check a store path while preserving cache misses and lookup failures.
pub async fn check_narinfo_availability(
    client: &Client,
    store_path: &str,
    caches: &[String],
) -> CacheCheck {
    let hash = store_path_narinfo_hash(store_path);
    let mut errors = Vec::new();
    let mut error_cache = None;

    for cache in caches {
        let url = format!("{cache}/{hash}.narinfo");
        match client.head(&url).send().await {
            Ok(response) if response.status().is_success() => {
                return CacheCheck {
                    availability: Availability::Cached,
                    cache: Some(cache.clone()),
                    error: None,
                };
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok(response) => {
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache} returned HTTP {}", response.status()));
            }
            Err(error) => {
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache}: {error}"));
            }
        }
    }

    if errors.is_empty() && !caches.is_empty() {
        CacheCheck {
            availability: Availability::Missing,
            cache: None,
            error: None,
        }
    } else {
        CacheCheck {
            availability: Availability::Unknown,
            cache: error_cache,
            error: Some(if errors.is_empty() {
                "no binary caches configured".to_string()
            } else {
                errors.join("; ")
            }),
        }
    }
}

async fn fetch_narinfo_references(
    client: &Client,
    hash: &str,
    caches: &[String],
) -> (CacheCheck, Vec<String>) {
    let mut errors = Vec::new();
    let mut error_cache = None;
    for cache in caches {
        let url = format!("{cache}/{hash}.narinfo");
        let response = match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => continue,
            Ok(response) => {
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache} returned HTTP {}", response.status()));
                continue;
            }
            Err(error) => {
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache}: {error}"));
                continue;
            }
        };
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache}: {error}"));
                continue;
            }
        };
        let references = body
            .lines()
            .find_map(|line| line.strip_prefix("References:"))
            .unwrap_or_default()
            .split_whitespace()
            .map(|reference| store_path_narinfo_hash(reference).to_owned())
            .collect();
        return (
            CacheCheck {
                availability: Availability::Cached,
                cache: Some(cache.clone()),
                error: None,
            },
            references,
        );
    }
    if errors.is_empty() && !caches.is_empty() {
        (
            CacheCheck {
                availability: Availability::Missing,
                cache: None,
                error: None,
            },
            Vec::new(),
        )
    } else {
        (
            CacheCheck {
                availability: Availability::Unknown,
                cache: error_cache,
                error: Some(if errors.is_empty() {
                    "no binary caches configured".to_string()
                } else {
                    errors.join("; ")
                }),
            },
            Vec::new(),
        )
    }
}

/// Verify the complete closure referenced by a store path is present in the
/// configured binary caches. Narinfo references contain store hashes, so the
/// same cache endpoint can be queried recursively without realizing the path.
pub async fn check_narinfo_closure(client: &Client, store_path: &str, caches: &[String]) -> bool {
    check_narinfo_closure_availability(client, store_path, caches)
        .await
        .availability
        == Availability::Cached
}

pub async fn check_narinfo_closure_availability(
    client: &Client,
    store_path: &str,
    caches: &[String],
) -> CacheCheck {
    let mut pending = vec![store_path_narinfo_hash(store_path).to_string()];
    let mut checked = std::collections::HashSet::new();
    let mut root_cache = None;

    while let Some(hash) = pending.pop() {
        if !checked.insert(hash.clone()) {
            continue;
        }
        let (check, references) = fetch_narinfo_references(client, &hash, caches).await;
        if check.availability != Availability::Cached {
            return check;
        }
        root_cache = root_cache.or(check.cache);
        pending.extend(references);
    }
    CacheCheck {
        availability: Availability::Cached,
        cache: root_cache,
        error: None,
    }
}

pub async fn verify_required_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    ext: &Arc<E>,
) -> VerifyResult {
    verify_labels_at_rev(client, cfg, rev, &cfg.required_labels(), ext, false).await
}

pub async fn verify_current<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    ext: &Arc<E>,
) -> VerifyResult {
    verify_labels_at_rev(client, cfg, rev, &cfg.required_labels(), ext, true).await
}

/// Verify that all packages at a given revision have narinfo cache hits.
pub async fn verify_narinfo_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    packages: &[String],
    ext: &Arc<E>,
) -> VerifyResult {
    verify_labels_at_rev(client, cfg, rev, packages, ext, false).await
}

async fn verify_labels_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    packages: &[String],
    ext: &Arc<E>,
    current_consumer: bool,
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
        required_consumer_targets: cfg.required_consumer_targets.clone(),
        verify_closure: cfg.verify_closure,
        current_consumer,
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
                        target: None,
                        cached: false,
                        availability: Availability::Unknown,
                        cache: None,
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
    let target = context
        .consumer_targets
        .get(pkg)
        .or_else(|| context.required_consumer_targets.get(pkg));
    let store_path = if let (Some(consumer_flake_ref), Some(target)) =
        (context.consumer_flake_ref.as_deref(), target)
    {
        if context.current_consumer {
            ext.eval_current_consumer_store_path(consumer_flake_ref, target)
                .await
        } else {
            ext.eval_consumer_store_path(
                consumer_flake_ref,
                &context.input_name,
                &context.flake_ref,
                rev,
                target,
            )
            .await
        }
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
            let cache_check = if context.verify_closure {
                check_narinfo_closure_availability(client, &store_path, &context.caches).await
            } else {
                check_narinfo_availability(client, &store_path, &context.caches).await
            };
            let (version_value, version_error, version_rejected_by) = if target.is_some() {
                // Consumer targets are already resolved through the complete
                // consuming flake. Version gates currently apply only to
                // direct source-flake package attributes.
                (None, None, Vec::new())
            } else {
                check_version_constraint(context, rev, pkg, ext).await
            };
            PackageCheckResult {
                package: pkg.to_string(),
                target: target.cloned(),
                cached: cache_check.availability == Availability::Cached,
                availability: cache_check.availability,
                cache: cache_check.cache,
                store_path: Some(store_path),
                error: cache_check.error,
                version: version_value,
                version_error,
                version_rejected_by,
            }
        }
        Err(e) => PackageCheckResult {
            package: pkg.to_string(),
            target: target.cloned(),
            cached: false,
            availability: Availability::Unknown,
            cache: None,
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
        let result =
            check_narinfo_availability(&client, "/nix/store/abc123-hello-2.12", &caches).await;
        assert_eq!(result.availability, Availability::Missing);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_check_narinfo_server_error_is_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = check_narinfo_availability(
            &Client::new(),
            "/nix/store/abc123-hello-2.12",
            &[server.uri()],
        )
        .await;
        assert_eq!(result.availability, Availability::Unknown);
        assert!(result.error.unwrap().contains("HTTP 500"));
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
                    .set_body_string(
                        "StorePath: /nix/store/root-app\nReferences: dep123456789012345678901234567890-dependency\n",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dep123456789012345678901234567890.narinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "StorePath: /nix/store/dep123456789012345678901234567890-dependency\nReferences:\n",
            ))
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
        let result =
            check_narinfo_closure_availability(&client, "/nix/store/root-app", &[server.uri()])
                .await;
        assert_eq!(result.availability, Availability::Missing);
    }

    #[tokio::test]
    async fn test_check_narinfo_closure_server_error_is_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = check_narinfo_closure_availability(
            &Client::new(),
            "/nix/store/root-app",
            &[server.uri()],
        )
        .await;
        assert_eq!(result.availability, Availability::Unknown);
        assert!(result.error.unwrap().contains("HTTP 500"));
    }
}
