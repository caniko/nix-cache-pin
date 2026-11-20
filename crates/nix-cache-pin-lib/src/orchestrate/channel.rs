//! Channel release candidates backed by the public release bucket and Hydra.
//!
//! A channel branch such as `nixos-unstable` publishes one release directory
//! per successful Hydra evaluation. Those releases are the candidate set: the
//! bucket orders them by release number, and Hydra's build history says which
//! of them have every required build finished. Only a release with a Hydra-built
//! required package and no failed or unfinished one is evaluated; walking
//! individual commits (the fallback) costs a full consumer evaluation for
//! revisions Hydra never built.

use super::report::format_rev_summary;
use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::ext::ExternalCommands;
use crate::flakeref;
use crate::hydra::{self, HydraBuildDetail};
use crate::narinfo;
use crate::output::{style, Output};
use reqwest::Client;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Public bucket holding every NixOS/nixpkgs channel release.
const RELEASES_URL: &str = "https://nix-releases.s3.amazonaws.com";

/// Builds inspected per Hydra job. A channel release produces at most one
/// build per job, so this covers the release window with margin.
const HYDRA_BUILD_HISTORY: usize = 50;

/// Upper bound on S3 listing pages. Every channel family is far below it, and
/// an unexpected listing cannot make the search unbounded.
const MAX_LISTING_PAGES: usize = 64;

/// Cap a cached response so a malformed backend cannot fill the cache.
const MAX_CACHED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChannelOutcome {
    /// A channel release was confirmed fully cached.
    Found(String),
    /// Channel evidence is conclusive: no newer release is usable. The caller
    /// must not fall back to a commit walk or any evaluation.
    Exhausted,
    /// The channel could not be resolved or Hydra could not provide evidence;
    /// use the revision-history fallback.
    Unavailable,
}

/// The S3 and Hydra names for one channel branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChannelSpec {
    /// Key prefix that contains every release of this channel.
    releases_prefix: String,
    /// Release name stem, e.g. `nixos-`.
    release_stem: &'static str,
    hydra_project: &'static str,
    hydra_jobset: String,
    /// Every channel job name starts with this (`nixpkgs.` for the NixOS
    /// jobset, empty for the nixpkgs jobset).
    job_prefix: &'static str,
}

/// Map a source branch to a channel release family.
///
/// Only nixpkgs channel branches have releases in the public bucket. Anything
/// else returns `None` and keeps the existing commit walk.
pub(super) fn channel_for(flake_ref: &str, branch: &str) -> Option<ChannelSpec> {
    if flakeref::extract_github_repo(flake_ref)? != "NixOS/nixpkgs" {
        return None;
    }

    if branch == "nixpkgs-unstable" {
        return Some(ChannelSpec {
            releases_prefix: "nixpkgs/nixpkgs-".to_string(),
            release_stem: "nixpkgs-",
            hydra_project: "nixpkgs",
            hydra_jobset: "unstable".to_string(),
            job_prefix: "",
        });
    }

    let rest = branch.strip_prefix("nixos-")?;
    let jobset = match rest {
        "unstable" | "unstable-small" => rest.to_string(),
        version if is_nixos_version(version) => format!("release-{version}"),
        _ => return None,
    };
    Some(ChannelSpec {
        releases_prefix: format!("nixos/{rest}/nixos-"),
        release_stem: "nixos-",
        hydra_project: "nixos",
        hydra_jobset: jobset,
        job_prefix: "nixpkgs.",
    })
}

fn is_nixos_version(rest: &str) -> bool {
    let base = rest.strip_suffix("-small").unwrap_or(rest);
    let mut parts = base.split('.');
    let major = parts.next().unwrap_or_default();
    let minor = parts.next().unwrap_or_default();
    parts.next().is_none()
        && !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit())
}

/// One release directory from the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelRelease {
    /// Version component of the release name, e.g. `26.11pre1117658`.
    version: String,
    /// Major/minor family, e.g. `26.11`.
    family: String,
    /// Release counter, used to order releases numerically.
    number: u64,
    short_rev: String,
    /// Full object key, without a trailing slash.
    key: String,
}

impl ChannelRelease {
    fn name(&self) -> &str {
        self.key.rsplit('/').next().unwrap_or(&self.key)
    }
}

#[derive(Debug, Default)]
struct ListingPage {
    releases: Vec<ChannelRelease>,
    next: Option<String>,
    truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobState {
    Success {
        out_path: Option<String>,
        /// Every output path of the build, so a consumer path for a
        /// non-default output is not mistaken for a divergent build.
        outputs: Vec<String>,
    },
    Failed {
        build: i64,
    },
    Unfinished,
    /// No explicit evidence: the eval is absent from the bounded build
    /// history, or release/build metadata could not be fetched.
    Unknown,
}

impl JobState {
    fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

/// A release whose Hydra evidence was inspected, with the state of every
/// required job.
struct CheckedRelease {
    release: ChannelRelease,
    states: Vec<(String, JobState)>,
}

impl CheckedRelease {
    fn state(&self, package: &str) -> Option<&JobState> {
        self.states
            .iter()
            .find(|(name, _)| name == package)
            .map(|(_, state)| state)
    }
}

/// Inspect one job's build history once and answer eval membership from it.
struct JobHistory {
    builds: Vec<i64>,
    details: Vec<Option<HydraBuildDetail>>,
}

impl JobHistory {
    async fn load(
        client: &Client,
        cfg: &PinConfig,
        spec: &ChannelSpec,
        job: String,
    ) -> Result<Self> {
        let builds = hydra::query_latest_builds(
            client,
            &cfg.hydra_url,
            spec.hydra_project,
            &spec.hydra_jobset,
            &job,
            HYDRA_BUILD_HISTORY,
        )
        .await?;
        Ok(Self {
            details: vec![None; builds.len()],
            builds,
        })
    }

    /// State of this job at one evaluation. Details are fetched lazily and
    /// newest-first, so a successful newest build costs one request.
    async fn state_at(
        &mut self,
        client: &Client,
        hydra_url: &str,
        cache: &ResponseCache,
        eval_id: i64,
    ) -> Result<JobState> {
        for index in 0..self.builds.len() {
            if self.details[index].is_none() {
                let build_id = self.builds[index];
                let url = format!("{}/build/{build_id}", hydra_url.trim_end_matches('/'));
                let body = cache.text(client, &url).await?;
                let detail: HydraBuildDetail = serde_json::from_str(&body).map_err(|error| {
                    Error::Config(format!("invalid Hydra build {build_id}: {error}"))
                })?;
                if detail.finished != Some(1) {
                    // An unfinished build can gain evaluations or change status
                    // between releases, so never keep its response. Only an
                    // explicit `finished = 0` proves it unfinished.
                    cache.forget(&url);
                    if detail.jobsetevals.contains(&eval_id) {
                        return Ok(if detail.finished == Some(0) {
                            JobState::Unfinished
                        } else {
                            JobState::Unknown
                        });
                    }
                    continue;
                }
                self.details[index] = Some(detail);
            }
            let detail = self.details[index]
                .as_ref()
                .expect("finished detail was just fetched or already present");
            if detail.jobsetevals.contains(&eval_id) {
                if detail.buildstatus == 0 {
                    return Ok(JobState::Success {
                        out_path: detail.out_path().map(str::to_string),
                        outputs: detail
                            .buildoutputs
                            .iter()
                            .flatten()
                            .filter_map(|(_, output)| output.path.clone())
                            .collect(),
                    });
                }
                return Ok(JobState::Failed { build: detail.id });
            }
        }
        // The eval is outside the bounded history: no evidence, not a failure.
        Ok(JobState::Unknown)
    }
}

/// Run-scoped response cache. Finished Hydra builds and release `src-url`
/// files never change, so they are kept for the run; nothing outlives the
/// invocation.
struct ResponseCache {
    bodies: std::sync::Mutex<HashMap<String, String>>,
}

impl ResponseCache {
    fn run_scoped() -> Self {
        Self {
            bodies: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self::run_scoped()
    }

    fn read(&self, url: &str) -> Option<String> {
        self.bodies.lock().unwrap().get(url).cloned()
    }

    fn write(&self, url: &str, body: &str) {
        if body.len() > MAX_CACHED_BYTES {
            return;
        }
        self.bodies
            .lock()
            .unwrap()
            .insert(url.to_string(), body.to_string());
    }

    /// Drop a cached response that may still change, such as an unfinished
    /// Hydra build.
    fn forget(&self, url: &str) {
        self.bodies.lock().unwrap().remove(url);
    }

    async fn text(&self, client: &Client, url: &str) -> Result<String> {
        if let Some(body) = self.read(url) {
            return Ok(body);
        }
        let body = hydra::get_text(client, url).await?;
        self.write(url, &body);
        Ok(body)
    }
}

/// List and order every release in a channel family, newest first.
async fn list_releases(
    client: &Client,
    releases_url: &str,
    spec: &ChannelSpec,
) -> Result<Vec<ChannelRelease>> {
    let mut releases = Vec::new();
    let mut token: Option<String> = None;
    let mut seen_pages = HashSet::new();
    for _ in 0..MAX_LISTING_PAGES {
        let url = listing_url(releases_url, &spec.releases_prefix, token.as_deref())?;
        let body = hydra::get_text(client, &url).await?;
        let page = parse_listing(&body, spec);
        releases.extend(page.releases);
        if !page.truncated {
            return Ok(newest_family(releases));
        }
        let next = page.next.ok_or_else(|| {
            Error::Config("release listing is truncated without a continuation token".into())
        })?;
        if !seen_pages.insert(next.clone()) {
            return Err(Error::Config(
                "release listing repeated a continuation token".into(),
            ));
        }
        token = Some(next);
    }
    Err(Error::Config(format!(
        "release listing did not finish within {MAX_LISTING_PAGES} pages"
    )))
}

fn listing_url(base: &str, prefix: &str, token: Option<&str>) -> Result<String> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|error| Error::Config(format!("invalid releases URL {base}: {error}")))?;
    url.query_pairs_mut()
        .append_pair("list-type", "2")
        .append_pair("delimiter", "/")
        .append_pair("prefix", prefix)
        .append_pair("max-keys", "1000");
    if let Some(token) = token {
        url.query_pairs_mut()
            .append_pair("continuation-token", token);
    }
    Ok(url.into())
}

/// Parse one S3 `ListObjectsV2` page. A release appears once as an object key
/// and once as a common prefix; both are read and duplicates collapse in
/// [`newest_family`], so a release missing either entry is still listed.
fn parse_listing(body: &str, spec: &ChannelSpec) -> ListingPage {
    let keys = tag_values(body, "Key");
    let prefixes = tag_values(body, "Prefix");
    let releases = keys
        .iter()
        .chain(prefixes.iter())
        .filter_map(|key| parse_release(key.trim_end_matches('/'), spec))
        .collect();
    let truncated = body.contains("<IsTruncated>true</IsTruncated>");
    let next = tag_values(body, "NextContinuationToken")
        .into_iter()
        .next()
        .filter(|_| truncated);
    ListingPage {
        releases,
        next,
        truncated,
    }
}

fn tag_values(body: &str, tag: &str) -> Vec<String> {
    let pattern = format!("<{tag}>([^<]*)</{tag}>");
    let regex = regex::Regex::new(&pattern).expect("static tag pattern is valid");
    regex
        .captures_iter(body)
        .map(|capture| capture[1].to_string())
        .collect()
}

fn parse_release(key: &str, spec: &ChannelSpec) -> Option<ChannelRelease> {
    let name = key.rsplit('/').next()?;
    let rest = name.strip_prefix(spec.release_stem)?;
    let (version, short_rev) = rest.rsplit_once('.')?;
    if version.is_empty()
        || !(7..=40).contains(&short_rev.len())
        || !short_rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(ChannelRelease {
        version: version.to_string(),
        family: release_family(version)?,
        number: release_number(version),
        short_rev: short_rev.to_string(),
        key: key.to_string(),
    })
}

fn release_family(version: &str) -> Option<String> {
    let (major, rest) = version.split_once('.')?;
    let minor_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if major.is_empty() || minor_len == 0 || !major.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("{major}.{}", &rest[..minor_len]))
}

/// Numeric release order: `26.11pre1117658`, `25.11.9840` and `25.11beta186`
/// all sort by the counter after their major/minor version.
fn release_number(version: &str) -> u64 {
    let Some((_, rest)) = version.split_once('.') else {
        return 0;
    };
    let after_minor = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let digits: String = after_minor
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

/// Keep the newest version family only: an unstable channel keeps releases
/// from every past development cycle in the same directory.
fn newest_family(releases: Vec<ChannelRelease>) -> Vec<ChannelRelease> {
    let Some(newest) = releases.iter().max_by_key(|release| release.number) else {
        return Vec::new();
    };
    let family = newest.family.clone();
    let mut releases: Vec<ChannelRelease> = releases
        .into_iter()
        .filter(|release| release.family == family)
        .collect();
    releases.sort_unstable_by(|a, b| {
        b.number
            .cmp(&a.number)
            .then_with(|| b.version.cmp(&a.version))
    });
    releases.dedup_by(|a, b| a.version == b.version && a.short_rev == b.short_rev);
    releases
}

/// Hydra job name for a package in the channel's own jobset.
fn channel_job(cfg: &PinConfig, spec: &ChannelSpec, package: &str) -> String {
    format!(
        "{}{}.{}",
        spec.job_prefix,
        crate::narinfo::build_attr(channel_prefix(cfg.full_attr_prefix()), package),
        cfg.arch
    )
}

/// The attribute prefix as the channel jobset spells it. `pkgs` is the legacy
/// package set the channel jobset builds as bare attributes, and
/// `nixpkgs.pkgs.cosmic-app-library.x86_64-linux` is not a job: drop exactly
/// that one leading path component. Every other prefix (`pkgsCuda`,
/// `pkgsRocm`, a Python package set) is kept, because guessing it would turn a
/// job the jobset does not have into a confident name.
fn channel_prefix(prefix: &str) -> &str {
    if prefix == "pkgs" {
        ""
    } else if let Some(rest) = prefix.strip_prefix("pkgs.") {
        rest
    } else {
        prefix
    }
}

async fn release_eval(
    client: &Client,
    releases_url: &str,
    cache: &ResponseCache,
    release: &ChannelRelease,
) -> Result<i64> {
    let url = format!(
        "{}/{}/src-url",
        releases_url.trim_end_matches('/'),
        release.key
    );
    let body = cache.text(client, &url).await?;
    parse_eval_url(&body).ok_or_else(|| {
        Error::Config(format!(
            "{} has no Hydra eval URL: {}",
            release.name(),
            body.trim()
        ))
    })
}

fn parse_eval_url(body: &str) -> Option<i64> {
    let trimmed = body.trim();
    if let Ok(id) = trimmed.parse::<i64>() {
        return Some(id);
    }
    let marker = "/eval/";
    let index = trimmed.find(marker)?;
    let digits: String = trimmed[index + marker.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

async fn resolve_rev<E: ExternalCommands + 'static>(
    ext: &Arc<E>,
    repo: &str,
    branch: &str,
    release: &ChannelRelease,
) -> Result<String> {
    ext.resolve_revision(repo, branch, &release.short_rev).await
}

/// Resolve every release in the window and keep those that are ancestors of
/// the configured branch tip and newer than the current pin.
///
/// `None` means a release revision could not be resolved: the window is
/// unknown, not conclusively empty, so the caller must fall back.
async fn eligible_releases<E: ExternalCommands + 'static>(
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
    repo: &str,
    releases: &[ChannelRelease],
) -> Result<Option<Vec<(ChannelRelease, String)>>> {
    let mut eligible = Vec::with_capacity(releases.len());
    for release in releases {
        let rev = match resolve_rev(ext, repo, &cfg.branch, release).await {
            Ok(rev) => rev,
            Err(error) => {
                out.milestone(format!(
                    "{}",
                    style(format!(
                        "  Could not resolve channel release {}: {error}; falling back to revision history",
                        release.name()
                    ))
                    .yellow()
                ));
                return Ok(None);
            }
        };
        if super::candidate_is_allowed(cfg, current_rev, &rev, out, ext).await? {
            eligible.push((release.clone(), rev));
        }
    }
    Ok(Some(eligible))
}

/// Search channel releases for the newest fully cached revision.
pub(super) async fn search<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<ChannelOutcome> {
    if !hydra::is_public_hydra(&cfg.hydra_url) {
        return Ok(ChannelOutcome::Unavailable);
    }
    let cache = ResponseCache::run_scoped();
    search_releases(client, cfg, out, ext, current_rev, RELEASES_URL, &cache).await
}

/// Search against an explicit release bucket, so tests can fake it.
#[cfg(test)]
pub(super) async fn search_in_memory<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
    releases_url: &str,
) -> Result<ChannelOutcome> {
    search_releases(
        client,
        cfg,
        out,
        ext,
        current_rev,
        releases_url,
        &ResponseCache::memory(),
    )
    .await
}

async fn search_releases<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
    releases_url: &str,
    cache: &ResponseCache,
) -> Result<ChannelOutcome> {
    let Some(spec) = channel_for(&cfg.flake_ref, &cfg.branch) else {
        return Ok(ChannelOutcome::Unavailable);
    };
    let Some(repo) = flakeref::extract_github_repo(&cfg.flake_ref) else {
        return Ok(ChannelOutcome::Unavailable);
    };

    out.set_action(format!(
        "{}",
        style(format!("Listing {} channel releases...", cfg.branch)).cyan()
    ));
    let releases = match list_releases(client, releases_url, &spec).await {
        Ok(releases) if !releases.is_empty() => releases,
        Ok(_) => {
            out.milestone(format!(
                "{}",
                style(format!(
                    "  No {} releases listed; falling back to revision history",
                    cfg.branch
                ))
                .yellow()
            ));
            return Ok(ChannelOutcome::Unavailable);
        }
        Err(error) => {
            out.milestone(format!(
                "{}",
                style(format!(
                    "  Channel releases unavailable for {}: {error}; falling back to revision history",
                    cfg.branch
                ))
                .yellow()
            ));
            return Ok(ChannelOutcome::Unavailable);
        }
    };
    let releases = &releases[..releases.len().min(cfg.depth.max(1))];
    out.milestone(format!(
        "  {}: newest {} ({} candidate release(s) in window)",
        cfg.branch,
        releases[0].name(),
        releases.len()
    ));

    // Every release is validated as an ancestor of this tip and newer than the
    // current pin before any Hydra evidence is trusted.
    if out.branch_tip().is_none() {
        out.milestone(format!(
            "{}",
            style(format!(
                "  Could not resolve the {} branch tip; falling back to revision history",
                cfg.branch
            ))
            .yellow()
        ));
        return Ok(ChannelOutcome::Unavailable);
    }
    let Some(window) = eligible_releases(cfg, out, ext, current_rev, repo, releases).await? else {
        return Ok(ChannelOutcome::Unavailable);
    };
    if window.is_empty() {
        out.milestone(format!(
            "{}",
            style(format!(
                "  No {} channel release is a newer ancestor of the branch tip.",
                cfg.branch
            ))
            .dim()
        ));
        return Ok(ChannelOutcome::Exhausted);
    }

    // Every candidate release names its Hydra eval; resolve them once so the
    // evidence pass costs nothing but the job queries.
    let mut eval_ids = Vec::with_capacity(window.len());
    for (release, _) in &window {
        let eval_id = match release_eval(client, releases_url, cache, release).await {
            Ok(eval_id) => Some(eval_id),
            Err(error) => {
                out.milestone(format!(
                    "{}",
                    style(format!(
                        "  {}: Hydra evidence unknown ({error})",
                        release.name()
                    ))
                    .yellow()
                ));
                None
            }
        };
        eval_ids.push(eval_id);
    }

    // Evidence per candidate release, in configured package order. A package
    // that explicitly failed at every candidate is conclusive on its own, so
    // the pass stops there instead of spending a request per remaining job.
    let mut evidence: Vec<Vec<(String, JobState)>> = vec![Vec::new(); window.len()];
    for package in &cfg.packages {
        let job = channel_job(cfg, &spec, package);
        let mut history = match JobHistory::load(client, cfg, &spec, job.clone()).await {
            Ok(history) => {
                // Hydra never builds some attributes (aliases, unfree or
                // non-Hydra packages). Their state stays unknown and the
                // release evaluation checks them, so the other packages'
                // evidence still selects the release.
                if history.builds.is_empty() {
                    out.milestone(format!(
                        "{}",
                        style(format!(
                            "  No Hydra build history for {package} as {job}; checking it by evaluation"
                        ))
                        .yellow()
                    ));
                }
                history
            }
            Err(error) => {
                out.milestone(format!(
                    "{}",
                    style(format!(
                        "  Hydra evidence unavailable for {package} ({job}): {error}; falling back to revision history"
                    ))
                    .yellow()
                ));
                return Ok(ChannelOutcome::Unavailable);
            }
        };

        let mut failures = BTreeSet::new();
        let mut failed_everywhere = !window.is_empty();
        for (index, eval_id) in eval_ids.iter().enumerate() {
            let state = match eval_id {
                Some(eval_id) => {
                    match history
                        .state_at(client, &cfg.hydra_url, cache, *eval_id)
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => {
                            out.milestone(format!(
                                "{}",
                                style(format!(
                                    "  {}: Hydra evidence unknown for {}: {error}",
                                    window[index].0.name(),
                                    package
                                ))
                                .yellow()
                            ));
                            JobState::Unknown
                        }
                    }
                }
                None => JobState::Unknown,
            };
            match &state {
                JobState::Failed { build } => {
                    failures.insert(*build);
                }
                _ => failed_everywhere = false,
            }
            evidence[index].push((package.clone(), state));
        }
        if failed_everywhere {
            return Ok(fail_everywhere(cfg, out, package, &failures, &window));
        }
    }

    let mut checked = Vec::with_capacity(window.len());
    // Consumer-specific targets that missed at every release evaluated so
    // far; `None` until one is evaluated or once one is skipped.
    let mut always_missing: Option<BTreeSet<String>> = None;
    let mut skipped = false;
    // ponytail: one release with partial evidence is evaluated, so a target
    // Hydra never builds costs at most one evaluation over the commit walk.
    let mut partial_budget = 1;
    for (index, (release, rev)) in window.iter().enumerate() {
        let eval_id = eval_ids[index];
        let states = &evidence[index];
        out.milestone(format_release_status(release, eval_id, states));
        // Unknown evidence for one package does not hide a release the other
        // packages prove built: the evaluation checks it like any target.
        let partial = states
            .iter()
            .any(|(_, state)| matches!(state, JobState::Unknown));
        let confirmable = states.iter().any(|(_, state)| state.is_success())
            && states
                .iter()
                .all(|(_, state)| matches!(state, JobState::Success { .. } | JobState::Unknown))
            && (!partial || partial_budget > 0);
        let confirmed = match eval_id {
            Some(eval_id) if confirmable => {
                partial_budget -= usize::from(partial);
                let mut misses = BTreeSet::new();
                let confirmed = confirm(
                    client,
                    cfg,
                    out,
                    ext,
                    release,
                    rev,
                    eval_id,
                    states,
                    &mut misses,
                )
                .await?;
                always_missing = Some(match always_missing {
                    Some(previous) => previous.intersection(&misses).cloned().collect(),
                    None => misses,
                });
                confirmed
            }
            _ => {
                skipped = true;
                None
            }
        };
        match confirmed {
            Some(rev) => return Ok(ChannelOutcome::Found(rev)),
            None => checked.push(CheckedRelease {
                release: release.clone(),
                states: states.clone(),
            }),
        }
    }

    // The commit walk is skipped only when every eligible release was
    // evaluated and the same consumer-specific target missed at each.
    let always_missing = always_missing.filter(|_| !skipped).unwrap_or_default();
    if !always_missing.is_empty() {
        let packages: Vec<_> = always_missing.into_iter().collect();
        out.milestone(format!(
            "{}",
            style(format!(
                "  No newer cached revision: the consumer's own build of {} is not fully cached at any Hydra-built release.",
                packages.join(", ")
            ))
            .red()
        ));
        out.milestone(
            "    Only the consumer builds these paths, so the commit walk is skipped; build and push them with their missing paths, or stop requiring them.",
        );
        return Ok(ChannelOutcome::Exhausted);
    }

    Ok(no_candidate(cfg, out, &cfg.packages, &checked))
}

/// Report a package that explicitly failed at every candidate release: the
/// search is conclusive, so nothing is evaluated.
fn fail_everywhere(
    cfg: &PinConfig,
    out: &mut Output,
    package: &str,
    failures: &BTreeSet<i64>,
    window: &[(ChannelRelease, String)],
) -> ChannelOutcome {
    let newest = window
        .first()
        .map(|(release, _)| release.name())
        .unwrap_or("unknown");
    let oldest = window
        .last()
        .map(|(release, _)| release.name())
        .unwrap_or("unknown");
    let urls = failures
        .iter()
        .map(|build| format!("{}/build/{build}", cfg.hydra_url.trim_end_matches('/')))
        .collect::<Vec<_>>()
        .join(", ");
    out.milestone(format!(
        "{}",
        style("  No newer cached revision: channel builds failed for required packages.").red()
    ));
    out.milestone(format!(
        "    {package}: failed at every channel release checked ({urls}; covering {newest} … {oldest})"
    ));
    out.milestone("    No evaluations were run for this verdict.");
    ChannelOutcome::Exhausted
}

fn format_release_status(
    release: &ChannelRelease,
    eval_id: Option<i64>,
    states: &[(String, JobState)],
) -> String {
    let mut parts = Vec::with_capacity(states.len());
    for (package, state) in states {
        parts.push(format!("{package} {}", state_label(state)));
    }
    let detail = if parts.is_empty() {
        "no producer jobs".to_string()
    } else {
        parts.join(", ")
    };
    match eval_id {
        Some(eval_id) => format!("  {} (eval {eval_id}): {detail}", release.name()),
        None => format!("  {} (eval unknown): {detail}", release.name()),
    }
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::Success { .. } => "built",
        JobState::Failed { .. } => "failed",
        JobState::Unfinished => "not built yet",
        JobState::Unknown => "unknown",
    }
}

/// Report why no channel release qualified, without evaluating anything.
fn no_candidate(
    cfg: &PinConfig,
    out: &mut Output,
    packages: &[String],
    checked: &[CheckedRelease],
) -> ChannelOutcome {
    if packages.is_empty() {
        out.milestone(format!(
            "{}",
            style(
                "  No channel release could be confirmed without producer build evidence; falling back to revision history"
            )
            .yellow()
        ));
        return ChannelOutcome::Unavailable;
    }

    let mut unbuilt = Vec::new();
    let mut succeeded_somewhere = 0;

    for package in packages {
        let mut failures = BTreeSet::new();
        let mut unfinished = Vec::new();
        let mut success = false;
        for release in checked {
            match release.state(package) {
                Some(JobState::Success { .. }) => success = true,
                Some(JobState::Failed { build }) => {
                    failures.insert(*build);
                }
                Some(JobState::Unfinished) => unfinished.push(release.release.name()),
                Some(JobState::Unknown) | None => {}
            }
        }
        if success {
            succeeded_somewhere += 1;
            continue;
        }
        if !unfinished.is_empty() {
            let detail = if failures.is_empty() {
                format!("{} release(s) not built yet", unfinished.len())
            } else {
                format!(
                    "last failure {}/build/{}; {} newer release(s) not built yet",
                    cfg.hydra_url.trim_end_matches('/'),
                    failures.iter().next().expect("failures is non-empty"),
                    unfinished.len()
                )
            };
            unbuilt.push((package.clone(), detail));
        }
    }

    // A release is still possibly usable only when its non-successful states
    // are all Unknown. Missing evidence there could change the verdict, so it
    // must fall back instead of being read as a proven failure.
    let mut incomplete = Vec::new();
    for release in checked {
        if release
            .states
            .iter()
            .any(|(_, state)| matches!(state, JobState::Failed { .. } | JobState::Unfinished))
        {
            continue;
        }
        let unknown: Vec<&str> = release
            .states
            .iter()
            .filter(|(_, state)| matches!(state, JobState::Unknown))
            .map(|(package, _)| package.as_str())
            .collect();
        if !unknown.is_empty() {
            incomplete.push((release.release.name().to_string(), unknown.join(", ")));
        }
    }
    if !incomplete.is_empty() {
        out.milestone(format!(
            "{}",
            style("  Channel evidence is incomplete; falling back to revision history.").yellow()
        ));
        for (release, packages) in incomplete {
            out.milestone(format!(
                "    {release}: Hydra evidence unknown for {packages}"
            ));
        }
        return ChannelOutcome::Unavailable;
    }

    if !unbuilt.is_empty() {
        out.milestone(format!(
            "{}",
            style("  No newer cached revision: required channel builds are not finished yet.")
                .yellow()
        ));
        for (package, detail) in unbuilt {
            out.milestone(format!("    {package}: not built yet ({detail})"));
        }
        out.milestone("    No evaluations were run for this verdict.");
        return ChannelOutcome::Exhausted;
    }

    if succeeded_somewhere == packages.len() {
        out.milestone(format!(
            "{}",
            style(
                "  No channel release confirmed: Hydra-built candidates were not fully cached; falling back to revision history"
            )
            .yellow()
        ));
    }
    ChannelOutcome::Unavailable
}

/// Evaluate one Hydra-confirmed release. `Some(rev)` means every required
/// target is accepted and the revision can be pinned. A target Hydra built
/// whose consumer path differs from every Hydra output and misses every cache
/// is added to `divergent_misses`: only the consumer builds that path.
#[allow(clippy::too_many_arguments)]
async fn confirm<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    release: &ChannelRelease,
    rev: &str,
    eval_id: i64,
    states: &[(String, JobState)],
    divergent_misses: &mut BTreeSet<String>,
) -> Result<Option<String>> {
    let short = &rev[..12.min(rev.len())];
    out.set_action(format!(
        "{}",
        style(format!("Confirming {} at rev {short}...", release.name())).cyan()
    ));

    let hydra_outputs: HashMap<String, String> = states
        .iter()
        .filter_map(|(package, state)| match state {
            JobState::Success {
                out_path: Some(path),
                ..
            } => Some((package.clone(), path.clone())),
            _ => None,
        })
        .collect();

    let check =
        narinfo::verify_required_at_rev_with_hydra(client, cfg, rev, &hydra_outputs, ext).await;
    out.record_probe(cfg, false, check.clone());
    out.milestone(format_rev_summary(rev, Some(eval_id), &check.results));

    let check_kind = if cfg.verify_closure {
        "consumer closure checked"
    } else {
        "consumer narinfo checked"
    };
    let mut matched = 0;
    for result in &check.results {
        let (Some(JobState::Success { outputs, .. }), Some(consumer)) = (
            states
                .iter()
                .find(|(package, _)| package == &result.package)
                .map(|(_, state)| state),
            result.store_path.as_deref(),
        ) else {
            continue;
        };
        if outputs.is_empty() {
            continue;
        }
        if outputs.iter().any(|path| path == consumer) {
            matched += 1;
            continue;
        }
        out.milestone(format!(
            "  {}: consumer-divergent (Hydra {}, consumer {consumer}); {check_kind}",
            result.package,
            outputs.join(", ")
        ));
        if result.availability == narinfo::Availability::Missing {
            divergent_misses.insert(result.package.clone());
        }
    }
    if matched > 0 {
        out.milestone(format!("  {matched} target(s) match Hydra outputs"));
    }

    if check.all_cached {
        out.milestone(format!(
            "{}",
            style(format!("  {} is fully cached", release.name())).green()
        ));
        Ok(Some(rev.to_string()))
    } else {
        out.milestone(format!(
            "{}",
            style(format!(
                "  {} is not fully cached; trying an older channel release",
                release.name()
            ))
            .yellow()
        ));
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn spec() -> ChannelSpec {
        channel_for("github:NixOS/nixpkgs", "nixos-unstable").unwrap()
    }

    #[test]
    fn channel_mapping_covers_channel_families() {
        let unstable = channel_for("github:NixOS/nixpkgs", "nixos-unstable").unwrap();
        assert_eq!(unstable.releases_prefix, "nixos/unstable/nixos-");
        assert_eq!(unstable.hydra_project, "nixos");
        assert_eq!(unstable.hydra_jobset, "unstable");
        assert_eq!(unstable.job_prefix, "nixpkgs.");

        let small = channel_for("github:NixOS/nixpkgs", "nixos-unstable-small").unwrap();
        assert_eq!(small.releases_prefix, "nixos/unstable-small/nixos-");
        assert_eq!(small.hydra_jobset, "unstable-small");

        let release = channel_for("github:NixOS/nixpkgs", "nixos-25.11").unwrap();
        assert_eq!(release.releases_prefix, "nixos/25.11/nixos-");
        assert_eq!(release.hydra_jobset, "release-25.11");

        let small_release = channel_for("github:NixOS/nixpkgs", "nixos-25.11-small").unwrap();
        assert_eq!(small_release.releases_prefix, "nixos/25.11-small/nixos-");
        assert_eq!(small_release.hydra_jobset, "release-25.11-small");

        let nixpkgs = channel_for("github:NixOS/nixpkgs", "nixpkgs-unstable").unwrap();
        assert_eq!(nixpkgs.releases_prefix, "nixpkgs/nixpkgs-");
        assert_eq!(nixpkgs.hydra_project, "nixpkgs");
        assert_eq!(nixpkgs.hydra_jobset, "unstable");
        assert_eq!(nixpkgs.job_prefix, "");

        assert!(channel_for("github:NixOS/nixpkgs", "trunk").is_none());
        assert!(channel_for("github:NixOS/nixpkgs", "nixos-25").is_none());
        assert!(channel_for("github:caniko/nix-cache-pin", "nixos-unstable").is_none());
    }

    #[test]
    fn only_the_public_hydra_answers_for_channel_releases() {
        assert!(hydra::is_public_hydra("https://hydra.nixos.org"));
        assert!(hydra::is_public_hydra("https://HYDRA.nixos.org/"));
        assert!(!hydra::is_public_hydra("https://hydra.lantian.pub"));
        assert!(!hydra::is_public_hydra("http://127.0.0.1:1234"));
    }

    #[test]
    fn listing_parses_and_orders_releases_newest_first() {
        let spec = spec();
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
<IsTruncated>false</IsTruncated>
<Contents><Key>nixos/unstable/nixos-26.11pre999.aaaaaaaaaaaa</Key></Contents>
<Contents><Key>nixos/unstable/nixos-26.11pre1000.bbbbbbbbbbbb</Key></Contents>
<Contents><Key>nixos/unstable/nixos-25.11pre99.cccccccccccc</Key></Contents>
<CommonPrefixes><Prefix>nixos/unstable/nixos-26.11pre1000.bbbbbbbbbbbb/</Prefix></CommonPrefixes>
<Contents><Key>nixos/unstable/nixos-26.11pre1000.bbbbbbbbbbbb</Key></Contents>
</ListBucketResult>"#;
        let page = parse_listing(body, &spec);
        assert_eq!(page.releases.len(), 5);
        assert!(page.next.is_none());
        let ordered = newest_family(page.releases);
        assert_eq!(
            ordered.iter().map(ChannelRelease::name).collect::<Vec<_>>(),
            [
                "nixos-26.11pre1000.bbbbbbbbbbbb",
                "nixos-26.11pre999.aaaaaaaaaaaa"
            ]
        );
    }

    #[test]
    fn listing_sorts_release_numbers_numerically_and_ignores_junk() {
        let spec = spec();
        let body = r#"<ListBucketResult>
<IsTruncated>true</IsTruncated>
<NextContinuationToken>next-page</NextContinuationToken>
<Contents><Key>nixos/unstable/nixos-26.11pre1000.aaaaaaaaaaaa</Key></Contents>
<Contents><Key>nixos/unstable/nixos-26.11pre9999.bbbbbbbbbbbb</Key></Contents>
<Contents><Key>nixos/unstable/src-url</Key></Contents>
<Contents><Key>nixos/unstable/nixos-26.11pre1.nothex</Key></Contents>
</ListBucketResult>"#;
        let page = parse_listing(body, &spec);
        assert_eq!(page.releases.len(), 2);
        assert_eq!(page.next.as_deref(), Some("next-page"));
        assert!(page.truncated);
        assert_eq!(
            newest_family(page.releases)
                .iter()
                .map(ChannelRelease::name)
                .collect::<Vec<_>>(),
            [
                "nixos-26.11pre9999.bbbbbbbbbbbb",
                "nixos-26.11pre1000.aaaaaaaaaaaa"
            ]
        );
    }

    #[tokio::test]
    async fn truncated_listing_without_a_new_token_is_not_a_complete_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>same-token</NextContinuationToken>\
                 <Contents><Key>nixos/unstable/nixos-26.11pre100.aaaaaaaaaaaa</Key></Contents>\
                 </ListBucketResult>",
            ))
            .mount(&server)
            .await;

        let error = list_releases(&Client::new(), &server.uri(), &spec())
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("continuation token"),
            "a repeated continuation token must fail the listing: {error}"
        );
    }

    #[tokio::test]
    async fn unfinished_build_responses_are_refetched() {
        let hydra = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/build/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 7,
                "buildstatus": 0,
                "finished": 0,
                "jobsetevals": []
            })))
            .expect(2)
            .mount(&hydra)
            .await;
        let mut history = JobHistory {
            builds: vec![7],
            details: vec![None],
        };
        let cache = ResponseCache::memory();
        let client = Client::new();

        for eval_id in [100, 101] {
            let state = history
                .state_at(&client, &hydra.uri(), &cache, eval_id)
                .await
                .unwrap();
            assert_eq!(state, JobState::Unknown);
        }
    }

    #[tokio::test]
    async fn only_an_explicit_finished_zero_is_unfinished() {
        let hydra = MockServer::start().await;
        for (id, finished) in [(8, Some(0)), (9, None)] {
            let mut body = serde_json::json!({
                "id": id, "buildstatus": 0, "jobsetevals": [100]
            });
            if let Some(finished) = finished {
                body["finished"] = finished.into();
            }
            Mock::given(method("GET"))
                .and(path(format!("/build/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&hydra)
                .await;
        }
        let (cache, client) = (ResponseCache::memory(), Client::new());
        let state = |id| {
            let mut history = JobHistory {
                builds: vec![id],
                details: vec![None],
            };
            let (cache, client, uri) = (&cache, &client, hydra.uri());
            async move { history.state_at(client, &uri, cache, 100).await.unwrap() }
        };
        assert_eq!(state(8).await, JobState::Unfinished);
        // A record without `finished` proves nothing: unknown, so it falls back.
        assert_eq!(state(9).await, JobState::Unknown);
    }

    #[test]
    fn eval_urls_parse_from_hydra_links_and_bare_numbers() {
        assert_eq!(
            parse_eval_url("https://hydra.nixos.org/eval/1831300\n"),
            Some(1831300)
        );
        assert_eq!(parse_eval_url("1831300"), Some(1831300));
        assert_eq!(parse_eval_url("https://hydra.nixos.org/build/1"), None);
    }

    #[test]
    fn job_names_follow_the_channel_jobset() {
        let cfg = PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": ["hello"],
                "inputName": "nixpkgs",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": [],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 5,
                "branch": "nixos-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();
        assert_eq!(
            channel_job(&cfg, &spec(), "hello"),
            "nixpkgs.hello.x86_64-linux"
        );
        let nixpkgs = channel_for("github:NixOS/nixpkgs", "nixpkgs-unstable").unwrap();
        assert_eq!(channel_job(&cfg, &nixpkgs, "hello"), "hello.x86_64-linux");
    }

    /// The pin that took 930 s: `attrPrefix = "pkgs"` made every job name
    /// `nixpkgs.pkgs.<pkg>.x86_64-linux`, a job the channel jobset never had.
    #[test]
    fn job_names_drop_the_legacy_pkgs_component() {
        let mut cfg = PinConfig::from_json(
            r#"{
                "name": "nixpkgs",
                "packages": ["windmill", "cosmic-app-library"],
                "inputName": "nixpkgs",
                "attrPrefix": "pkgs",
                "pythonPackages": null,
                "caches": [],
                "hydraJobset": "nixos/unstable",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixos-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();

        assert_eq!(
            channel_job(&cfg, &spec(), "windmill"),
            "nixpkgs.windmill.x86_64-linux"
        );
        assert_eq!(
            channel_job(&cfg, &spec(), "cosmic-app-library"),
            "nixpkgs.cosmic-app-library.x86_64-linux"
        );
        let nixpkgs = channel_for("github:NixOS/nixpkgs", "nixpkgs-unstable").unwrap();
        assert_eq!(
            channel_job(&cfg, &nixpkgs, "windmill"),
            "windmill.x86_64-linux"
        );

        // Other prefixes stay: the channel jobset may not have those jobs, and
        // a made-up name would hide that behind different evidence.
        for (prefix, job) in [
            ("pkgsCuda", "nixpkgs.pkgsCuda.hello.x86_64-linux"),
            ("pkgs.foo", "nixpkgs.foo.hello.x86_64-linux"),
            (
                "python312Packages",
                "nixpkgs.python312Packages.hello.x86_64-linux",
            ),
            ("", "nixpkgs.hello.x86_64-linux"),
        ] {
            cfg.attr_prefix = prefix.to_string();
            assert_eq!(channel_job(&cfg, &spec(), "hello"), job, "{prefix}");
        }
    }
}
