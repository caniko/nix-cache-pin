use crate::config::PinConfig;
use crate::error::{Error, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::{Duration, Instant, SystemTime};

const MAX_REQUEST_ATTEMPTS: usize = 3;
const REQUEST_BUDGET: Duration = Duration::from_secs(60);

/// Retry only explicit throttling/unavailability, with a fixed total budget.
/// A Retry-After beyond that budget returns the failure without retrying early.
async fn get(client: &Client, url: &str) -> reqwest::Result<reqwest::Response> {
    let started = Instant::now();
    for attempt in 0..MAX_REQUEST_ATTEMPTS {
        let remaining = REQUEST_BUDGET.saturating_sub(started.elapsed());
        let response = client
            .get(url)
            .header("Accept", "application/json")
            .timeout(remaining.min(Duration::from_secs(30)))
            .send()
            .await?;
        if !matches!(
            response.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
        ) || attempt + 1 == MAX_REQUEST_ATTEMPTS
        {
            return Ok(response);
        }
        let delay = retry_after(response.headers(), SystemTime::now())
            .unwrap_or_else(|| Duration::from_secs(1 << attempt));
        if delay >= REQUEST_BUDGET.saturating_sub(started.elapsed()) {
            return Ok(response);
        }
        drop(response);
        tokio::time::sleep(delay).await;
    }
    unreachable!("bounded attempts return their final response")
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

    #[tokio::test]
    async fn long_retry_after_does_not_retry_early() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "3600"))
            .expect(1)
            .mount(&server)
            .await;
        assert!(fetch_eval(&Client::new(), &server.uri(), 100)
            .await
            .is_err());
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
            .expect(3)
            .mount(&server)
            .await;
        let result =
            query_hydra_build(&Client::new(), &cfg_with_hydra_url(&server.uri()), "hello").await;
        assert_eq!(result.status, HydraStatus::Unknown);
        assert!(result.error.as_deref().unwrap().contains("429"));
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
