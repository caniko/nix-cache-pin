use crate::config::PinConfig;
use crate::error::{Error, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep_until, Instant};

pub const DEFAULT_HYDRA_MAX_CONCURRENCY: u16 = 4;
pub const MAX_HYDRA_CONCURRENCY: u16 = 32;
pub const DEFAULT_HYDRA_MIN_INTERVAL_MS: u32 = 500;
pub const MAX_HYDRA_MIN_INTERVAL_MS: u32 = 8_000;

const PENALTY_FLOOR: Duration = Duration::from_millis(250);
const PENALTY_MAX: Duration = Duration::from_millis(8_000);
const RECOVERY_SUCCESS_STREAK: u8 = 4;
const MAX_ATTEMPTS: usize = 6;
const RETRY_BUDGET: Duration = Duration::from_secs(180);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HydraPolicy {
    pub max_concurrency: u16,
    pub min_interval_ms: u32,
}

impl Default for HydraPolicy {
    fn default() -> Self {
        Self {
            max_concurrency: DEFAULT_HYDRA_MAX_CONCURRENCY,
            min_interval_ms: DEFAULT_HYDRA_MIN_INTERVAL_MS,
        }
    }
}

impl HydraPolicy {
    pub(crate) fn from_config(cfg: &PinConfig) -> Result<Self> {
        let policy = Self {
            max_concurrency: cfg.hydra_max_concurrency,
            min_interval_ms: cfg.hydra_min_interval_ms,
        };
        if !(1..=MAX_HYDRA_CONCURRENCY).contains(&policy.max_concurrency) {
            return Err(Error::Config(format!(
                "{}: hydraMaxConcurrency must be in 1..={MAX_HYDRA_CONCURRENCY}",
                cfg.name
            )));
        }
        if policy.min_interval_ms > MAX_HYDRA_MIN_INTERVAL_MS {
            return Err(Error::Config(format!(
                "{}: hydraMinIntervalMs must be in 0..={MAX_HYDRA_MIN_INTERVAL_MS}",
                cfg.name
            )));
        }
        Ok(policy)
    }

    pub(crate) fn for_configs(configs: &[PinConfig]) -> Result<Option<Self>> {
        let Some((first, rest)) = configs.split_first() else {
            return Ok(None);
        };
        rest.iter()
            .try_fold(Self::from_config(first)?, |policy, cfg| {
                Ok(policy.combine(Self::from_config(cfg)?))
            })
            .map(Some)
    }

    fn combine(self, other: Self) -> Self {
        Self {
            max_concurrency: self.max_concurrency.min(other.max_concurrency),
            min_interval_ms: self.min_interval_ms.max(other.min_interval_ms),
        }
    }

    fn floor(self) -> Duration {
        Duration::from_millis(self.min_interval_ms.into())
    }
}

struct State {
    next_policy_id: u64,
    policies: Vec<(u64, HydraPolicy)>,
    next_free: Instant,
    last_start: Option<(Instant, u32)>,
    cooldown_until: Instant,
    penalty: Duration,
    epoch: u64,
    clean: u8,
}

impl State {
    fn policy(&self) -> HydraPolicy {
        self.policies
            .iter()
            .map(|&(_, policy)| policy)
            .reduce(HydraPolicy::combine)
            .unwrap_or_default()
    }

    fn extend_debt(&mut self, interval: Duration) {
        if let Some((last, weight)) = self.last_start {
            self.next_free = self.next_free.max(last + interval * weight);
        }
    }
}

pub(crate) struct Limiter {
    state: Mutex<State>,
    permits: Arc<Semaphore>,
    changed: watch::Sender<()>,
}

impl Limiter {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(State {
                next_policy_id: 0,
                policies: Vec::new(),
                next_free: now,
                last_start: None,
                cooldown_until: now,
                penalty: Duration::ZERO,
                epoch: 0,
                clean: 0,
            }),
            permits: Arc::new(Semaphore::new(MAX_HYDRA_CONCURRENCY.into())),
            changed: watch::channel(()).0,
        }
    }

    async fn admit(
        &self,
        weight: u32,
        deadline: Option<Instant>,
        traffic: &TrafficCounters,
    ) -> Option<(PermitGuard, u64)> {
        let mut rx = self.changed.subscribe();
        let mut wait = None;
        loop {
            let wake_at = {
                let mut state = self.state.lock().unwrap();
                let now = Instant::now();
                let start = now.max(state.next_free).max(state.cooldown_until);
                let policy = state.policy();
                let interval = policy.floor().max(state.penalty);
                if deadline.is_some_and(|end| start + interval * (weight - 1) > end) {
                    return None;
                }
                let live = usize::from(MAX_HYDRA_CONCURRENCY) - self.permits.available_permits();
                if start == now && live < usize::from(policy.max_concurrency) {
                    if let Ok(permit) = self.permits.clone().try_acquire_owned() {
                        state.last_start = Some((now, weight));
                        state.next_free = now + interval * weight;
                        return Some((
                            PermitGuard {
                                permit: Some(permit),
                                changed: self.changed.clone(),
                            },
                            state.epoch,
                        ));
                    }
                }
                // At the deadline the admission above is the final nonblocking check.
                if deadline.is_some_and(|end| now >= end) {
                    return None;
                }
                (start > now).then_some(start)
            };
            wait.get_or_insert_with(|| WaitTimer(traffic, Instant::now()));
            tokio::select! {
                _ = rx.changed() => {},
                _ = wake(wake_at) => {},
                _ = wake(deadline) => {},
            }
        }
    }

    fn register(self: &Arc<Self>, policy: HydraPolicy) -> PolicyGuard {
        let mut state = self.state.lock().unwrap();
        let previous = state.policy().floor().max(state.penalty);
        let id = state.next_policy_id;
        state.next_policy_id += 1;
        state.policies.push((id, policy));
        let interval = state.policy().floor().max(state.penalty);
        if interval > previous {
            state.extend_debt(interval);
        }
        self.changed.send_replace(());
        PolicyGuard(Some(Arc::new(PolicyRegistration {
            limiter: self.clone(),
            id,
        })))
    }

    fn congestion(
        &self,
        escalation_epoch: Option<u64>,
        status: StatusCode,
        delay: Duration,
        traffic: &TrafficCounters,
    ) -> Instant {
        if status == StatusCode::TOO_MANY_REQUESTS {
            traffic.throttled.fetch_add(1, Ordering::Relaxed);
        }
        let mut state = self.state.lock().unwrap();
        let receipt = Instant::now();
        state.cooldown_until = state.cooldown_until.max(receipt + delay);
        state.clean = 0;
        if escalation_epoch == Some(state.epoch) {
            let floor = state.policy().floor();
            let interval = floor.max(state.penalty);
            state.penalty = interval.saturating_mul(2).clamp(PENALTY_FLOOR, PENALTY_MAX);
            state.epoch = state.epoch.wrapping_add(1);
            let interval = floor.max(state.penalty);
            state.extend_debt(interval);
        }
        self.changed.send_replace(());
        receipt
    }

    fn feedback(&self, epoch: u64, clean: bool) {
        let mut state = self.state.lock().unwrap();
        if !clean {
            state.clean = 0;
        } else if epoch == state.epoch && state.penalty > Duration::ZERO {
            state.clean += 1;
            if state.clean >= RECOVERY_SUCCESS_STREAK {
                state.clean = 0;
                let penalty = state.penalty / 2;
                state.penalty = if penalty <= state.policy().floor() || penalty < PENALTY_FLOOR {
                    Duration::ZERO
                } else {
                    penalty
                };
                self.changed.send_replace(());
            }
        }
    }
}

async fn wake(at: Option<Instant>) {
    if let Some(at) = at {
        sleep_until(at).await;
    } else {
        std::future::pending::<()>().await;
    }
}

struct PermitGuard {
    permit: Option<OwnedSemaphorePermit>,
    changed: watch::Sender<()>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.changed.send_replace(());
    }
}

/// A scoped process-wide Hydra policy. Clones keep the registration active.
#[derive(Clone)]
pub struct PolicyGuard(#[allow(dead_code)] Option<Arc<PolicyRegistration>>);

struct PolicyRegistration {
    limiter: Arc<Limiter>,
    id: u64,
}

impl Drop for PolicyRegistration {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().unwrap();
        let previous = state.policy().floor().max(state.penalty);
        state.policies.retain(|&(id, _)| id != self.id);
        let interval = state.policy().floor().max(state.penalty);
        if interval > previous {
            state.extend_debt(interval);
        }
        self.limiter.changed.send_replace(());
    }
}

static LIMITER: OnceLock<Arc<Limiter>> = OnceLock::new();

fn limiter() -> &'static Arc<Limiter> {
    LIMITER.get_or_init(|| Arc::new(Limiter::new()))
}

/// Apply the strictest concurrency and interval limits from these pins.
pub fn policy_scope(configs: &[PinConfig]) -> Result<PolicyGuard> {
    Ok(HydraPolicy::for_configs(configs)?
        .map_or(PolicyGuard(None), |policy| limiter().register(policy)))
}

#[derive(Default)]
pub(crate) struct TrafficCounters {
    requests: AtomicU64,
    throttled: AtomicU64,
    waited_micros: AtomicU64,
}

static TRAFFIC: TrafficCounters = TrafficCounters {
    requests: AtomicU64::new(0),
    throttled: AtomicU64::new(0),
    waited_micros: AtomicU64::new(0),
};

impl TrafficCounters {
    fn snapshot(&self) -> HydraTraffic {
        HydraTraffic {
            requests: self.requests.load(Ordering::Relaxed),
            throttled: self.throttled.load(Ordering::Relaxed),
            waited_ms: self.waited_micros.load(Ordering::Relaxed) / 1000,
        }
    }
}

struct WaitTimer<'a>(&'a TrafficCounters, Instant);

impl Drop for WaitTimer<'_> {
    fn drop(&mut self) {
        let micros = u64::try_from(self.1.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.0.waited_micros.fetch_add(micros, Ordering::Relaxed);
    }
}

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
    TRAFFIC.snapshot()
}

struct GuardedResponse {
    response: reqwest::Response,
    _permit: Option<PermitGuard>,
}

impl GuardedResponse {
    fn status(&self) -> StatusCode {
        self.response.status()
    }

    fn url(&self) -> &reqwest::Url {
        self.response.url()
    }

    fn error_for_status(self) -> reqwest::Result<Self> {
        self.response.error_for_status_ref()?;
        Ok(self)
    }

    async fn json<T: serde::de::DeserializeOwned>(self) -> reqwest::Result<T> {
        self.response.json().await
    }

    async fn text(self) -> reqwest::Result<String> {
        self.response.text().await
    }
}

async fn get(client: &Client, url: &str) -> reqwest::Result<GuardedResponse> {
    get_with(client, url, limiter(), &TRAFFIC).await
}

/// Retry explicit throttling/unavailability, keeping capacity through body reads.
async fn get_with(
    client: &Client,
    url: &str,
    limiter: &Limiter,
    traffic: &TrafficCounters,
) -> reqwest::Result<GuardedResponse> {
    let parsed = reqwest::Url::parse(url).ok();
    // Reqwest follows the redirect itself; reserve both starts atomically.
    let weight = if parsed
        .as_ref()
        .is_some_and(|url| url.path().ends_with("/latest-finished"))
    {
        2
    } else {
        1
    };
    let paced = paced(parsed.as_ref());
    let mut deadline = None;
    let mut last = None;
    for attempt in 0..MAX_ATTEMPTS {
        let admission = if paced {
            let Some(admission) = limiter.admit(weight, deadline, traffic).await else {
                return Err(last.expect("only retries have an admission deadline"));
            };
            Some(admission)
        } else {
            if deadline.is_some_and(|end| Instant::now() > end) {
                return Err(last.expect("only retries have a previous HTTP error"));
            }
            None
        };
        // The initial queue wait does not spend the HTTP retry budget.
        let deadline = *deadline.get_or_insert_with(|| Instant::now() + RETRY_BUDGET);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let request = client
            .get(url)
            .header("Accept", "application/json")
            .header(
                reqwest::header::USER_AGENT,
                concat!("nix-cache-pin/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(remaining.clamp(Duration::from_secs(1), Duration::from_secs(30)));
        if paced {
            traffic.requests.fetch_add(1, Ordering::Relaxed);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some((_, epoch)) = &admission {
                    limiter.feedback(*epoch, false);
                }
                return Err(error);
            }
        };
        let (permit, epoch) = admission
            .map(|(permit, epoch)| (Some(permit), Some(epoch)))
            .unwrap_or_default();
        let response = GuardedResponse {
            response,
            _permit: permit,
        };
        if paced && parsed.as_ref() != Some(response.url()) {
            traffic.requests.fetch_add(1, Ordering::Relaxed);
        }
        let status = response.status();
        if !matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            if let Some(epoch) = epoch {
                if status.is_server_error() {
                    limiter.feedback(epoch, false);
                } else if (200..500).contains(&status.as_u16()) {
                    limiter.feedback(epoch, true);
                }
            }
            return Ok(response);
        }
        let delay = retry_delay(response.response.headers(), SystemTime::now(), attempt);
        let receipt = if let Some(epoch) = epoch {
            // Retries still publish cooldown, but only the first attempt can raise the penalty.
            limiter.congestion((attempt == 0).then_some(epoch), status, delay, traffic)
        } else {
            Instant::now()
        };
        let error = response.response.error_for_status_ref().unwrap_err();
        drop(response);
        if attempt + 1 == MAX_ATTEMPTS || receipt + delay >= deadline {
            return Err(error);
        }
        last = Some(error);
        if !paced && !delay.is_zero() {
            sleep_until(receipt + delay).await;
        }
    }
    unreachable!("bounded attempts return their final response")
}

/// The channel release bucket is neither paced nor counted.
fn paced(url: Option<&reqwest::Url>) -> bool {
    url.and_then(reqwest::Url::host_str)
        .is_none_or(|host| !host.eq_ignore_ascii_case("nix-releases.s3.amazonaws.com"))
}

/// Pause before the next attempt: Retry-After when Hydra sends one, an
/// exponential backoff otherwise, never above `MAX_RETRY_DELAY`.
fn retry_delay(headers: &reqwest::header::HeaderMap, now: SystemTime, attempt: usize) -> Duration {
    retry_after(headers, now)
        .unwrap_or_else(|| RETRY_BACKOFF * (1u32 << attempt))
        .min(MAX_RETRY_DELAY)
}

fn retry_after(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
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
    PacedClient::global(client)
        .query_latest_builds(hydra_url, project, jobset, job, nr)
        .await
}

#[derive(Clone, Copy)]
pub(crate) struct PacedClient<'a> {
    pub(crate) client: &'a Client,
    pub(crate) limiter: &'a Limiter,
    pub(crate) traffic: &'a TrafficCounters,
}

impl<'a> PacedClient<'a> {
    pub(crate) fn global(client: &'a Client) -> Self {
        Self {
            client,
            limiter: limiter(),
            traffic: &TRAFFIC,
        }
    }

    async fn get(self, url: &str) -> Result<GuardedResponse> {
        get_with(self.client, url, self.limiter, self.traffic)
            .await
            .map_err(Error::Http)
    }

    pub(crate) async fn query_latest_builds(
        self,
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

        let response = self.get(url.as_str()).await?;
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

    pub(crate) async fn get_text(self, url: &str) -> Result<String> {
        self.get(url)
            .await?
            .error_for_status()
            .map_err(Error::Http)?
            .text()
            .await
            .map_err(Error::Http)
    }
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
    let _policy = match policy_scope(std::slice::from_ref(cfg)) {
        Ok(policy) => policy,
        Err(error) => {
            return HydraBuildResult {
                package: pkg.to_string(),
                evals: vec![],
                status: HydraStatus::Unknown,
                store_path: None,
                error: Some(error.to_string()),
                build_status: None,
            };
        }
    };
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

/// Fetch the raw latest-finished build JSON for a standalone job lookup.
pub async fn query_job(client: &Client, hydra_url: &str, job: &str) -> Result<serde_json::Value> {
    let url = format!("{hydra_url}/job/{job}/latest-finished");
    Ok(get(client, &url).await?.error_for_status()?.json().await?)
}

/// Query the Hydra jobset eval list.
pub async fn query_hydra_jobset_evals(
    client: &Client,
    cfg: &PinConfig,
    limit: usize,
    out: &mut crate::output::Output,
) -> Vec<HydraEval> {
    let _policy = match policy_scope(std::slice::from_ref(cfg)) {
        Ok(policy) => policy,
        Err(error) => {
            out.milestone(format!("  Hydra history backend unknown: {error}"));
            return vec![];
        }
    };
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

    #[test]
    fn hydra_policy_json_is_backward_compatible_and_validated() {
        let cfg = test_cfg();
        assert_eq!(cfg.hydra_max_concurrency, 4);
        assert_eq!(cfg.hydra_min_interval_ms, 500);
        let mut json = serde_json::to_value(cfg).unwrap();
        assert!(json.get("hydraMaxConcurrency").is_none());
        assert!(json.get("hydraMinIntervalMs").is_none());
        json["hydraMaxConcurrency"] = 32.into();
        json["hydraMinIntervalMs"] = 0.into();
        let cfg = PinConfig::from_json(&json.to_string()).unwrap();
        assert_eq!(
            (cfg.hydra_max_concurrency, cfg.hydra_min_interval_ms),
            (32, 0)
        );
        assert_eq!(serde_json::to_value(cfg).unwrap(), json);
        for (field, value) in [
            ("hydraMaxConcurrency", 0),
            ("hydraMaxConcurrency", 33),
            ("hydraMinIntervalMs", 8001),
        ] {
            let mut invalid = json.clone();
            invalid[field] = value.into();
            assert!(matches!(
                PinConfig::from_json(&invalid.to_string()),
                Err(Error::Config(message)) if message.contains(field)
            ));
        }
    }

    #[tokio::test]
    async fn find_all_reports_a_mutated_policy_before_spawning() {
        assert!(policy_scope(&[]).unwrap().0.is_none());
        let mut invalid = test_cfg();
        invalid.hydra_max_concurrency = 33;
        assert!(matches!(
            policy_scope(&[test_cfg(), invalid.clone()]),
            Err(Error::Config(message)) if message.contains("hydraMaxConcurrency")
        ));
        let results =
            crate::runner::find_all(vec![invalid], false, &Arc::new(crate::ext::RealCommands))
                .await;
        assert!(matches!(
            &results[0].target_rev,
            Err(Error::Config(message)) if message.contains("hydraMaxConcurrency")
        ));
    }

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
        for value in ["3600", "18446744073709551616"] {
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(
                retry_delay(&headers, SystemTime::UNIX_EPOCH, 0),
                MAX_RETRY_DELAY
            );
        }
        for value in ["0", "Thu, 01 Jan 1970 00:00:00 GMT"] {
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(
                retry_delay(&headers, SystemTime::UNIX_EPOCH + Duration::from_secs(1), 3),
                Duration::ZERO
            );
        }
    }

    #[test]
    fn only_hydra_requests_are_paced() {
        for (url, expected) in [
            ("https://hydra.nixos.org/build/1", true),
            (
                "https://nix-releases.s3.amazonaws.com/?prefix=nixos/unstable/",
                false,
            ),
            (
                "https://NIX-RELEASES.s3.amazonaws.com/nixos/unstable/x/src-url",
                false,
            ),
            ("https://nix-releases.s3.amazonaws.com.example.org/", true),
        ] {
            assert_eq!(paced(Some(&reqwest::Url::parse(url).unwrap())), expected);
        }
    }

    fn local_policy(concurrency: u16, interval: u32) -> (Arc<Limiter>, PolicyGuard) {
        let limiter = Arc::new(Limiter::new());
        let guard = limiter.register(HydraPolicy {
            max_concurrency: concurrency,
            min_interval_ms: interval,
        });
        (limiter, guard)
    }

    #[tokio::test(start_paused = true)]
    async fn burst_is_spaced_and_capacity_is_bounded() {
        let (limiter, _scope) = local_policy(3, 20);
        let counters = TrafficCounters::default();
        let start = Instant::now();
        let mut active = Vec::new();
        for i in 0..3 {
            active.push(limiter.admit(1, None, &counters).await.unwrap());
            let scheduled = Duration::from_millis(20 * i);
            assert!(start.elapsed() >= scheduled);
            assert!(start.elapsed() <= scheduled + Duration::from_millis(3));
        }
        let mut waiting = Box::pin(limiter.admit(1, None, &counters));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(30)).await;
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        active.pop();
        assert!(futures_util::poll!(waiting.as_mut()).is_ready());
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_is_inclusive_and_a_weighted_start_is_atomic() {
        let (limiter, _scope) = local_policy(1, 100);
        let counters = TrafficCounters::default();
        let (held, epoch) = limiter.admit(1, None, &counters).await.unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        assert!(limiter.admit(2, Some(deadline), &counters).await.is_none());
        assert_eq!(limiter.state.lock().unwrap().next_free, deadline);
        let mut waiting = Box::pin(limiter.admit(1, Some(deadline), &counters));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(100)).await;
        drop(held);
        let (held, _) = waiting.await.unwrap();
        tokio::time::advance(Duration::from_nanos(1)).await;
        assert!(limiter.admit(1, Some(deadline), &counters).await.is_none());

        let deadline = deadline + Duration::from_millis(500);
        tokio::time::advance(Duration::from_millis(100)).await;
        let mut waiting = Box::pin(limiter.admit(1, Some(deadline), &counters));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        limiter.congestion(
            Some(epoch),
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(1),
            &counters,
        );
        drop(held);
        assert!(waiting.await.is_none());
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_only_wait_expires_without_spending_a_slot() {
        let (limiter, _scope) = local_policy(1, 0);
        let counters = TrafficCounters::default();
        let _held = limiter.admit(1, None, &counters).await.unwrap();
        let start = Instant::now();
        assert!(limiter
            .admit(1, Some(start + Duration::from_millis(50)), &counters)
            .await
            .is_none());
        assert_eq!(limiter.state.lock().unwrap().next_free, start);
        assert_eq!(
            counters.snapshot().waited_ms,
            start.elapsed().as_millis() as u64
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scopes_fold_in_either_order_and_last_clone_keeps_debt() {
        for reverse in [false, true] {
            let (limiter, initial) = local_policy(12, 0);
            let counters = TrafficCounters::default();
            let start = Instant::now();
            let (_, epoch) = limiter.admit(2, None, &counters).await.unwrap();
            limiter.congestion(
                Some(epoch),
                StatusCode::SERVICE_UNAVAILABLE,
                Duration::from_secs(2),
                &counters,
            );
            let narrow = HydraPolicy {
                max_concurrency: 2,
                min_interval_ms: 20,
            };
            let slow = HydraPolicy {
                max_concurrency: 8,
                min_interval_ms: 700,
            };
            let (first, second) = if reverse {
                (slow, narrow)
            } else {
                (narrow, slow)
            };
            let first_guard = limiter.register(first);
            let second_guard = limiter.register(second);
            assert_eq!(
                limiter.state.lock().unwrap().policy(),
                HydraPolicy {
                    max_concurrency: 2,
                    min_interval_ms: 700,
                }
            );
            let last_clone = second_guard.clone();
            drop(second_guard);
            assert_eq!(limiter.state.lock().unwrap().policies.len(), 3);
            drop(last_clone);
            assert_eq!(limiter.state.lock().unwrap().policy(), first);
            drop(first_guard);
            assert_eq!(limiter.state.lock().unwrap().policy().min_interval_ms, 0);
            drop(initial);
            let state = limiter.state.lock().unwrap();
            assert_eq!(state.policy(), HydraPolicy::default());
            assert_eq!(state.next_free, start + Duration::from_millis(1_400));
            assert_eq!(state.cooldown_until, start + Duration::from_secs(2));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_zero_interval_scope_restores_weighted_default_pacing() {
        let (limiter, scope) = local_policy(1, 0);
        let traffic = TrafficCounters::default();
        let (permit, _) = limiter.admit(2, None, &traffic).await.unwrap();
        drop(permit);
        drop(scope);

        let mut next = Box::pin(limiter.admit(1, None, &traffic));
        assert!(futures_util::poll!(next.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(futures_util::poll!(next.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            futures_util::poll!(next.as_mut()),
            std::task::Poll::Ready(Some(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn congestion_deduplicates_penalties_but_keeps_the_latest_cooldown() {
        let (limiter, _scope) = local_policy(4, 500);
        let traffic = TrafficCounters::default();
        let mut burst = Vec::new();
        for _ in 0..4 {
            burst.push(limiter.admit(1, None, &traffic).await.unwrap());
        }
        let receipt = Instant::now();
        for ((permit, epoch), (status, retry_after)) in burst.into_iter().zip([
            (StatusCode::TOO_MANY_REQUESTS, "0"),
            (StatusCode::SERVICE_UNAVAILABLE, "3600"),
            (StatusCode::TOO_MANY_REQUESTS, "60"),
            (StatusCode::SERVICE_UNAVAILABLE, "malformed"),
        ]) {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::RETRY_AFTER, retry_after.parse().unwrap());
            limiter.congestion(
                Some(epoch),
                status,
                retry_delay(&headers, SystemTime::UNIX_EPOCH, 0),
                &traffic,
            );
            drop(permit);
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        let cooldown = receipt + Duration::from_millis(60_200);
        {
            let state = limiter.state.lock().unwrap();
            assert_eq!(state.epoch, 1);
            assert_eq!(state.penalty, Duration::from_secs(1));
            assert_eq!(state.cooldown_until, cooldown);
        }
        assert_eq!(traffic.snapshot().throttled, 2);
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY)
        );
        for _ in 0..4 {
            limiter.feedback(0, true);
        }
        assert_eq!(limiter.state.lock().unwrap().clean, 0);
        limiter.feedback(1, true);
        assert_eq!(limiter.state.lock().unwrap().clean, 1);
        // A late 500 from the old epoch breaks the active recovery streak.
        limiter.feedback(0, false);
        let clean = limiter.state.lock().unwrap().clean;
        assert_eq!(clean, 0);
        limiter.feedback(1, true);
        assert_eq!(limiter.state.lock().unwrap().clean, 1);
        limiter.feedback(1, false);
        assert_eq!(limiter.state.lock().unwrap().clean, 0);
        for _ in 0..4 {
            let (_permit, epoch) = limiter.admit(1, None, &traffic).await.unwrap();
            assert!(Instant::now() >= cooldown);
            limiter.feedback(epoch, true);
        }
        let penalty = limiter.state.lock().unwrap().penalty;
        assert_eq!(penalty, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn clean_streaks_halve_penalty_from_the_cap() {
        let (limiter, _scope) = local_policy(1, 500);
        let traffic = TrafficCounters::default();
        for _ in 0..4 {
            let (_permit, epoch) = limiter.admit(1, None, &traffic).await.unwrap();
            limiter.congestion(
                Some(epoch),
                StatusCode::SERVICE_UNAVAILABLE,
                Duration::ZERO,
                &traffic,
            );
        }
        let penalty = limiter.state.lock().unwrap().penalty;
        assert_eq!(penalty, PENALTY_MAX);

        let start = Instant::now();
        for expected in [4, 2, 1, 0] {
            for _ in 0..RECOVERY_SUCCESS_STREAK {
                let (_permit, epoch) = limiter.admit(1, None, &traffic).await.unwrap();
                limiter.feedback(epoch, true);
            }
            let penalty = limiter.state.lock().unwrap().penalty;
            assert_eq!(penalty, Duration::from_secs(expected));
        }
        assert!(start.elapsed() < Duration::from_secs(68));
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_waits_are_counted_once_and_cancellation_spends_no_slot() {
        let (limiter, _scope) = local_policy(1, 100);
        let traffic = TrafficCounters::default();
        let start = Instant::now();
        let (held, epoch) = limiter.admit(1, None, &traffic).await.unwrap();
        let mut cancelled = Box::pin(limiter.admit(2, None, &traffic));
        assert!(futures_util::poll!(cancelled.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(20)).await;
        drop(cancelled);
        assert_eq!(traffic.snapshot().waited_ms, 20);
        assert_eq!(
            limiter.state.lock().unwrap().next_free,
            start + Duration::from_millis(100)
        );

        let mut waiting = Box::pin(limiter.admit(1, None, &traffic));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        limiter.congestion(
            Some(epoch),
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(1),
            &traffic,
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        drop(held);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY)
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(futures_util::poll!(waiting.as_mut()).is_ready());
        assert_eq!(traffic.snapshot().waited_ms, 1020);
    }

    #[tokio::test]
    async fn cancelling_a_slow_body_releases_its_permit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let serve = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            socket // Keep the body open without sending the byte.
        };
        let (limiter, _scope) = local_policy(1, 0);
        let traffic = TrafficCounters::default();
        let client = Client::builder().no_proxy().build().unwrap();
        let (socket, response) = tokio::join!(serve, get_with(&client, &url, &limiter, &traffic));
        let mut body = Box::pin(response.unwrap().text());
        assert!(futures_util::poll!(body.as_mut()).is_pending());
        let mut next = Box::pin(limiter.admit(1, None, &traffic));
        assert!(futures_util::poll!(next.as_mut()).is_pending());
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY) - 1
        );
        drop(body);
        assert!(futures_util::poll!(next.as_mut()).is_ready());
        drop(socket);
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
        let (limiter, _scope) = local_policy(1, 0);
        let counters = TrafficCounters::default();
        let result = get_with(
            &Client::new(),
            &format!("{}/eval/100", server.uri()),
            &limiter,
            &counters,
        )
        .await
        .unwrap()
        .json::<HydraEval>()
        .await
        .unwrap();
        assert_eq!(result.id, 100);
        assert_eq!(counters.snapshot().requests, 2);
        assert_eq!(counters.snapshot().throttled, 1);
    }

    #[tokio::test]
    async fn s3_retries_without_changing_shared_state_or_counters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .expect(MAX_ATTEMPTS as u64)
            .mount(&server)
            .await;
        let port = server.address().port();
        let host = "nix-releases.s3.amazonaws.com";
        let client = Client::builder()
            .no_proxy()
            .resolve(host, *server.address())
            .build()
            .unwrap();
        let (limiter, _scope) = local_policy(1, 8_000);
        let counters = TrafficCounters::default();
        let _held = limiter.admit(1, None, &counters).await.unwrap();
        let result = get_with(
            &client,
            &format!("http://{host}:{port}/"),
            &limiter,
            &counters,
        )
        .await;
        assert_eq!(
            result.err().unwrap().status(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(counters.snapshot(), HydraTraffic::default());
        assert_eq!(limiter.state.lock().unwrap().epoch, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_publish_cooldown_even_when_attempts_or_time_run_out() {
        for (status, delay, attempts) in [(503, 2, 6), (429, 2, 6), (503, 60, 3)] {
            let retry_after = delay.to_string();
            let delay = Duration::from_secs(delay);
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(status).insert_header("Retry-After", retry_after),
                )
                .expect(attempts)
                .mount(&server)
                .await;
            // Keep the paused clock from auto-advancing over real socket I/O.
            let (release, parked) = std::sync::mpsc::channel::<()>();
            let clock_guard = tokio::task::spawn_blocking(move || parked.recv());
            let (limiter, _scope) = local_policy(1, 500);
            let traffic = TrafficCounters::default();
            let mut rx = limiter.changed.subscribe();
            let client = Client::builder().no_proxy().build().unwrap();
            let url = server.uri();
            let mut request = Box::pin(get_with(&client, &url, &limiter, &traffic));
            let drive_clock = async {
                for sent in 1..attempts {
                    while limiter.state.lock().unwrap().cooldown_until <= Instant::now() {
                        rx.changed().await.unwrap();
                    }
                    assert_eq!(traffic.snapshot().requests, sent);
                    assert_eq!(
                        limiter.permits.available_permits(),
                        usize::from(MAX_HYDRA_CONCURRENCY)
                    );
                    let eligible = {
                        let state = limiter.state.lock().unwrap();
                        assert_eq!(state.cooldown_until, Instant::now() + delay);
                        state.next_free.max(state.cooldown_until)
                    };
                    tokio::time::advance(eligible - Instant::now() - Duration::from_millis(1))
                        .await;
                    assert_eq!(traffic.snapshot().requests, sent);
                    // Tokio timers have millisecond granularity; advance past the edge.
                    tokio::time::advance(Duration::from_millis(2)).await;
                }
            };
            let result = tokio::select! {
                result = &mut request => result,
                () = drive_clock => request.await,
            };
            assert_eq!(
                result.err().unwrap().status(),
                Some(StatusCode::from_u16(status).unwrap())
            );
            assert_eq!(traffic.snapshot().requests, attempts);
            assert_eq!(
                traffic.snapshot().throttled,
                if status == 429 { attempts } else { 0 }
            );
            let (epoch, penalty, cooldown) = {
                let state = limiter.state.lock().unwrap();
                (state.epoch, state.penalty, state.cooldown_until)
            };
            assert_eq!(epoch, 1);
            assert_eq!(penalty, Duration::from_secs(1));
            assert_eq!(cooldown, Instant::now() + delay);
            assert_eq!(
                limiter.permits.available_permits(),
                usize::from(MAX_HYDRA_CONCURRENCY)
            );
            drop(release);
            assert_eq!(clock_guard.await.unwrap(), Err(std::sync::mpsc::RecvError));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_admission_wait_does_not_spend_the_retry_budget() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (limiter, _scope) = local_policy(1, 0);
        let traffic = TrafficCounters::default();
        let client = Client::builder().no_proxy().build().unwrap();
        let (held, _) = limiter.admit(1, None, &traffic).await.unwrap();
        let mut request = Box::pin(get_with(&client, &url, &limiter, &traffic));
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        tokio::time::advance(RETRY_BUDGET + Duration::from_secs(1)).await;
        assert_eq!(traffic.snapshot().requests, 0);
        drop(held);

        // Advance time only at fixture-controlled points, not over socket I/O.
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let clock_guard = tokio::task::spawn_blocking(move || parked.recv());
        let mut rx = limiter.changed.subscribe();
        let serve = async {
            for first in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(socket.read_u8().await.unwrap());
                }
                if first {
                    // A request with the old one-second timeout cannot get this response.
                    tokio::time::advance(Duration::from_secs(29)).await;
                    socket
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    while limiter.state.lock().unwrap().epoch == 0 {
                        rx.changed().await.unwrap();
                    }
                    tokio::time::advance(Duration::from_secs(2)).await;
                } else {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        };
        let result = tokio::select! {
            result = &mut request => result,
            () = serve => request.await,
        };
        drop(release);
        assert_eq!(clock_guard.await.unwrap(), Err(std::sync::mpsc::RecvError));
        let response = result.expect("the first timeout and retry budget start after admission");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(traffic.snapshot().requests, 2);
    }

    #[tokio::test]
    async fn a_failed_send_is_counted_but_not_retried() {
        let client = Client::new();
        let (limiter, _scope) = local_policy(1, 0);
        let counters = TrafficCounters::default();
        let result = get_with(&client, "not a valid URL", &limiter, &counters).await;
        assert!(result.err().unwrap().is_builder());
        assert_eq!(
            counters.snapshot(),
            HydraTraffic {
                requests: 1,
                ..HydraTraffic::default()
            }
        );
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY)
        );
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
        let redirected_url = format!("{}/job/a/latest-finished?probe=1", server.uri());
        let (limiter, _scope) = local_policy(1, 0);
        let counters = TrafficCounters::default();
        get_with(&client, &redirected_url, &limiter, &counters)
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(counters.snapshot().requests, 2);
        assert_eq!(limiter.state.lock().unwrap().last_start.unwrap().1, 2);
        // A dot segment is normalized away, not followed.
        let dotted = format!("{}/job/./b/latest-finished", server.uri());
        let response = get_with(&client, &dotted, &limiter, &counters)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(counters.snapshot().requests, 3);
        drop(response);
        assert_eq!(
            limiter.permits.available_permits(),
            usize::from(MAX_HYDRA_CONCURRENCY)
        );
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
