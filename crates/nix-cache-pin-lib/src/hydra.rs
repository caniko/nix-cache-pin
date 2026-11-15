use crate::config::PinConfig;
use crate::error::{Error, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

/// Hydra publishes two requests per second with a burst of seven. One request
/// per second keeps every task and retry below that.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

const MAX_REQUEST_ATTEMPTS: usize = 6;

/// Total time one request may spend retrying, pacing included.
const REQUEST_BUDGET: Duration = Duration::from_secs(180);

/// Longest single pause, whatever Retry-After asks for.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// First pause without a Retry-After; doubles per attempt.
const RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Space request starts: every reservation claims the next free slot, so the
/// process never runs two Hydra requests closer than the interval, and a
/// retry waits its turn like any other request.
struct Limiter {
    interval: Duration,
    next: Mutex<tokio::time::Instant>,
}

impl Limiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(tokio::time::Instant::now()),
        }
    }

    /// Claim the next start slot, wait for it, and return the wait. With a
    /// deadline, a slot that would start after it is not claimed (`None`).
    async fn wait(&self, deadline: Option<tokio::time::Instant>) -> Option<Duration> {
        let wait = {
            let mut next = self.next.lock().await;
            let now = tokio::time::Instant::now();
            // Never move the slot backwards: a late request does not hand its
            // slack to the next one.
            let slot = now.max(*next);
            if deadline.is_some_and(|deadline| slot > deadline) {
                return None;
            }
            *next = slot + self.interval;
            slot - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Some(wait)
    }
}

static LIMITER: OnceLock<Limiter> = OnceLock::new();

fn limiter() -> &'static Limiter {
    LIMITER.get_or_init(|| Limiter::new(request_interval()))
}

/// Test builds drop the production interval so the suite stays fast; the
/// limiter test drives its own instance at the production interval.
fn request_interval() -> Duration {
    if cfg!(test) {
        Duration::ZERO
    } else {
        MIN_REQUEST_INTERVAL
    }
}

/// Hydra traffic of this process. Every HTTP attempt is counted where it is
/// sent, retries, redirect hops and failed sends included, so concurrent
/// searches and their spawned tasks all add up.
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static THROTTLED: AtomicU64 = AtomicU64::new(0);
static WAITED_MICROS: AtomicU64 = AtomicU64::new(0);

/// Hydra traffic since the process started, for a consumer's run report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HydraTraffic {
    pub requests: u64,
    pub throttled: u64,
    pub waited_ms: u64,
}

#[must_use]
pub fn traffic() -> HydraTraffic {
    HydraTraffic {
        requests: REQUESTS.load(Ordering::Relaxed),
        throttled: THROTTLED.load(Ordering::Relaxed),
        waited_ms: WAITED_MICROS.load(Ordering::Relaxed) / 1000,
    }
}

fn record_wait(waited: Duration) {
    let micros = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
    WAITED_MICROS.fetch_add(micros, Ordering::Relaxed);
}

/// Retry explicit throttling/unavailability, paced at one request per second
/// and bounded in both attempts and total time. Every attempt, retries
/// included, reserves a slot on the limiter.
async fn get(client: &Client, url: &str) -> reqwest::Result<reqwest::Response> {
    let deadline = tokio::time::Instant::now() + REQUEST_BUDGET;
    // `/latest-finished` answers with a redirect to its build, a second Hydra
    // request the client follows on its own: reserve its slot up front.
    let slots = if url.ends_with("/latest-finished") {
        2
    } else {
        1
    };
    let paced = paced(url);
    let mut last = None;
    for attempt in 0..MAX_REQUEST_ATTEMPTS {
        if paced {
            // The first attempt queues like any request; a retry runs only in
            // a slot that starts before the request's deadline.
            let deadline = (attempt > 0).then_some(deadline);
            for _ in 0..slots {
                match limiter().wait(deadline).await {
                    Some(waited) => record_wait(waited),
                    None => {
                        return Ok(last.expect("only a retry has a deadline"));
                    }
                }
            }
        }
        // Hydra traffic only: the release bucket is neither paced nor counted.
        if paced {
            REQUESTS.fetch_add(1, Ordering::Relaxed);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let response = client
            .get(url)
            .header("Accept", "application/json")
            .header(
                reqwest::header::USER_AGENT,
                concat!("nix-cache-pin/", env!("CARGO_PKG_VERSION")),
            )
            // A retry slot can start right at the deadline; give it a second.
            .timeout(remaining.clamp(Duration::from_secs(1), Duration::from_secs(30)))
            .send()
            .await?;
        // A followed redirect was one more request.
        if paced && redirected(url, &response) {
            REQUESTS.fetch_add(1, Ordering::Relaxed);
        }
        if !matches!(
            response.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
        ) {
            return Ok(response);
        }
        if paced && response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            THROTTLED.fetch_add(1, Ordering::Relaxed);
        }
        if attempt + 1 == MAX_REQUEST_ATTEMPTS {
            return Ok(response);
        }
        let delay = retry_delay(response.headers(), SystemTime::now(), attempt);
        if tokio::time::Instant::now() + delay >= deadline {
            return Ok(response);
        }
        if paced {
            record_wait(delay);
        }
        last = Some(response);
        tokio::time::sleep(delay).await;
    }
    unreachable!("bounded attempts return their final response")
}

/// Whether the client followed a redirect: the final URL differs from the
/// requested one once both are parsed, so normalization alone (case, a default
/// port, dot segments) is not a hop.
fn redirected(requested: &str, response: &reqwest::Response) -> bool {
    reqwest::Url::parse(requested).ok().as_ref() != Some(response.url())
}

/// Hydra rate-limits; the S3 bucket that publishes channel releases does not,
/// so its listing pages and `src-url` files skip the limiter.
fn paced(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_none_or(|host| !host.eq_ignore_ascii_case("nix-releases.s3.amazonaws.com"))
}

/// Pause before the next attempt: Retry-After when Hydra sends one, an
/// exponential backoff otherwise, never above `MAX_RETRY_WAIT`.
fn retry_delay(headers: &reqwest::header::HeaderMap, now: SystemTime, attempt: usize) -> Duration {
    retry_after(headers, now)
        .unwrap_or_else(|| RETRY_BACKOFF * (1u32 << attempt))
        .min(MAX_RETRY_WAIT)
}

fn retry_after(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(now).unwrap_or_default())
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HydraStatus {
    OnHydra,
    NotOnHydra,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct HydraBuildResult {
    pub package: String,
    pub evals: Vec<i64>,
    pub status: HydraStatus,
    pub store_path: Option<String>,
    pub error: Option<String>,
    pub build_status: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HydraBuild {
    jobsetevals: Option<Vec<i64>>,
    buildoutputs: Option<std::collections::HashMap<String, BuildOutput>>,
    buildstatus: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BuildOutput {
    path: Option<String>,
}

/// One entry of `latestbuilds`: only the build id is needed to fetch details.
#[derive(Debug, Deserialize)]
struct HydraLatestBuild {
    id: i64,
}

/// A build record with the evaluation membership and output needed to gate a
/// channel release. A finished build and its output never change; its
/// `jobsetevals` only grows as later evaluations reuse it.
#[derive(Debug, Clone, Deserialize)]
pub struct HydraBuildDetail {
    pub id: i64,
    pub buildstatus: i64,
    #[serde(default)]
    pub finished: Option<i64>,
    #[serde(default)]
    pub jobsetevals: Vec<i64>,
    #[serde(default)]
    pub buildoutputs: Option<std::collections::HashMap<String, HydraBuildOutput>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HydraBuildOutput {
    #[serde(default)]
    pub path: Option<String>,
}

impl HydraBuildDetail {
    #[must_use]
    pub fn out_path(&self) -> Option<&str> {
        self.buildoutputs.as_ref()?.get("out")?.path.as_deref()
    }
}

/// Whether a Hydra URL is the public instance that publishes channel builds.
#[must_use]
pub(crate) fn is_public_hydra(hydra_url: &str) -> bool {
    reqwest::Url::parse(hydra_url).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("hydra.nixos.org"))
    })
}

/// Fetch the ids of a job's most recent builds, newest first. An unknown job is
/// an empty history rather than an error.
pub async fn query_latest_builds(
    client: &Client,
    hydra_url: &str,
    project: &str,
    jobset: &str,
    job: &str,
    nr: usize,
) -> Result<Vec<i64>> {
    let mut url = reqwest::Url::parse(hydra_url)
        .map_err(|error| Error::Config(format!("invalid Hydra URL {hydra_url}: {error}")))?;
    url.set_path(&format!(
        "{}/api/latestbuilds",
        url.path().trim_end_matches('/')
    ));
    url.query_pairs_mut()
        .append_pair("nr", &nr.to_string())
        .append_pair("project", project)
        .append_pair("jobset", jobset)
        .append_pair("job", job);

    let response = get(client, url.as_str()).await.map_err(Error::Http)?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    let response = response.error_for_status().map_err(Error::Http)?;
    let builds = response
        .json::<Vec<HydraLatestBuild>>()
        .await
        .map_err(Error::Http)?;
    Ok(builds.into_iter().map(|build| build.id).collect())
}

/// Fetch a text response through the shared Hydra retry and rate-limit policy.
pub(crate) async fn get_text(client: &Client, url: &str) -> Result<String> {
    get(client, url)
        .await
        .map_err(Error::Http)?
        .error_for_status()
        .map_err(Error::Http)?
        .text()
        .await
        .map_err(Error::Http)
}

#[derive(Debug, Clone, Deserialize)]
pub struct HydraEval {
    pub id: i64,
    pub flake: Option<String>,
    pub jobsetevalinputs: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct HydraEvalsResponse {
    evals: Vec<HydraEval>,
    next: Option<String>,
}

/// Build the Hydra job URL for a given package.
#[must_use]
pub fn build_hydra_job_url(cfg: &PinConfig, pkg: &str) -> String {
    let pattern = cfg
        .hydra_job_pattern
        .replace("{jobset}", &cfg.hydra_jobset)
        .replace("{fullAttrPrefix}", cfg.full_attr_prefix())
        .replace("{attrPrefix}", &cfg.attr_prefix)
        .replace("{arch}", &cfg.arch)
        .replace("{pkg}", pkg);
    format!("{}/job/{}/latest-finished", cfg.hydra_url, pattern)
}

/// Query Hydra for the latest-finished build of a package.
pub async fn query_hydra_build(client: &Client, cfg: &PinConfig, pkg: &str) -> HydraBuildResult {
    let job_url = build_hydra_job_url(cfg, pkg);
    let result = get(client, &job_url).await;

    match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => HydraBuildResult {
            package: pkg.to_string(),
            evals: vec![],
            status: HydraStatus::NotOnHydra,
            store_path: None,
            error: None,
            build_status: None,
        },
        Ok(resp) => match resp.error_for_status() {
            Ok(resp) => match resp.json::<HydraBuild>().await {
                Ok(build) => {
                    let store_path = build
                        .buildoutputs
                        .as_ref()
                        .and_then(|o| o.get("out"))
                        .and_then(|o| o.path.clone());
                    HydraBuildResult {
                        package: pkg.to_string(),
                        evals: {
                            let mut evals = build.jobsetevals.unwrap_or_default();
                            evals.sort_unstable_by(|a, b| b.cmp(a));
                            evals.dedup();
                            evals
                        },
                        status: HydraStatus::OnHydra,
                        store_path,
                        error: None,
                        build_status: build.buildstatus,
                    }
                }
                Err(error) => HydraBuildResult {
                    package: pkg.to_string(),
                    evals: vec![],
                    status: HydraStatus::Unknown,
                    store_path: None,
                    error: Some(error.without_url().to_string()),
                    build_status: None,
                },
            },
            Err(error) => HydraBuildResult {
                package: pkg.to_string(),
                evals: vec![],
                status: HydraStatus::Unknown,
                store_path: None,
                error: Some(error.without_url().to_string()),
                build_status: None,
            },
        },
        Err(error) => HydraBuildResult {
            package: pkg.to_string(),
            evals: vec![],
            status: HydraStatus::Unknown,
            store_path: None,
            error: Some(error.without_url().to_string()),
            build_status: None,
        },
    }
}

/// Query the Hydra jobset eval list.
pub async fn query_hydra_jobset_evals(
    client: &Client,
    cfg: &PinConfig,
    limit: usize,
    out: &mut crate::output::Output,
) -> Vec<HydraEval> {
    let parts: Vec<&str> = cfg.hydra_jobset.splitn(2, '/').collect();
    if parts.len() < 2 {
        return vec![];
    }
    let (project, jobset) = (parts[0], parts[1]);
    let url = format!(
        "{}/jobset/{}/{}/evals?limit={}",
        cfg.hydra_url, project, jobset, limit
    );

    let mut next = Some(url);
    let mut seen_pages = std::collections::HashSet::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut evals = Vec::new();
    // Bound requests even if a backend repeats records or provides empty pages.
    for _ in 0..limit {
        let Some(url) = next.take() else { break };
        if !seen_pages.insert(url.clone()) {
            break;
        }
        let response = async {
            get(client, &url)
                .await?
                .error_for_status()?
                .json::<HydraEvalsResponse>()
                .await
        }
        .await;
        match response {
            Ok(page) => {
                for eval in page.evals {
                    if seen_ids.insert(eval.id) {
                        evals.push(eval);
                    }
                }
                if evals.len() >= limit {
                    break;
                }
                // Hydra's next link is query-relative. Do not follow arbitrary hosts.
                next = page
                    .next
                    .filter(|s| s.starts_with('?'))
                    .map(|query| format!("{}{query}", url.split('?').next().unwrap_or(&url)));
            }
            Err(error) => {
                out.milestone(format!(
                    "  Hydra history backend unknown: {}",
                    error.without_url()
                ));
                break;
            }
        }
    }
    evals.sort_unstable_by_key(|e| std::cmp::Reverse(e.id));
    evals.truncate(limit);
    evals
}

/// Fetch eval metadata from Hydra.
///
/// Deserialize the complete record so named inputs are never silently replaced
/// by a producer flake revision from a truncated JSON prefix.
pub async fn fetch_eval(client: &Client, hydra_url: &str, eval_id: i64) -> Result<HydraEval> {
    let url = format!("{hydra_url}/eval/{eval_id}");

    // Named input revisions must not be replaced by the producer flake revision.
    full_fetch_eval(client, &url).await
}

/// Full-fetch fallback: downloads the entire eval response and deserialises
/// it with serde.  Populates `jobsetevalinputs` so that `extract_eval_rev`
/// can look up a named flake input revision.
async fn full_fetch_eval(client: &Client, url: &str) -> Result<HydraEval> {
    let resp = get(client, url)
        .await
        .map_err(|error| Error::Http(error.without_url()))?
        .error_for_status()
        .map_err(|error| Error::Http(error.without_url()))?;

    resp.json::<HydraEval>()
        .await
        .map_err(|error| Error::Http(error.without_url()))
}

/// Extract revision from a Hydra evaluation.
#[must_use]
pub fn extract_eval_rev(cfg: &PinConfig, eval: &HydraEval) -> Option<String> {
    if cfg.hydra_rev_input == "flake" {
        let flake = eval.flake.as_deref()?;
        let re = regex::Regex::new(r"(?P<rev>[0-9a-f]{40})").ok()?;
        return re.captures(flake).map(|c| c["rev"].to_string());
    }
    // Look up named input in jobsetevalinputs
    let inputs = eval.jobsetevalinputs.as_ref()?;
    inputs
        .get(&cfg.hydra_rev_input)?
        .get("revision")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> PinConfig {
        PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": ["hello"],
                "inputName": "nixpkgs",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": ["https://cache.nixos.org"],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn test_build_hydra_job_url() {
        let cfg = test_cfg();
        let url = build_hydra_job_url(&cfg, "blender");
        assert_eq!(
            url,
            "https://hydra.nixos.org/job/nixpkgs/trunk/blender.x86_64-linux/latest-finished"
        );
    }

    #[test]
    fn test_build_hydra_job_url_full_prefix() {
        let cfg = PinConfig::from_json(
            r#"{
                "name": "rocm",
                "packages": ["rocblas"],
                "inputName": "nixpkgs-rocm",
                "attrPrefix": "pkgsRocm",
                "pythonPackages": null,
                "caches": ["https://cache.nixos.org"],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{fullAttrPrefix}.{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();
        let url = build_hydra_job_url(&cfg, "rocblas");
        assert_eq!(
            url,
            "https://hydra.nixos.org/job/nixpkgs/trunk/pkgsRocm.rocblas.x86_64-linux/latest-finished"
        );
    }

    #[test]
    fn test_extract_eval_rev_flake() {
        let cfg = PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": [],
                "inputName": "test",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": [],
                "hydraJobset": "lantian/nix-cachyos-kernel",
                "hydraUrl": "https://hydra.lantian.pub",
                "hydraJobPattern": "{jobset}/packages.{arch}.{pkg}",
                "hydraRevInput": "flake",
                "depth": 15,
                "branch": "main",
                "flakeRef": "github:xddxdd/nix-cachyos-kernel",
                "flakeOutput": "packages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();

        let eval = HydraEval {
            id: 1,
            flake: Some("github:xddxdd/nix-cachyos-kernel/1fc1e3f6d65a3e16898c8a75a951cfc529e71001?narHash=sha256-abc".into()),
            jobsetevalinputs: None,
        };
        assert_eq!(
            extract_eval_rev(&cfg, &eval),
            Some("1fc1e3f6d65a3e16898c8a75a951cfc529e71001".into())
        );
    }

    #[test]
    fn test_extract_eval_rev_named_input() {
        let cfg = test_cfg();
        let eval = HydraEval {
            id: 1,
            flake: None,
            jobsetevalinputs: Some(serde_json::json!({
                "nixpkgs": {
                    "revision": "abcdef1234567890abcdef1234567890abcdef12"
                }
            })),
        };
        assert_eq!(
            extract_eval_rev(&cfg, &eval),
            Some("abcdef1234567890abcdef1234567890abcdef12".into())
        );
    }

    #[test]
    fn test_extract_eval_rev_named_input_missing() {
        // When the named input exists but has no "revision" field
        let cfg = test_cfg();
        let eval = HydraEval {
            id: 1,
            flake: None,
            jobsetevalinputs: Some(serde_json::json!({
                "nixpkgs": { "uri": "https://github.com/NixOS/nixpkgs" }
            })),
        };
        assert_eq!(extract_eval_rev(&cfg, &eval), None);
    }

    #[test]
    fn missing_named_input_never_uses_producer_revision() {
        let eval = HydraEval {
            id: 1,
            flake: Some("github:producer/project/1111111111111111111111111111111111111111".into()),
            jobsetevalinputs: None,
        };
        assert_eq!(extract_eval_rev(&test_cfg(), &eval), None);
    }

    #[test]
    fn test_extract_eval_rev_missing() {
        let cfg = PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": [],
                "inputName": "test",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": [],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nonexistent",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();
        let eval = HydraEval {
            id: 1,
            flake: None,
            jobsetevalinputs: Some(serde_json::json!({
                "nixpkgs": { "revision": "abc" }
            })),
        };
        assert_eq!(extract_eval_rev(&cfg, &eval), None);
    }

    // --- HTTP integration tests with wiremock ---

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn retry_after_accepts_seconds_and_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(
            retry_after(&headers, SystemTime::UNIX_EPOCH),
            Some(Duration::from_secs(5))
        );
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Thu, 01 Jan 1970 00:00:10 GMT".parse().unwrap(),
        );
        assert_eq!(
            retry_after(&headers, SystemTime::UNIX_EPOCH),
            Some(Duration::from_secs(10))
        );
        headers.insert(reqwest::header::RETRY_AFTER, "invalid".parse().unwrap());
        assert_eq!(retry_after(&headers, SystemTime::UNIX_EPOCH), None);
    }

    #[test]
    fn retry_waits_honour_retry_after_and_stay_within_the_cap() {
        let mut headers = reqwest::header::HeaderMap::new();
        // Without a header the wait doubles from two seconds.
        assert_eq!(
            retry_delay(&headers, SystemTime::UNIX_EPOCH, 0),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_delay(&headers, SystemTime::UNIX_EPOCH, 2),
            Duration::from_secs(8)
        );
        headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(
            retry_delay(&headers, SystemTime::UNIX_EPOCH, 0),
            Duration::from_secs(5)
        );
        // An hour-long Retry-After is honoured, but only for one bounded wait.
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(
            retry_delay(&headers, SystemTime::UNIX_EPOCH, 0),
            MAX_RETRY_WAIT
        );
    }

    #[test]
    fn only_hydra_requests_are_paced() {
        assert!(paced("https://hydra.nixos.org/build/1"));
        assert!(!paced(
            "https://nix-releases.s3.amazonaws.com/?prefix=nixos/unstable/"
        ));
        assert!(!paced(
            "https://NIX-RELEASES.s3.amazonaws.com/nixos/unstable/x/src-url"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_spaces_requests_a_second_apart() {
        let limiter = Limiter::new(MIN_REQUEST_INTERVAL);
        // The first request starts at once; the next one waits for its slot.
        assert_eq!(limiter.wait(None).await, Some(Duration::ZERO));
        let started = tokio::time::Instant::now();
        let waited = limiter.wait(None).await.unwrap();
        assert!(
            waited >= Duration::from_millis(900),
            "the second request waited only {waited:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "two sequential requests took only {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_never_claims_a_slot_past_its_deadline() {
        let limiter = Limiter::new(MIN_REQUEST_INTERVAL);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        // Concurrent requests queue a second apart: slots at 0 s and 1 s fit,
        // the one at 2 s would start after the deadline and is not claimed.
        let (first, second, third) = tokio::join!(
            limiter.wait(Some(deadline)),
            limiter.wait(Some(deadline)),
            limiter.wait(Some(deadline)),
        );
        let mut waits = [first, second, third];
        waits.sort();
        assert_eq!(
            waits,
            [None, Some(Duration::ZERO), Some(MIN_REQUEST_INTERVAL)]
        );
        // A refused slot is not consumed: at 1 s the next free slot is still
        // 2 s, one interval away, not 3 s.
        assert_eq!(limiter.wait(None).await, Some(MIN_REQUEST_INTERVAL));
    }

    #[tokio::test]
    async fn rate_limit_retries_then_recovers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/eval/100"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/eval/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":100})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            fetch_eval(&Client::new(), &server.uri(), 100)
                .await
                .unwrap()
                .id,
            100
        );
    }

    #[tokio::test]
    async fn rate_limit_exhaustion_stays_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .expect(MAX_REQUEST_ATTEMPTS as u64)
            .mount(&server)
            .await;
        let result =
            query_hydra_build(&Client::new(), &cfg_with_hydra_url(&server.uri()), "hello").await;
        assert_eq!(result.status, HydraStatus::Unknown);
        assert!(result.error.as_deref().unwrap().contains("429"));
    }

    #[tokio::test]
    async fn only_a_followed_redirect_counts_as_a_second_request() {
        // `get` counts the redirect hop from the final URL reqwest reports:
        // a followed `/latest-finished` redirect changes it, a direct 404
        // (or 429) does not.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/job/a/latest-finished"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/build/7"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/build/7"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/job/b/latest-finished"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = Client::new();
        let redirected_url = format!("{}/job/a/latest-finished", server.uri());
        let response = get(&client, &redirected_url).await.unwrap();
        assert!(redirected(&redirected_url, &response));
        let missing = format!("{}/job/b/latest-finished", server.uri());
        let response = get(&client, &missing).await.unwrap();
        assert!(!redirected(&missing, &response));
        // A dot segment is normalized away, not followed.
        let dotted = format!("{}/job/./b/latest-finished", server.uri());
        let response = get(&client, &dotted).await.unwrap();
        assert!(!redirected(&dotted, &response));
        assert_ne!(response.url().as_str(), dotted);
    }

    #[tokio::test]
    async fn jobset_pagination_collects_unique_newest_first_evals() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jobset/nixpkgs/trunk/evals"))
            .and(query_param("limit", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "evals": [{"id": 100}, {"id": 99}], "next": "?page=2"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jobset/nixpkgs/trunk/evals"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "evals": [{"id": 99}, {"id": 98}], "next": "?page=2"
            })))
            .mount(&server)
            .await;
        let cfg = cfg_with_hydra_url(&server.uri());
        let evals = query_hydra_jobset_evals(
            &Client::new(),
            &cfg,
            3,
            &mut crate::output::Output::buffered("test"),
        )
        .await;
        assert_eq!(
            evals.iter().map(|e| e.id).collect::<Vec<_>>(),
            [100, 99, 98]
        );
    }

    fn cfg_with_hydra_url(hydra_url: &str) -> PinConfig {
        PinConfig::from_json(&format!(
            r#"{{
                "name": "test",
                "packages": ["hello"],
                "inputName": "nixpkgs",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": ["https://cache.nixos.org"],
                "hydraJobset": "nixpkgs/trunk",
                "hydraUrl": "{hydra_url}",
                "hydraJobPattern": "{{jobset}}/{{pkg}}.{{arch}}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }}"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn test_query_hydra_build_on_hydra() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jobsetevals": [100, 99, 98],
            "buildoutputs": {
                "out": { "path": "/nix/store/abc123-hello-2.12" }
            }
        });
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = cfg_with_hydra_url(&server.uri());
        let client = Client::new();
        let result = query_hydra_build(&client, &cfg, "hello").await;
        assert_eq!(result.status, HydraStatus::OnHydra);
        assert_eq!(result.evals, vec![100, 99, 98]);
        assert_eq!(
            result.store_path,
            Some("/nix/store/abc123-hello-2.12".into())
        );
    }

    #[tokio::test]
    async fn test_query_hydra_build_not_on_hydra_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let cfg = cfg_with_hydra_url(&server.uri());
        let client = Client::new();
        let result = query_hydra_build(&client, &cfg, "hello").await;
        assert_eq!(result.status, HydraStatus::NotOnHydra);
        assert!(result.evals.is_empty());
        assert!(result.store_path.is_none());
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_query_hydra_build_malformed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/job/nixpkgs/trunk/hello.x86_64-linux/latest-finished",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let cfg = cfg_with_hydra_url(&server.uri());
        let client = Client::new();
        let result = query_hydra_build(&client, &cfg, "hello").await;
        assert_eq!(result.status, HydraStatus::Unknown);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_fetch_eval_success() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "id": 42,
            "flake": "github:NixOS/nixpkgs/abcdef1234567890abcdef1234567890abcdef12",
            "jobsetevalinputs": {
                "nixpkgs": { "revision": "abcdef1234567890abcdef1234567890abcdef12" }
            }
        });
        Mock::given(method("GET"))
            .and(path("/eval/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = Client::new();
        let eval = fetch_eval(&client, &server.uri(), 42).await.unwrap();
        assert_eq!(eval.id, 42);
        assert!(eval.flake.is_some());
        assert!(eval.jobsetevalinputs.is_some());
    }

    #[tokio::test]
    async fn test_fetch_eval_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/eval/99"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new();
        let result = fetch_eval(&client, &server.uri(), 99).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_eval_named_input_beyond_former_truncation_bound() {
        const EVAL_RESPONSE_MAX_BYTES: usize = 4096;
        let server = MockServer::start().await;

        // Build a response large enough that `id` is within the first 4 KB
        // but `flake` is pushed past it by a big `jobsetevals` array.
        // This forces `fetch_eval` to hit the full-fetch fallback.
        //
        // NOTE: we construct the body string manually rather than using
        // serde_json::json! because serde_json's default BTreeMap storage
        // sorts keys alphabetically, putting `flake` first and defeating
        // the large-array padding.
        let large_evals: Vec<i64> = (0..1500).collect();
        let evals_json = serde_json::to_string(&large_evals).unwrap();
        let body_string = format!(
            r#"{{"id":42,"jobsetevals":{evals_json},"flake":"github:NixOS/nixpkgs/abcdef1234567890abcdef1234567890abcdef12","jobsetevalinputs":{{"nixpkgs":{{"revision":"abcdef1234567890abcdef1234567890abcdef12"}}}}}}"#,
        );
        // Sanity: the JSON must exceed the bounded-read window
        assert!(
            body_string.len() > EVAL_RESPONSE_MAX_BYTES,
            "test body ({} B) must exceed bounded window ({EVAL_RESPONSE_MAX_BYTES} B)",
            body_string.len(),
        );
        // And `id` should still be extractable from the first 4 KB.
        assert!(
            body_string[..EVAL_RESPONSE_MAX_BYTES.min(body_string.len())].contains(r#""id":42"#),
            "id must be within the bounded-read window",
        );
        // But `flake` should NOT be in the first 4 KB (this is the whole point).
        assert!(
            !body_string[..EVAL_RESPONSE_MAX_BYTES.min(body_string.len())]
                .contains("github:NixOS/nixpkgs/"),
            "flake must NOT be within the bounded-read window",
        );

        Mock::given(method("GET"))
            .and(path("/eval/42"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&body_string))
            .mount(&server)
            .await;

        let client = Client::new();
        let eval = fetch_eval(&client, &server.uri(), 42).await.unwrap();
        assert_eq!(eval.id, 42);
        assert_eq!(
            eval.flake.as_deref(),
            Some("github:NixOS/nixpkgs/abcdef1234567890abcdef1234567890abcdef12"),
        );
        // The full-fetch path should also have populated jobsetevalinputs
        assert!(
            eval.jobsetevalinputs.is_some(),
            "full-fetch fallback should populate jobsetevalinputs",
        );
    }

    #[tokio::test]
    async fn test_query_hydra_jobset_evals_success() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "evals": [
                { "id": 100, "flake": null, "jobsetevalinputs": null },
                { "id": 99, "flake": null, "jobsetevalinputs": null }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/jobset/nixpkgs/trunk/evals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = cfg_with_hydra_url(&server.uri());
        let client = Client::new();
        let mut out = crate::output::Output::buffered("test");
        let evals = query_hydra_jobset_evals(&client, &cfg, 15, &mut out).await;
        assert_eq!(evals.len(), 2);
        assert_eq!(evals[0].id, 100);
        assert_eq!(evals[1].id, 99);
    }

    #[tokio::test]
    async fn test_query_hydra_jobset_evals_bad_jobset_format() {
        // No "/" in jobset -> returns empty immediately
        let cfg = PinConfig::from_json(
            r#"{
                "name": "test",
                "packages": [],
                "inputName": "test",
                "attrPrefix": "",
                "pythonPackages": null,
                "caches": [],
                "hydraJobset": "noslash",
                "hydraUrl": "https://hydra.nixos.org",
                "hydraJobPattern": "{jobset}/{pkg}.{arch}",
                "hydraRevInput": "nixpkgs",
                "depth": 15,
                "branch": "nixpkgs-unstable",
                "flakeRef": "github:NixOS/nixpkgs",
                "flakeOutput": "legacyPackages",
                "failFast": false,
                "arch": "x86_64-linux"
            }"#,
        )
        .unwrap();
        let client = Client::new();
        let mut out = crate::output::Output::buffered("test");
        let evals = query_hydra_jobset_evals(&client, &cfg, 15, &mut out).await;
        assert!(evals.is_empty());
    }

    #[tokio::test]
    async fn test_query_hydra_jobset_evals_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jobset/nixpkgs/trunk/evals"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = cfg_with_hydra_url(&server.uri());
        let client = Client::new();
        let mut out = crate::output::Output::buffered("test");
        let evals = query_hydra_jobset_evals(&client, &cfg, 15, &mut out).await;
        assert!(evals.is_empty());
        // Should have logged a warning
        assert!(out
            .test_buffer()
            .iter()
            .any(|l| l.contains("backend unknown") && l.contains("500")));
    }
}
