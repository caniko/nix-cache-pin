//! Wheel-pin resolution against a mock PEP 503 index: PEP 658 metadata, the HTTP range
//! fallback over a real ZIP, dependency closure, conflicts and atomic sidecar updates.

use nix_cache_pin_lib::wheel_pins::{
    read_sidecar, resolve, update, verify, UpdateStatus, WheelPinConfig,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Minimal single-entry ZIP (deflate) containing `<dist>.dist-info/METADATA`.
fn wheel_zip(dist: &str, metadata: &str) -> Vec<u8> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(metadata.as_bytes()).unwrap();
    let data = enc.finish().unwrap();
    let crc = flate2::Crc::new();
    let mut crc = crc;
    crc.update(metadata.as_bytes());
    let name = format!("{dist}.dist-info/METADATA");
    let mut z = Vec::new();
    // filler entry bytes before the local header, so offsets are non-trivial
    z.extend_from_slice(&[0u8; 100]);
    let local = z.len() as u32;
    z.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
    z.extend_from_slice(&crc.sum().to_le_bytes());
    z.extend_from_slice(&(data.len() as u32).to_le_bytes());
    z.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    z.extend_from_slice(&(name.len() as u16).to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z.extend_from_slice(name.as_bytes());
    z.extend_from_slice(&data);
    let cd_start = z.len() as u32;
    z.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
    z.extend_from_slice(&crc.sum().to_le_bytes());
    z.extend_from_slice(&(data.len() as u32).to_le_bytes());
    z.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    z.extend_from_slice(&(name.len() as u16).to_le_bytes());
    z.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    z.extend_from_slice(&local.to_le_bytes());
    z.extend_from_slice(name.as_bytes());
    let cd_size = z.len() as u32 - cd_start;
    z.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 1, 0, 1, 0]);
    z.extend_from_slice(&cd_size.to_le_bytes());
    z.extend_from_slice(&cd_start.to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z
}

/// Serves `Range: bytes=a-b` and `bytes=-n` with 206 responses.
struct RangeServer(Vec<u8>);

impl Respond for RangeServer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let total = self.0.len();
        let Some(range) = request.headers.get("range").and_then(|v| v.to_str().ok()) else {
            return ResponseTemplate::new(200).set_body_bytes(self.0.clone());
        };
        let spec = range.trim_start_matches("bytes=");
        let (start, end) = if let Some(n) = spec.strip_prefix('-') {
            let n: usize = n.parse().unwrap();
            (total.saturating_sub(n), total - 1)
        } else {
            let (a, b) = spec.split_once('-').unwrap();
            (
                a.parse().unwrap(),
                b.parse::<usize>().unwrap().min(total - 1),
            )
        };
        ResponseTemplate::new(206)
            .insert_header("content-range", format!("bytes {start}-{end}/{total}"))
            .set_body_bytes(self.0[start..=end].to_vec())
    }
}

const FAKE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn anchor(file: &str, extra: &str) -> String {
    format!(
        r#"<a href="../../files/{}#sha256={FAKE_SHA}"{extra}>{file}</a>"#,
        file.replace('+', "%2B")
    )
}

async fn index(server: &MockServer, project: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn config(server: &MockServer, dir: &std::path::Path) -> WheelPinConfig {
    WheelPinConfig {
        name: "torch-rocm".into(),
        indexes: vec![format!("{}/simple", server.uri())],
        packages: BTreeMap::from([("torch".into(), "==2.14.1".into())]),
        local_version: Some("rocm7.2".into()),
        python_version: "3.14".into(),
        platform_tags: vec!["manylinux_2_28_x86_64".into(), "linux_x86_64".into()],
        include_requires: true,
        provided_externally: BTreeSet::from(["filelock".into()]),
        output_file: dir.join("wheels.json"),
    }
}

async fn standard_index(server: &MockServer) {
    let torch_meta = "Metadata-Version: 2.1\nName: torch\nVersion: 2.14.1+rocm7.2\nRequires-Dist: filelock\nRequires-Dist: triton-rocm==3.8.0; platform_system == \"Linux\"\nRequires-Dist: pywin32; sys_platform == \"win32\"\n\nlong description";
    let torch = "torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl";
    // PEP 658 metadata with an advertised hash for torch
    let meta_sha = hex(&Sha256::digest(torch_meta.as_bytes()));
    index(
        server,
        "torch",
        [
            anchor(
                torch,
                &format!(r#" data-core-metadata="sha256={meta_sha}""#),
            ),
            anchor(
                "torch-2.14.1+rocm7.1-cp314-cp314-manylinux_2_28_x86_64.whl",
                "",
            ),
            anchor(
                "torch-2.15.0+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl",
                "",
            ),
            anchor(
                "torch-2.14.1+rocm7.2-cp313-cp313-manylinux_2_28_x86_64.whl",
                "",
            ),
        ]
        .concat(),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/files/{}.metadata",
            torch.replace('+', "%2B")
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(torch_meta))
        .mount(server)
        .await;
    // triton-rocm: no PEP 658 metadata, read from the wheel by ranges
    let triton = "triton_rocm-3.8.0-cp314-cp314-linux_x86_64.whl";
    index(server, "triton-rocm", anchor(triton, "")).await;
    let zip = wheel_zip(
        "triton_rocm-3.8.0",
        "Metadata-Version: 2.1\nName: triton-rocm\nVersion: 3.8.0\n\n",
    );
    Mock::given(method("GET"))
        .and(path(format!("/files/{triton}")))
        .respond_with(RangeServer(zip))
        .mount(server)
        .await;
}

#[tokio::test]
async fn resolves_closure_via_pep658_and_range_reads() {
    let server = MockServer::start().await;
    standard_index(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&server, dir.path());
    let sidecar = resolve(&cfg, reqwest::Client::new()).await.unwrap();
    let versions: Vec<_> = sidecar
        .wheels
        .iter()
        .map(|w| (w.pname.as_str(), w.version.as_str()))
        .collect();
    assert_eq!(
        versions,
        vec![("torch", "2.14.1+rocm7.2"), ("triton-rocm", "3.8.0")]
    );
    assert_eq!(sidecar.requires["torch"], vec!["filelock", "triton-rocm"]);
    assert!(sidecar.wheels[0]
        .url
        .ends_with("/files/torch-2.14.1%2Brocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl"));
    verify(&cfg, &sidecar).unwrap();
}

#[tokio::test]
async fn update_is_atomic_and_idempotent() {
    let server = MockServer::start().await;
    standard_index(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&server, dir.path());
    let dry = update(&cfg, reqwest::Client::new(), true).await.unwrap();
    assert!(matches!(dry, UpdateStatus::WouldUpdate(ref c) if c.len() == 2));
    assert!(!cfg.output_file.exists(), "dry run must not write");
    assert!(matches!(
        update(&cfg, reqwest::Client::new(), false).await.unwrap(),
        UpdateStatus::Updated(_)
    ));
    let written = std::fs::read(&cfg.output_file).unwrap();
    assert_eq!(
        update(&cfg, reqwest::Client::new(), false).await.unwrap(),
        UpdateStatus::Current
    );
    assert_eq!(std::fs::read(&cfg.output_file).unwrap(), written);
    assert!(read_sidecar(&cfg.output_file).unwrap().is_some());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn unpinnable_dependency_fails_with_its_name() {
    let server = MockServer::start().await;
    standard_index(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&server, dir.path());
    cfg.provided_externally.clear(); // filelock is not on the mock index
    let err = resolve(&cfg, reqwest::Client::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("filelock"), "{err}");
}

#[tokio::test]
async fn conflicting_pins_are_reported() {
    let server = MockServer::start().await;
    standard_index(&server).await;
    let vision = "torchvision-0.29.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl";
    index(
        &server,
        "torchvision",
        anchor(vision, r#" data-core-metadata="true""#),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/files/{}.metadata",
            vision.replace('+', "%2B")
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("Name: torchvision\nRequires-Dist: triton-rocm==3.7.0\n\n"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&server, dir.path());
    cfg.packages.insert("torchvision".into(), "==0.29.1".into());
    let err = resolve(&cfg, reqwest::Client::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("conflict") && err.contains("triton-rocm"),
        "{err}"
    );
}

#[tokio::test]
async fn tampered_pep658_metadata_is_rejected() {
    let server = MockServer::start().await;
    let torch = "torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl";
    index(
        &server,
        "torch",
        anchor(
            torch,
            &format!(r#" data-dist-info-metadata="sha256={FAKE_SHA}""#),
        ),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/files/{}.metadata",
            torch.replace('+', "%2B")
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string("Name: torch\n\n"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = resolve(&config(&server, dir.path()), reqwest::Client::new())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("advertised"), "{err:#}");
}
