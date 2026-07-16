use crate::config::PinConfig;
use crate::error::{Error, Result};
use colored::Colorize;
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HydraStatus {
    OnHydra,
    NotOnHydra,
}

#[derive(Debug, Clone)]
pub struct HydraBuildResult {
    pub package: String,
    pub evals: Vec<i64>,
    pub status: HydraStatus,
    pub store_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HydraBuild {
    jobsetevals: Option<Vec<i64>>,
    buildoutputs: Option<std::collections::HashMap<String, BuildOutput>>,
}

#[derive(Debug, Deserialize)]
struct BuildOutput {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HydraEval {
    pub id: i64,
    pub flake: Option<String>,
    pub jobsetevalinputs: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct HydraEvalsResponse {
    evals: Vec<HydraEval>,
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
    let result = client
        .get(&job_url)
        .header("Accept", "application/json")
        .send()
        .await
        .and_then(|r| r.error_for_status());

    match result {
        Ok(resp) => match resp.json::<HydraBuild>().await {
            Ok(build) => {
                let store_path = build
                    .buildoutputs
                    .as_ref()
                    .and_then(|o| o.get("out"))
                    .and_then(|o| o.path.clone());
                HydraBuildResult {
                    package: pkg.to_string(),
                    evals: build.jobsetevals.unwrap_or_default(),
                    status: HydraStatus::OnHydra,
                    store_path,
                }
            }
            Err(_) => HydraBuildResult {
                package: pkg.to_string(),
                evals: vec![],
                status: HydraStatus::NotOnHydra,
                store_path: None,
            },
        },
        Err(_) => HydraBuildResult {
            package: pkg.to_string(),
            evals: vec![],
            status: HydraStatus::NotOnHydra,
            store_path: None,
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

    match client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) => match resp.json::<HydraEvalsResponse>().await {
            Ok(r) => r.evals,
            Err(e) => {
                out.milestone(format!(
                    "{}",
                    format!("  Warning: failed to parse jobset evals: {e}").yellow()
                ));
                vec![]
            }
        },
        Err(e) => {
            out.milestone(format!(
                "{}",
                format!("  Warning: failed to fetch jobset evals: {e}").yellow()
            ));
            vec![]
        }
    }
}

/// Maximum bytes to read from a Hydra eval response.
///
/// The `/eval/{id}` endpoint can return multi-megabyte responses (the
/// `jobsetevalinputs.value` field), but we only need `id`, `flake`, and the
/// nixpkgs `revision` — all of which typically appear within the first ~200
/// bytes.  When the bounded window does not capture `flake` (e.g. a large
/// `jobsetevals` array precedes it), `fetch_eval` falls back to a full JSON
/// deserialisation.
const EVAL_RESPONSE_MAX_BYTES: usize = 4096;

/// Fetch eval metadata from Hydra.
///
/// Fast path: bounded read + regex (avoids downloading multi-MB
/// `jobsetevalinputs`).  When the bounded read cannot extract both `id` and
/// `flake`, falls back to a full JSON request that also populates
/// `jobsetevalinputs` (needed by pins whose `hydraRevInput` references a
/// named flake input rather than `"flake"`).
pub async fn fetch_eval(client: &Client, hydra_url: &str, eval_id: i64) -> Result<HydraEval> {
    let url = format!("{hydra_url}/eval/{eval_id}");

    match bounded_fetch_eval(client, &url, eval_id).await {
        Ok(eval) if eval.flake.is_some() => return Ok(eval),
        Ok(_) => { /* flake missing from bounded window — fall through */ }
        Err(_) => { /* bounded read failed — fall through */ }
    }

    full_fetch_eval(client, &url).await
}

/// Bounded-read fast path: reads at most `EVAL_RESPONSE_MAX_BYTES` bytes
/// and extracts `id` and `flake` via regex.  Returns an error when the
/// `id` field is not found within the truncation window.
async fn bounded_fetch_eval(client: &Client, url: &str, eval_id: i64) -> Result<HydraEval> {
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;

    use futures_util::StreamExt;

    let mut buf = Vec::with_capacity(EVAL_RESPONSE_MAX_BYTES);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::Http)?;
        let remaining = EVAL_RESPONSE_MAX_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let end = chunk.len().min(remaining);
        buf.extend_from_slice(&chunk[..end]);
        if buf.len() >= EVAL_RESPONSE_MAX_BYTES {
            break;
        }
    }
    // Stream dropped here — remaining bytes are not downloaded.

    let text = std::str::from_utf8(&buf)
        .map_err(|_| Error::Config("eval response is not valid UTF-8".into()))?;

    let re_id = regex::Regex::new(r#""id"\s*:\s*(\d+)"#)
        .map_err(|e| Error::Config(format!("id regex: {e}")))?;
    let id = re_id
        .captures(text)
        .and_then(|c| c[1].parse().ok())
        .ok_or_else(|| {
            Error::Config(format!(
                "eval id not found in truncated response for eval {eval_id}"
            ))
        })?;

    let re_flake = regex::Regex::new(r#""flake"\s*:\s*"([^"]+)""#)
        .map_err(|e| Error::Config(format!("flake regex: {e}")))?;
    let flake = re_flake.captures(text).map(|c| c[1].to_string());

    Ok(HydraEval {
        id,
        flake,
        jobsetevalinputs: None,
    })
}

/// Full-fetch fallback: downloads the entire eval response and deserialises
/// it with serde.  Populates `jobsetevalinputs` so that `extract_eval_rev`
/// can look up a named flake input revision.
async fn full_fetch_eval(client: &Client, url: &str) -> Result<HydraEval> {
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;

    resp.json::<HydraEval>().await.map_err(Error::Http)
}

/// Extract revision from a Hydra evaluation.
#[must_use]
pub fn extract_eval_rev(cfg: &PinConfig, eval: &HydraEval) -> Option<String> {
    if cfg.hydra_rev_input == "flake" || eval.jobsetevalinputs.is_none() {
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
        assert_eq!(result.status, HydraStatus::NotOnHydra);
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
        // Bounded-read path does not download jobsetevalinputs
        assert!(eval.jobsetevalinputs.is_none());
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
    async fn test_fetch_eval_fallback_when_flake_outside_bound() {
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
        assert!(out.test_buffer().iter().any(|l| l.contains("Warning")));
    }
}
