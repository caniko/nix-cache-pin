use crate::config::{PinConfig, VersionConstraint};
use crate::error::{Error, Result};
use crate::ext::{
    ConsumerOverride, EvalAttrRequest, EvalConsumerAttrRequest, ExternalCommands, StorePathRequest,
};
use crate::flakeref::append_rev;
use crate::version;
use reqwest::Client;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

mod metadata;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
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
    pub attempts: Vec<CacheAttempt>,
}

/// An actual HTTP lookup, including closure references and unsuccessful caches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CacheAttempt {
    pub url: String,
    pub method: String,
    pub status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
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
    pub cache_attempts: Vec<CacheAttempt>,
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyResult {
    pub rev: String,
    pub all_cached: bool,
    pub results: Vec<PackageCheckResult>,
    /// Final-graph negative requirements: cache hits require wish promotion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wish_results: Vec<PackageCheckResult>,
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
    locked_input_targets: std::collections::BTreeSet<String>,
    required_consumer_targets: std::collections::BTreeMap<String, String>,
    verify_closure: bool,
    current_consumer: bool,
}

impl PackageEvalContext {
    fn target(&self, pkg: &str) -> Option<&String> {
        self.consumer_targets
            .get(pkg)
            .or_else(|| self.required_consumer_targets.get(pkg))
    }
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
        .kill_on_drop(true)
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
        .kill_on_drop(true)
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
        .kill_on_drop(true)
        .args([
            "eval",
            "--impure",
            "--no-write-lock-file",
            "--no-update-lock-file",
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

/// Evaluate several store paths. Requests that share a flake and flags share
/// one `nix eval`, so the flake and whatever their attributes have in common
/// (a NixOS configuration, a package set) are evaluated once. Each path
/// resolves as its per-item evaluation would. A batch that fails is retried
/// request by request, so each keeps its own result and error. Evaluations run
/// one at a time, so an embedder can admit the whole call once.
pub async fn eval_store_paths(requests: &[StorePathRequest<'_>]) -> Vec<Result<String>> {
    let mut results: Vec<Option<Result<String>>> = requests.iter().map(|_| None).collect();
    let mut batches: std::collections::BTreeMap<Batch, Vec<(usize, Vec<String>)>> =
        std::collections::BTreeMap::new();
    for (index, request) in requests.iter().enumerate() {
        if let Some((batch, path)) = batch_of(request) {
            batches.entry(batch).or_default().push((index, path));
        }
    }
    for (batch, members) in batches {
        if members.len() < 2 {
            continue;
        }
        let paths: Vec<&[String]> = members.iter().map(|(_, path)| path.as_slice()).collect();
        if let Ok(store_paths) = eval_batch(&batch, &paths).await {
            for ((index, _), store_path) in members.into_iter().zip(store_paths) {
                results[index] = Some(Ok(store_path));
            }
        }
    }
    // One at a time, so an embedder that admits the whole call once never runs
    // more than one evaluation under that admission.
    let mut single = Vec::with_capacity(requests.len());
    for (result, request) in results.iter().zip(requests) {
        single.push(match result {
            Some(_) => None,
            None => Some(eval_single(request).await),
        });
    }
    results
        .into_iter()
        .zip(single)
        .map(|(batched, single)| {
            batched
                .or(single)
                .expect("every request is evaluated in a batch or on its own")
        })
        .collect()
}

async fn eval_single(request: &StorePathRequest<'_>) -> Result<String> {
    match *request {
        StorePathRequest::Producer {
            flake_ref,
            rev,
            arch,
            flake_output,
            attr_prefix,
            pkg,
        } => eval_store_path(flake_ref, rev, arch, flake_output, attr_prefix, pkg).await,
        StorePathRequest::Consumer {
            consumer_flake_ref,
            candidate: Some(c),
            target,
        } => {
            eval_consumer_store_path(
                consumer_flake_ref,
                c.input_name,
                c.source_flake_ref,
                c.rev,
                target,
            )
            .await
        }
        StorePathRequest::Consumer {
            consumer_flake_ref,
            candidate: None,
            target,
        } => eval_current_consumer_store_path(consumer_flake_ref, target).await,
        StorePathRequest::LockedInput {
            consumer_flake_ref,
            target,
        } => eval_input_graph_attr(consumer_flake_ref, target, "outPath").await,
    }
}

/// Requests that one `nix eval` can answer together.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Batch {
    /// Flags of the requests' per-item evaluations, minus the output format.
    args: Vec<String>,
    /// The flake the paths select from.
    flake: String,
    /// Installable paths try the `packages.<system>` prefixes first, as
    /// `nix eval <flake>#<path>` does; `builtins.getFlake` paths do not.
    installable: bool,
}

/// The batch a request joins and its full attribute path, mirroring its
/// per-item evaluation. Paths Nix would parse differently stay single.
fn batch_of(request: &StorePathRequest<'_>) -> Option<(Batch, Vec<String>)> {
    let strings = |args: &[&str]| args.iter().map(ToString::to_string).collect::<Vec<_>>();
    let (batch, mut path) = match *request {
        StorePathRequest::Producer {
            flake_ref,
            rev,
            arch,
            flake_output,
            attr_prefix,
            pkg,
        } => (
            Batch {
                args: strings(&["--impure"]),
                flake: append_rev(flake_ref, rev),
                installable: true,
            },
            attr_path_segments(&format!(
                "{flake_output}.{arch}.{}",
                build_attr(attr_prefix, pkg)
            ))?,
        ),
        StorePathRequest::Consumer {
            consumer_flake_ref,
            candidate,
            target,
        } => {
            let mut args = strings(&["--impure", "--no-write-lock-file"]);
            match candidate {
                Some(c) => args.extend(strings(&[
                    "--override-input",
                    c.input_name,
                    &append_rev(c.source_flake_ref, c.rev),
                ])),
                None => args.push("--no-update-lock-file".into()),
            }
            (
                Batch {
                    args,
                    flake: consumer_flake_ref.trim_end_matches('#').to_string(),
                    installable: true,
                },
                attr_path_segments(target)?,
            )
        }
        StorePathRequest::LockedInput {
            consumer_flake_ref,
            target,
        } => (
            Batch {
                args: strings(&["--impure", "--no-write-lock-file", "--no-update-lock-file"]),
                flake: consumer_flake_ref.to_string(),
                installable: false,
            },
            target.split('.').map(ToString::to_string).collect(),
        ),
    };
    path.push("outPath".into());
    Some((batch, path))
}

async fn eval_batch(batch: &Batch, paths: &[&[String]]) -> Result<Vec<String>> {
    let mut command = tokio::process::Command::new("nix");
    command
        .kill_on_drop(true)
        .arg("eval")
        .args(&batch.args)
        .arg("--json");
    let label = if batch.installable {
        // `#.` is the flake's outputs root.
        let installable = format!("{}#.", batch.flake);
        command.args(["--apply", &installable_batch_expr(paths), &installable]);
        installable
    } else {
        command.args(["--expr", &get_flake_batch_expr(&batch.flake, paths)]);
        batch.flake.clone()
    };
    let output = command.env("NIXPKGS_ALLOW_UNFREE", "1").output().await?;
    let failed = |stderr: String| Error::NixEval {
        package: format!("{} attributes of {label}", paths.len()),
        stderr,
    };
    if !output.status.success() {
        return Err(failed(String::from_utf8_lossy(&output.stderr).to_string()));
    }
    let store_paths: Vec<String> = serde_json::from_slice(&output.stdout)
        .map_err(|e| failed(format!("unexpected batch output: {e}")))?;
    if store_paths.len() != paths.len() {
        return Err(failed(format!(
            "batch returned {} store paths for {} attributes",
            store_paths.len(),
            paths.len()
        )));
    }
    Ok(store_paths)
}

/// Split an installable attribute path the way Nix's `parseAttrPath` does:
/// `.` separates segments and `"…"` quotes a run that may contain dots.
/// Paths with an empty segment or an unclosed quote are left to Nix, and so is
/// any character the installable's URL parsing could change (`%61` selects
/// `a`) or reject (`${`).
fn attr_path_segments(target: &str) -> Option<Vec<String>> {
    if !target
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-'\"+".contains(c))
    {
        return None;
    }
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = target.chars();
    while let Some(c) = chars.next() {
        match c {
            '.' => segments.push(std::mem::take(&mut current)),
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    c => current.push(c),
                }
            },
            c => current.push(c),
        }
    }
    segments.push(current);
    (!segments.iter().any(String::is_empty)).then_some(segments)
}

/// A Nix string literal, so no attribute name can inject code.
fn nix_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn nix_path_lists(paths: &[&[String]]) -> String {
    let paths: Vec<String> = paths
        .iter()
        .map(|path| {
            let segments: Vec<String> = path.iter().map(|s| nix_string(s)).collect();
            format!("[ {} ]", segments.join(" "))
        })
        .collect();
    format!("[ {} ]", paths.join(" "))
}

/// Each path resolves like an installable: under `packages.<system>`, then
/// `legacyPackages.<system>`, then from the root, taking the first that has
/// it. Like Nix, every candidate is looked up, so one that throws fails the
/// batch even when an earlier one matched.
fn installable_batch_expr(paths: &[&[String]]) -> String {
    format!(
        r#"root: let
  system = builtins.currentSystem;
  has = set: path: path == [ ] || (builtins.isAttrs set && set ? ${{builtins.head path}} && has set.${{builtins.head path}} (builtins.tail path));
  get = builtins.foldl' (set: name: set.${{name}});
  resolve = path: let
    packages = [ "packages" system ] ++ path;
    legacy = [ "legacyPackages" system ] ++ path;
    found = map (has root) [ packages legacy path ];
  in builtins.deepSeq found (get root (if builtins.elemAt found 0 then packages else if builtins.elemAt found 1 then legacy else path));
in map resolve {}"#,
        nix_path_lists(paths)
    )
}

fn get_flake_batch_expr(flake: &str, paths: &[&[String]]) -> String {
    format!(
        "map (builtins.foldl' (set: name: set.${{name}}) (builtins.getFlake {})) {}",
        nix_string(flake),
        nix_path_lists(paths)
    )
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
        .kill_on_drop(true)
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

/// Evaluate the actual consumer package version with the same override as
/// the candidate store-path lookup. Current checks use the locked input.
pub async fn eval_consumer_attr_value(request: EvalConsumerAttrRequest<'_>) -> Result<String> {
    let consumer_target = format!(
        "{}#{}.{}",
        request.consumer_flake_ref.trim_end_matches('#'),
        request.target,
        request.attr
    );
    let mut command = tokio::process::Command::new("nix");
    command.kill_on_drop(true);
    command.args(["eval", "--impure", "--no-write-lock-file", "--raw"]);
    if !request.current {
        let candidate = append_rev(request.source_flake_ref, request.rev);
        command.args(["--override-input", request.input_name, &candidate]);
    } else {
        command.arg("--no-update-lock-file");
    }
    let output = command
        .arg(&consumer_target)
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .output()
        .await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(Error::NixEval {
            package: format!("{}.{}", request.target, request.attr),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

pub(crate) async fn eval_input_graph_attr(
    consumer: &str,
    target: &str,
    attr: &str,
) -> Result<String> {
    let selection = target
        .split('.')
        .chain(std::iter::once(attr))
        .map(|s| serde_json::to_string(s).expect("string serialization"))
        .collect::<Vec<_>>()
        .join(".");
    let expr = format!(
        "(builtins.getFlake {}).{}",
        serde_json::to_string(consumer)?,
        selection
    );
    let output = tokio::process::Command::new("nix")
        .kill_on_drop(true)
        .args([
            "eval",
            "--impure",
            "--no-write-lock-file",
            "--no-update-lock-file",
            "--raw",
            "--expr",
            &expr,
        ])
        .env("NIXPKGS_ALLOW_UNFREE", "1")
        .output()
        .await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    } else {
        Err(Error::NixEval {
            package: target.into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
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
pub(crate) fn serialize_cache_urls<S: serde::Serializer>(
    caches: &[String],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::Serialize;
    caches
        .iter()
        .map(|cache| diagnostic_url(cache))
        .collect::<Vec<_>>()
        .serialize(serializer)
}

fn diagnostic_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.into()
        }
        Err(_) => "<invalid cache URL>".into(),
    }
}

fn narinfo_url(cache: &str, hash: &str) -> String {
    match reqwest::Url::parse(cache) {
        Ok(mut url) => {
            url.set_path(&format!(
                "{}/{hash}.narinfo",
                url.path().trim_end_matches('/')
            ));
            url.into()
        }
        Err(_) => format!("{cache}/{hash}.narinfo"),
    }
}

pub async fn check_narinfo_availability(
    client: &Client,
    store_path: &str,
    caches: &[String],
) -> CacheCheck {
    // Availability requires usable metadata even when closure traversal is off.
    fetch_narinfo_references(client, store_path, caches).await.0
}

const MAX_NARINFO_BYTES: usize = 1024 * 1024;

async fn read_narinfo_body(mut response: reqwest::Response) -> std::result::Result<String, String> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_NARINFO_BYTES as u64)
    {
        return Err("narinfo exceeds 1 MiB response limit".into());
    }
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url().to_string())?
        {
            if chunk.len() > MAX_NARINFO_BYTES - body.len() {
                return Err("narinfo exceeds 1 MiB response limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).map_err(|_| "narinfo is not UTF-8".to_string())
    })
    .await
    .map_err(|_| "narinfo response body timed out after 30 seconds".to_string())?
}

async fn fetch_narinfo_references(
    client: &Client,
    store_path: &str,
    caches: &[String],
) -> (CacheCheck, Vec<String>) {
    let hash = store_path_narinfo_hash(store_path);
    let mut errors = Vec::new();
    let mut error_cache = None;
    let mut attempts = Vec::new();
    for cache in caches {
        // Deserializing PinConfig directly skips `validate`, so every lookup
        // re-checks here before a request can reach the banned host.
        if crate::config::is_compromised_cache(cache) {
            let cache = diagnostic_url(cache);
            error_cache.get_or_insert_with(|| cache.clone());
            errors.push(format!("{cache}: refusing a compromised cache"));
            continue;
        }
        let url = narinfo_url(cache, hash);
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(reqwest::Error::without_url);
        let cache = diagnostic_url(cache);
        attempts.push(CacheAttempt {
            url: diagnostic_url(&url),
            method: "GET".into(),
            status: response.as_ref().ok().map(|r| r.status().as_u16()),
            error: response.as_ref().err().map(ToString::to_string),
        });
        let response = match response {
            // Nix 2.34 filetransfer.cc accepts 200 and 201 for full content;
            // 203 is not in its successfulStatuses. No partial/empty responses.
            Ok(response) if matches!(response.status().as_u16(), 200 | 201) => response,
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
        let body = match read_narinfo_body(response).await {
            Ok(body) => body,
            Err(error) => {
                if let Some(attempt) = attempts.last_mut() {
                    attempt.error = Some(error.to_string());
                }
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache}: {error}"));
                continue;
            }
        };
        let references = match metadata::references(&body, store_path) {
            Ok(references) => references,
            Err(reason) => {
                let error = format!("invalid narinfo: {reason}");
                if let Some(attempt) = attempts.last_mut() {
                    attempt.error = Some(error.clone());
                }
                error_cache.get_or_insert_with(|| cache.clone());
                errors.push(format!("{cache}: {error}"));
                continue;
            }
        };
        return (
            CacheCheck {
                availability: Availability::Cached,
                cache: Some(cache.clone()),
                error: None,
                attempts,
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
                attempts,
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
                attempts,
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
    let mut pending = vec![store_path.to_string()];
    let mut checked = std::collections::HashSet::new();
    let mut root_cache = None;
    let mut attempts = Vec::new();

    while let Some(path) = pending.pop() {
        if !checked.insert(path.clone()) {
            continue;
        }
        let (mut check, references) = fetch_narinfo_references(client, &path, caches).await;
        attempts.append(&mut check.attempts);
        if check.availability != Availability::Cached {
            check.attempts = attempts;
            return check;
        }
        root_cache = root_cache.or(check.cache);
        pending.extend(references);
    }
    CacheCheck {
        availability: Availability::Cached,
        cache: root_cache,
        error: None,
        attempts,
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

pub(crate) async fn verify_current_wishes<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    rev: &str,
    ext: &Arc<E>,
) -> Vec<PackageCheckResult> {
    verify_labels_at_rev(client, cfg, rev, &cfg.wish_packages, ext, true)
        .await
        .results
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
        locked_input_targets: cfg.locked_input_targets.clone(),
        required_consumer_targets: cfg.required_consumer_targets.clone(),
        verify_closure: cfg.verify_closure,
        current_consumer,
    };
    let requests: Vec<StorePathRequest<'_>> = packages
        .iter()
        .map(|pkg| store_path_request(&context, rev, pkg))
        .collect();
    let results = if cfg.fail_fast {
        // One at a time, so the first rejection stops before later evaluations.
        let mut res = Vec::with_capacity(packages.len());
        for (pkg, request) in packages.iter().zip(&requests) {
            let store_path = ext
                .eval_store_paths(std::slice::from_ref(request))
                .await
                .pop()
                .expect("one result per request");
            let result = check_package_at_rev(client, &context, rev, pkg, store_path, ext).await;
            let accepted = result.accepted();
            res.push(result);
            if !accepted {
                break;
            }
        }
        res
    } else {
        let store_paths = ext.eval_store_paths(&requests).await;
        let mut handles = Vec::with_capacity(packages.len());
        for (pkg, store_path) in packages.iter().zip(store_paths) {
            let client = client.clone();
            let ext = Arc::clone(ext);
            let rev = rev.to_string();
            let context = context.clone();
            let pkg = pkg.clone();
            let task_package = pkg.clone();

            handles.push((
                task_package,
                tokio::spawn(async move {
                    check_package_at_rev(&client, &context, &rev, &pkg, store_path, &ext).await
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
                        cache_attempts: Vec::new(),
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
        wish_results: Vec::new(),
    }
}

/// How a package's store path is evaluated: through its consumer target
/// (from the locked input graph, at the current lock, or with the pinned
/// input overridden), else from the producer flake.
fn store_path_request<'a>(
    context: &'a PackageEvalContext,
    rev: &'a str,
    pkg: &'a str,
) -> StorePathRequest<'a> {
    match (context.consumer_flake_ref.as_deref(), context.target(pkg)) {
        (Some(consumer_flake_ref), Some(target)) if context.locked_input_targets.contains(pkg) => {
            StorePathRequest::LockedInput {
                consumer_flake_ref,
                target,
            }
        }
        (Some(consumer_flake_ref), Some(target)) => StorePathRequest::Consumer {
            consumer_flake_ref,
            candidate: (!context.current_consumer).then_some(ConsumerOverride {
                input_name: &context.input_name,
                source_flake_ref: &context.flake_ref,
                rev,
            }),
            target,
        },
        _ => StorePathRequest::Producer {
            flake_ref: &context.flake_ref,
            rev,
            arch: &context.arch,
            flake_output: &context.flake_output,
            attr_prefix: &context.full_attr_prefix,
            pkg,
        },
    }
}

async fn check_package_at_rev<E: ExternalCommands + 'static>(
    client: &Client,
    context: &PackageEvalContext,
    rev: &str,
    pkg: &str,
    store_path: Result<String>,
    ext: &Arc<E>,
) -> PackageCheckResult {
    let target = context.target(pkg);
    match store_path {
        Ok(store_path) => {
            let cache_check = if context.verify_closure {
                check_narinfo_closure_availability(client, &store_path, &context.caches).await
            } else {
                check_narinfo_availability(client, &store_path, &context.caches).await
            };
            let (version_value, version_error, version_rejected_by) =
                check_version_constraint(context, rev, pkg, target.map(String::as_str), ext).await;
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
                cache_attempts: cache_check.attempts,
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
            cache_attempts: Vec::new(),
        },
    }
}

async fn check_version_constraint<E: ExternalCommands + 'static>(
    context: &PackageEvalContext,
    rev: &str,
    pkg: &str,
    consumer_target: Option<&str>,
    ext: &Arc<E>,
) -> (Option<String>, Option<String>, Vec<String>) {
    let Some(rule) = context.version_constraints.get(pkg) else {
        return (None, None, Vec::new());
    };

    let value = if let Some(target) = consumer_target {
        match context.consumer_flake_ref.as_deref() {
            Some(consumer_flake_ref) => {
                if context.locked_input_targets.contains(pkg) {
                    ext.eval_locked_input_attr_value(consumer_flake_ref, target, &rule.version_attr)
                        .await
                } else {
                    ext.eval_consumer_attr_value(EvalConsumerAttrRequest {
                        consumer_flake_ref,
                        input_name: &context.input_name,
                        source_flake_ref: &context.flake_ref,
                        rev,
                        target,
                        attr: &rule.version_attr,
                        current: context.current_consumer,
                    })
                    .await
                }
            }
            None => Err(Error::Config(format!(
                "consumer target {target} requires consumerFlakeRef"
            ))),
        }
    } else {
        ext.eval_attr_value(EvalAttrRequest {
            flake_ref: &context.flake_ref,
            rev,
            arch: &context.arch,
            flake_output: &context.flake_output,
            attr_prefix: &context.full_attr_prefix,
            pkg,
            attr: &rule.version_attr,
        })
        .await
    };
    match value {
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

    #[test]
    fn attr_paths_split_like_nix_and_quote_into_literals() {
        assert_eq!(
            attr_path_segments(r#"checks.x86_64-linux."a.b"c"#).unwrap(),
            ["checks", "x86_64-linux", "a.bc"]
        );
        assert_eq!(attr_path_segments("a..b"), None);
        assert_eq!(attr_path_segments(r#"a."b"#), None);
        let path = ["x".to_string(), r#"${a}"\"#.to_string()];
        assert_eq!(nix_path_lists(&[&path]), r#"[ [ "x" "\${a}\"\\" ] ]"#);
    }

    #[test]
    fn requests_batch_by_flake_and_flags_with_their_per_item_paths() {
        let consumer = |target| StorePathRequest::Consumer {
            consumer_flake_ref: ".#",
            candidate: None,
            target,
        };
        let (first, path) = batch_of(&consumer("a.b")).unwrap();
        assert_eq!(batch_of(&consumer("c")).unwrap().0, first);
        assert_eq!(path, ["a", "b", "outPath"]);
        assert_eq!(first.flake, ".");
        let candidate = StorePathRequest::Consumer {
            consumer_flake_ref: ".",
            candidate: Some(ConsumerOverride {
                input_name: "nixpkgs",
                source_flake_ref: "github:NixOS/nixpkgs",
                rev: "abc",
            }),
            target: "a.b",
        };
        assert_ne!(batch_of(&candidate).unwrap().0, first);
        let (producer, path) = batch_of(&StorePathRequest::Producer {
            flake_ref: "github:NixOS/nixpkgs",
            rev: "abc",
            arch: "x86_64-linux",
            flake_output: "legacyPackages",
            attr_prefix: "python3Packages",
            pkg: "numpy",
        })
        .unwrap();
        assert!(producer.installable);
        assert_eq!(
            path,
            [
                "legacyPackages",
                "x86_64-linux",
                "python3Packages",
                "numpy",
                "outPath"
            ]
        );
        let (locked, _) = batch_of(&StorePathRequest::LockedInput {
            consumer_flake_ref: ".",
            target: "inputs.nixpkgs.legacyPackages.x86_64-linux.hello",
        })
        .unwrap();
        assert!(!locked.installable);
    }

    #[tokio::test]
    async fn cache_receipts_redact_credentials_but_preserve_lookup_path_and_status() {
        use wiremock::{
            matchers::{method, path, query_param},
            Mock, MockServer, ResponseTemplate,
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/prefix/hash.narinfo"))
            .and(query_param("token", "fixture-query-secret"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let mut url = reqwest::Url::parse(&format!(
            "{}/prefix?token=fixture-query-secret",
            server.uri()
        ))
        .unwrap();
        url.set_username("fixture-user").unwrap();
        url.set_password(Some("fixture-password")).unwrap();
        let raw = url.to_string();
        let result = check_narinfo_availability(
            &Client::new(),
            "/nix/store/hash-app",
            std::slice::from_ref(&raw),
        )
        .await;
        let evidence = format!("{result:?}");
        for credential in ["fixture-user", "fixture-password", "fixture-query-secret"] {
            assert!(!evidence.contains(credential));
        }
        assert!(result.attempts[0].url.ends_with("/prefix/hash.narinfo"));
        assert_eq!(result.attempts[0].status, Some(500));
        let mut cfg = crate::config::PinConfig::from_json(r#"{"name":"test","packages":["hello"],"inputName":"nixpkgs","attrPrefix":"","pythonPackages":null,"caches":[],"hydraJobset":"jobset","hydraUrl":"hydra","hydraJobPattern":"pattern","hydraRevInput":"nixpkgs","depth":1,"branch":"main","flakeRef":"github:NixOS/nixpkgs","flakeOutput":"legacyPackages","failFast":false,"arch":"x86_64-linux"}"#).unwrap();
        cfg.caches = vec![raw];
        let serialized = serde_json::to_string(&cfg).unwrap();
        assert!(!serialized.contains("fixture-"));
        assert!(serialized.contains("/prefix"));
    }

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
        crate::test_cache::hit(
            &server,
            "/nix/store/00000000000000000000000000000000-hello-2.12",
        )
        .await;

        let client = Client::new();
        let caches = vec![server.uri()];
        assert!(
            check_narinfo(
                &client,
                "/nix/store/00000000000000000000000000000000-hello-2.12",
                &caches
            )
            .await
        );
    }

    #[tokio::test]
    async fn test_check_narinfo_cache_miss() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/abc123.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = Client::new();
        let caches = vec![server.uri()];
        let result =
            check_narinfo_availability(&client, "/nix/store/abc123-hello-2.12", &caches).await;
        assert_eq!(result.availability, Availability::Missing);
        assert_eq!(result.attempts[0].status, Some(404));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_check_narinfo_never_queries_compromised_cache() {
        let result = check_narinfo_availability(
            &Client::new(),
            "/nix/store/abc123-hello-2.12",
            &["https://attic%2exuyh0120.win/lantian".to_string()],
        )
        .await;
        assert_eq!(result.availability, Availability::Unknown);
        assert!(result.attempts.is_empty());
        assert!(result.error.unwrap().contains("compromised"));
    }

    #[tokio::test]
    async fn test_check_narinfo_server_error_is_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
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
        assert_eq!(result.attempts[0].status, Some(500));
        assert!(result.error.unwrap().contains("HTTP 500"));
    }

    #[tokio::test]
    async fn test_check_narinfo_multiple_caches_second_hit() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/00000000000000000000000000000000.narinfo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server1)
            .await;
        crate::test_cache::hit(
            &server2,
            "/nix/store/00000000000000000000000000000000-hello-2.12",
        )
        .await;

        let client = Client::new();
        let caches = vec![server1.uri(), server2.uri()];
        assert!(
            check_narinfo(
                &client,
                "/nix/store/00000000000000000000000000000000-hello-2.12",
                &caches
            )
            .await
        );
    }

    #[tokio::test]
    async fn test_check_narinfo_all_caches_miss() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server1)
            .await;
        Mock::given(method("GET"))
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
    async fn non_content_success_statuses_are_unknown() {
        for status in [204, 206] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let path = "/nix/store/00000000000000000000000000000000-app";
            for check in [
                check_narinfo_availability(&Client::new(), path, &[server.uri()]).await,
                check_narinfo_closure_availability(&Client::new(), path, &[server.uri()]).await,
            ] {
                assert_eq!(check.availability, Availability::Unknown);
                assert_eq!(check.attempts[0].status, Some(status));
            }
        }
    }

    #[tokio::test]
    async fn content_statuses_match_nix_filetransfer() {
        for (status, availability) in [
            (200, Availability::Cached),
            (201, Availability::Cached),
            (203, Availability::Unknown),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status).set_body_string("StorePath: /nix/store/00000000000000000000000000000000-app\nURL: nar/app.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\n"))
                .mount(&server).await;
            for result in [
                check_narinfo_availability(
                    &Client::new(),
                    "/nix/store/00000000000000000000000000000000-app",
                    &[server.uri()],
                )
                .await,
                check_narinfo_closure_availability(
                    &Client::new(),
                    "/nix/store/00000000000000000000000000000000-app",
                    &[server.uri()],
                )
                .await,
            ] {
                assert_eq!(result.availability, availability, "HTTP {status}");
                assert_eq!(result.attempts[0].status, Some(status));
            }
        }
    }

    #[tokio::test]
    async fn oversized_metadata_is_unknown_and_allows_fallback() {
        let oversized = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("a".repeat(1048577)))
            .mount(&oversized)
            .await;
        let valid = MockServer::start().await;
        crate::test_cache::hit(&valid, "/nix/store/00000000000000000000000000000000-app").await;
        let result = check_narinfo_availability(
            &Client::new(),
            "/nix/store/00000000000000000000000000000000-app",
            &[oversized.uri(), valid.uri()],
        )
        .await;
        assert_eq!(result.availability, Availability::Cached);
        assert!(result.attempts[0].error.as_ref().unwrap().contains("limit"));
        assert!(result.attempts[1].error.is_none());
    }

    #[tokio::test]
    async fn chunked_metadata_is_bounded_without_content_length() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.extend(byte);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            // Valid-looking metadata padded past the byte limit, without a
            // Content-Length header on which the client could rely.
            let mut body = "StorePath: /nix/store/00000000000000000000000000000000-app\nURL: nar/app.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nPadding: ".to_string();
            body.push_str(&"a".repeat(1048576));
            body.push('\n');
            let _ = write!(stream, "{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body);
        });
        let result = check_narinfo_availability(
            &Client::new(),
            "/nix/store/00000000000000000000000000000000-app",
            &[format!("http://{address}")],
        )
        .await;
        server.join().unwrap();
        assert_eq!(result.availability, Availability::Unknown);
        assert!(result.attempts[0].error.as_ref().unwrap().contains("limit"));
    }

    #[tokio::test]
    async fn malformed_narinfo_cannot_satisfy_a_required_closure() {
        let store_path = "/nix/store/00000000000000000000000000000000-app";
        for body in [
            "",
            "<html>cache maintenance</html>\n",
            "StorePath: /nix/store/00000000000000000000000000000000-app\nReferences: \n",
            "StorePath: /nix/store/11111111111111111111111111111111-other\nURL: nar/other.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: \n",
            "StorePath: /nix/store/00000000000000000000000000000000-other\nURL: nar/other.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: \n",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            for result in [
                check_narinfo_availability(&Client::new(), store_path, &[server.uri()]).await,
                check_narinfo_closure_availability(&Client::new(), store_path, &[server.uri()]).await,
            ] {
                assert_eq!(result.availability, Availability::Unknown, "body: {body:?}");
                assert!(result.error.is_some());
                assert_eq!(result.attempts[0].status, Some(200));
                assert!(result.attempts[0].error.is_some());
            }
        }
    }

    #[tokio::test]
    async fn malformed_cache_falls_back_to_a_valid_leaf_and_retains_the_error() {
        let invalid = MockServer::start().await;
        let valid = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>\n"))
            .mount(&invalid)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "StorePath: /nix/store/00000000000000000000000000000000-app\nURL: nar/app.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: \n",
            ))
            .mount(&valid)
            .await;
        let result = check_narinfo_closure_availability(
            &Client::new(),
            "/nix/store/00000000000000000000000000000000-app",
            &[invalid.uri(), valid.uri()],
        )
        .await;
        assert_eq!(result.availability, Availability::Cached);
        assert_eq!(
            result.cache.as_deref(),
            Some(format!("{}/", valid.uri()).as_str())
        );
        assert_eq!(result.attempts.len(), 2);
        assert!(result.attempts[0].error.is_some());
        assert!(result.attempts[1].error.is_none());
    }

    #[tokio::test]
    async fn test_check_narinfo_closure_follows_references() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/00000000000000000000000000000000.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        "StorePath: /nix/store/00000000000000000000000000000000-app\nURL: nar/app.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: 11111111111111111111111111111111-dependency\n",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/11111111111111111111111111111111.narinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "StorePath: /nix/store/11111111111111111111111111111111-dependency\nURL: nar/dep.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: \n",
            ))
            .mount(&server)
            .await;

        let client = Client::new();
        assert!(
            check_narinfo_closure(
                &client,
                "/nix/store/00000000000000000000000000000000-app",
                &[server.uri()]
            )
            .await
        );
    }

    #[tokio::test]
    async fn test_check_narinfo_closure_rejects_missing_reference() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/00000000000000000000000000000000.narinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("StorePath: /nix/store/00000000000000000000000000000000-app\nURL: nar/app.nar\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: 11111111111111111111111111111111-missing\n"),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        let result = check_narinfo_closure_availability(
            &client,
            "/nix/store/00000000000000000000000000000000-app",
            &[server.uri()],
        )
        .await;
        assert_eq!(result.availability, Availability::Missing);
        assert_eq!(result.attempts.len(), 2);
        assert!(result.attempts[1]
            .url
            .ends_with("/11111111111111111111111111111111.narinfo"));
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
