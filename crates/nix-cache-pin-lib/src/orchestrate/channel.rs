//! Channel release candidates backed by the public release bucket and Hydra.
//!
//! A channel branch such as `nixos-unstable` publishes one release directory
//! per successful Hydra evaluation. Those releases are the candidate set: the
//! bucket orders them by release number, and Hydra's build history says which
//! of them have every required build finished. Only a release that passes that
//! evidence gate is evaluated; walking individual commits (the fallback) costs
//! a full consumer evaluation for revisions Hydra never built.

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
use std::path::PathBuf;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobState {
    Success { out_path: Option<String> },
    Failed { build: i64 },
    Unfinished,
    NotBuilt,
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
    package: String,
    builds: Vec<i64>,
    details: Vec<Option<HydraBuildDetail>>,
}

impl JobHistory {
    async fn load(
        client: &Client,
        cfg: &PinConfig,
        spec: &ChannelSpec,
        package: &str,
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
            package: package.to_string(),
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
        for (index, build_id) in self.builds.iter().enumerate() {
            if self.details[index].is_none() {
                let url = format!("{}/build/{build_id}", hydra_url.trim_end_matches('/'));
                let body = cache.text(client, &url).await?;
                let detail: HydraBuildDetail = serde_json::from_str(&body).map_err(|error| {
                    Error::Config(format!("invalid Hydra build {build_id}: {error}"))
                })?;
                self.details[index] = Some(detail);
            }
            let detail = self.details[index]
                .as_ref()
                .expect("detail was just fetched or already present");
            if detail.jobsetevals.contains(&eval_id) {
                if detail.finished != Some(1) {
                    return Ok(JobState::Unfinished);
                }
                if detail.buildstatus == 0 {
                    return Ok(JobState::Success {
                        out_path: detail.out_path().map(str::to_string),
                    });
                }
                return Ok(JobState::Failed { build: detail.id });
            }
        }
        Ok(JobState::NotBuilt)
    }
}

/// Run-scoped response cache. Finished Hydra builds and release `src-url`
/// files never change, so they are cached on disk for the run; listings and
/// job histories stay fresh.
struct ResponseCache {
    dir: Option<PathBuf>,
}

impl ResponseCache {
    fn run_scoped() -> Self {
        Self {
            dir: Some(std::env::temp_dir().join("nix-cache-pin-hydra-cache")),
        }
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self { dir: None }
    }

    fn path(&self, url: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(cache_key(url)))
    }

    fn read(&self, url: &str) -> Option<String> {
        std::fs::read_to_string(self.path(url)?).ok()
    }

    fn write(&self, url: &str, body: &str) {
        let Some(path) = self.path(url) else {
            return;
        };
        if body.len() > MAX_CACHED_BYTES {
            return;
        }
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        let _ = std::fs::write(path, body);
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

fn cache_key(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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
        match page.next {
            Some(next) if seen_pages.insert(next.clone()) => token = Some(next),
            _ => break,
        }
    }
    Ok(newest_family(releases))
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
    let next = tag_values(body, "NextContinuationToken")
        .into_iter()
        .next()
        .filter(|_| body.contains("<IsTruncated>true</IsTruncated>"));
    ListingPage { releases, next }
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
        crate::narinfo::build_attr(cfg.full_attr_prefix(), package),
        cfg.arch
    )
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

/// Keep the releases that are newer than the current pin.
///
/// Channel releases advance linearly, so checking the newest and oldest of the
/// bounded window is enough in the common case. A pin inside the window is
/// resolved from the oldest end.
async fn newer_than_pin<E: ExternalCommands + 'static>(
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
    repo: &str,
    releases: &[ChannelRelease],
) -> Result<Vec<ChannelRelease>> {
    let Some(newest) = releases.first() else {
        return Ok(Vec::new());
    };
    let newest_rev = match resolve_rev(ext, repo, &cfg.branch, newest).await {
        Ok(rev) => rev,
        Err(error) => {
            out.milestone(format!(
                "{}",
                style(format!(
                    "  Could not resolve the newest channel release {}: {error}",
                    newest.name()
                ))
                .yellow()
            ));
            return Ok(Vec::new());
        }
    };
    if !super::candidate_is_allowed(cfg, current_rev, &newest_rev, out, ext).await? {
        return Ok(Vec::new());
    }

    let oldest = &releases[releases.len() - 1];
    if releases.len() > 1 {
        let oldest_is_newer = match resolve_rev(ext, repo, &cfg.branch, oldest).await {
            Ok(rev) => super::candidate_is_allowed(cfg, current_rev, &rev, out, ext).await?,
            Err(_) => return Ok(vec![newest.clone()]),
        };
        if oldest_is_newer {
            return Ok(releases.to_vec());
        }
    } else {
        return Ok(vec![newest.clone()]);
    }

    let mut count = 1;
    for index in (1..releases.len()).rev() {
        let Ok(rev) = resolve_rev(ext, repo, &cfg.branch, &releases[index]).await else {
            continue;
        };
        if super::candidate_is_allowed(cfg, current_rev, &rev, out, ext).await? {
            count = index + 1;
            break;
        }
    }
    Ok(releases[..count].to_vec())
}

/// Channel releases are published by the public Hydra; a different instance
/// cannot answer for their evaluation ids.
fn is_public_hydra(hydra_url: &str) -> bool {
    reqwest::Url::parse(hydra_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "hydra.nixos.org")
}

/// Search channel releases for the newest fully cached revision.
pub(super) async fn search<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    current_rev: Option<&str>,
) -> Result<ChannelOutcome> {
    if !is_public_hydra(&cfg.hydra_url) {
        return Ok(ChannelOutcome::Unavailable);
    }
    let cache = ResponseCache::run_scoped();
    search_at(client, cfg, out, ext, current_rev, RELEASES_URL, &cache).await
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
    search_at(
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

async fn search_at<E: ExternalCommands + 'static>(
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

    let window = newer_than_pin(cfg, out, ext, current_rev, repo, releases).await?;
    if window.is_empty() {
        out.milestone(format!(
            "{}",
            style(format!(
                "  No {} channel release is newer than the current pin.",
                cfg.branch
            ))
            .dim()
        ));
        return Ok(ChannelOutcome::Exhausted);
    }

    let mut jobs = Vec::with_capacity(cfg.packages.len());
    for package in &cfg.packages {
        let job = channel_job(cfg, &spec, package);
        match JobHistory::load(client, cfg, &spec, package, job.clone()).await {
            Ok(history) if history.builds.is_empty() => {
                out.milestone(format!(
                    "{}",
                    style(format!(
                        "  No Hydra build history for {package} as {job}; falling back to revision history"
                    ))
                    .yellow()
                ));
                return Ok(ChannelOutcome::Unavailable);
            }
            Ok(history) => jobs.push(history),
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
        }
    }

    let mut checked = Vec::with_capacity(window.len());
    for release in &window {
        let eval_id = match release_eval(client, releases_url, cache, release).await {
            Ok(eval_id) => eval_id,
            Err(error) => {
                out.milestone(format!(
                    "{}",
                    style(format!("  Skipping {}: {error}", release.name())).yellow()
                ));
                continue;
            }
        };

        let mut states = Vec::with_capacity(jobs.len());
        let mut all_success = true;
        for job in &mut jobs {
            match job.state_at(client, &cfg.hydra_url, cache, eval_id).await {
                Ok(state) => {
                    all_success &= state.is_success();
                    states.push((job.package.clone(), state));
                }
                Err(error) => {
                    out.milestone(format!(
                        "{}",
                        style(format!(
                            "  Hydra evidence unavailable for {}: {error}; falling back to revision history",
                            job.package
                        ))
                        .yellow()
                    ));
                    return Ok(ChannelOutcome::Unavailable);
                }
            }
        }

        out.milestone(format_release_status(release, eval_id, &states));
        if !all_success {
            checked.push(CheckedRelease {
                release: release.clone(),
                states,
            });
            continue;
        }

        match confirm(client, cfg, out, ext, repo, release, eval_id, &states).await? {
            Some(rev) => return Ok(ChannelOutcome::Found(rev)),
            None => checked.push(CheckedRelease {
                release: release.clone(),
                states,
            }),
        }
    }

    if checked.is_empty() {
        out.milestone(format!(
            "{}",
            style(
                "  No channel release had usable Hydra evidence; falling back to revision history"
            )
            .yellow()
        ));
        return Ok(ChannelOutcome::Unavailable);
    }
    Ok(no_candidate(cfg, out, &jobs, &checked))
}

fn format_release_status(
    release: &ChannelRelease,
    eval_id: i64,
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
    format!("  {} (eval {eval_id}): {detail}", release.name())
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::Success { .. } => "built",
        JobState::Failed { .. } => "failed",
        JobState::Unfinished | JobState::NotBuilt => "not built yet",
    }
}

/// Report why no channel release qualified, without evaluating anything.
fn no_candidate(
    cfg: &PinConfig,
    out: &mut Output,
    jobs: &[JobHistory],
    checked: &[CheckedRelease],
) -> ChannelOutcome {
    if jobs.is_empty() {
        out.milestone(format!(
            "{}",
            style(
                "  No channel release could be confirmed without producer build evidence; falling back to revision history"
            )
            .yellow()
        ));
        return ChannelOutcome::Unavailable;
    }

    let mut failed_everywhere = Vec::new();
    let mut unbuilt = Vec::new();
    let mut succeeded_somewhere = 0;

    for job in jobs {
        let mut failures = BTreeSet::new();
        let mut unfinished = Vec::new();
        let mut success = false;
        let mut not_built = 0;
        for release in checked {
            match release.state(&job.package) {
                Some(JobState::Success { .. }) => success = true,
                Some(JobState::Failed { build }) => {
                    failures.insert(*build);
                }
                Some(JobState::Unfinished) => unfinished.push(release.release.name()),
                Some(JobState::NotBuilt) | None => not_built += 1,
            }
        }
        if success {
            succeeded_somewhere += 1;
            continue;
        }
        let newest = checked
            .first()
            .map(|release| release.release.name())
            .unwrap_or("unknown");
        let oldest = checked
            .last()
            .map(|release| release.release.name())
            .unwrap_or("unknown");
        if !failures.is_empty() && unfinished.is_empty() {
            failed_everywhere.push((job.package.clone(), failures, newest, oldest));
        } else if !failures.is_empty() {
            unbuilt.push((
                job.package.clone(),
                format!(
                    "last failure {}/build/{}; {} newer release(s) not built yet",
                    cfg.hydra_url.trim_end_matches('/'),
                    failures.iter().next().expect("failures is non-empty"),
                    unfinished.len() + not_built
                ),
            ));
        } else {
            unbuilt.push((
                job.package.clone(),
                format!("{} release(s) not built yet", unfinished.len() + not_built),
            ));
        }
    }

    if !failed_everywhere.is_empty() {
        out.milestone(format!(
            "{}",
            style("  No newer cached revision: channel builds failed for required packages.").red()
        ));
        for (package, builds, newest, oldest) in failed_everywhere {
            let urls = builds
                .iter()
                .map(|build| format!("{}/build/{build}", cfg.hydra_url.trim_end_matches('/')))
                .collect::<Vec<_>>()
                .join(", ");
            out.milestone(format!(
                "    {package}: failed at every channel release checked ({urls}; covering {newest} … {oldest})"
            ));
        }
        out.milestone("    No evaluations were run for this verdict.");
        return ChannelOutcome::Exhausted;
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

    if succeeded_somewhere == jobs.len() {
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
/// target is accepted and the revision can be pinned.
#[allow(clippy::too_many_arguments)]
async fn confirm<E: ExternalCommands + 'static>(
    client: &Client,
    cfg: &PinConfig,
    out: &mut Output,
    ext: &Arc<E>,
    repo: &str,
    release: &ChannelRelease,
    eval_id: i64,
    states: &[(String, JobState)],
) -> Result<Option<String>> {
    let rev = match resolve_rev(ext, repo, &cfg.branch, release).await {
        Ok(rev) => rev,
        Err(error) => {
            out.milestone(format!(
                "{}",
                style(format!(
                    "  Could not resolve {}: {error}; trying an older release",
                    release.name()
                ))
                .yellow()
            ));
            return Ok(None);
        }
    };
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
        narinfo::verify_required_at_rev_with_hydra(client, cfg, &rev, &hydra_outputs, ext).await;
    out.record_probe(cfg, false, check.clone());
    out.milestone(format_rev_summary(&rev, Some(eval_id), &check.results));

    let mut matched = 0;
    let mut divergent = Vec::new();
    for result in &check.results {
        let Some(hydra) = hydra_outputs.get(&result.package) else {
            continue;
        };
        match result.store_path.as_deref() {
            Some(consumer) if consumer == hydra => matched += 1,
            Some(consumer) => divergent.push((result.package.as_str(), hydra.as_str(), consumer)),
            None => {}
        }
    }
    if matched > 0 {
        out.milestone(format!(
            "  {matched} target(s) match Hydra outputs; root narinfo only, closure walks skipped"
        ));
    }
    for (package, hydra, consumer) in divergent {
        let check_kind = if cfg.verify_closure {
            "consumer closure checked"
        } else {
            "consumer narinfo checked"
        };
        out.milestone(format!(
            "  {package}: consumer-divergent (Hydra {hydra}, consumer {consumer}); {check_kind}"
        ));
    }

    if check.all_cached {
        out.milestone(format!(
            "{}",
            style(format!("  {} is fully cached", release.name())).green()
        ));
        Ok(Some(rev))
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
        assert!(is_public_hydra("https://hydra.nixos.org"));
        assert!(is_public_hydra("https://HYDRA.nixos.org/"));
        assert!(!is_public_hydra("https://hydra.lantian.pub"));
        assert!(!is_public_hydra("http://127.0.0.1:1234"));
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
}
