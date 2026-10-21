//! Wheel pins: resolve prebuilt Python wheels (e.g. PyTorch ROCm/XPU builds) from PEP 503
//! simple indexes to exact files and sha256 hashes, and keep them in a checked-in JSON sidecar.
//!
//! Selection is deliberately simple and auditable: declared packages take the newest version that
//! satisfies their PEP 440 specifier (and local label, e.g. `+rocm7.2`); their `Requires-Dist`
//! closure is read from PEP 658 metadata and must be pinned too, unless the consumer supplies a
//! dependency itself (`providedExternally`). There is no backtracking resolver: a conflict is
//! reported, never silently solved.

use anyhow::{anyhow, bail, Context, Result};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, MarkerEnvironmentBuilder, Requirement, VersionOrUrl};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

pub const SIDECAR_SCHEMA_VERSION: u32 = 1;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound for a wheel's ZIP central directory read over HTTP ranges.
const MAX_CENTRAL_DIRECTORY_BYTES: usize = 64 * 1024 * 1024;
const ATTEMPTS: usize = 3;

/// One `cache-pin.wheel-pins.<name>` declaration (written as JSON by the flake module).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WheelPinConfig {
    pub name: String,
    /// PEP 503 simple-index base URLs, searched in order (first index listing a project wins).
    pub indexes: Vec<String>,
    /// Declared packages and their PEP 440 specifiers (`""` means any version).
    pub packages: BTreeMap<String, String>,
    /// Required local version label for declared packages, e.g. `rocm7.2` or `xpu`.
    #[serde(default)]
    pub local_version: Option<String>,
    /// CPython version used for `Requires-Python` and markers, e.g. `3.14`.
    pub python_version: String,
    /// Accepted platform tags, most preferred first (`any` is always accepted).
    pub platform_tags: Vec<String>,
    #[serde(default = "default_true")]
    pub include_requires: bool,
    /// Dependencies the consumer provides (e.g. from nixpkgs); never pinned here.
    #[serde(default)]
    pub provided_externally: BTreeSet<String>,
    pub output_file: PathBuf,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinnedWheel {
    pub pname: String,
    pub version: String,
    pub filename: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sidecar {
    pub schema_version: u32,
    pub name: String,
    pub python_version: String,
    pub local_version: Option<String>,
    pub wheels: Vec<PinnedWheel>,
    /// Runtime requirements per pinned project (normalized names), for audit and verification.
    pub requires: BTreeMap<String, Vec<String>>,
    pub provided_externally: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    Current,
    WouldUpdate(Vec<Change>),
    Updated(Vec<Change>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub pname: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// PEP 503 name normalization.
pub fn normalize(name: &str) -> String {
    static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[-_.]+").unwrap());
    SEP.replace_all(&name.to_ascii_lowercase(), "-")
        .into_owned()
}

#[derive(Debug, Clone)]
struct IndexFile {
    filename: String,
    url: String,
    sha256: String,
    requires_python: Option<String>,
    metadata_sha256: Option<Option<String>>, // Some(None): metadata available without a hash
}

#[derive(Debug, Clone)]
struct WheelName {
    project: String,
    version: Version,
    python: Vec<String>,
    abi: Vec<String>,
    platform: Vec<String>,
}

fn parse_wheel_filename(filename: &str) -> Option<WheelName> {
    let stem = filename.strip_suffix(".whl")?;
    let parts: Vec<&str> = stem.split('-').collect();
    let (name, version, rest) = match parts.len() {
        5 => (parts[0], parts[1], &parts[2..]),
        6 => (parts[0], parts[1], &parts[3..]), // build tag
        _ => return None,
    };
    let split = |s: &str| s.split('.').map(str::to_owned).collect::<Vec<_>>();
    Some(WheelName {
        project: normalize(name),
        version: Version::from_str(version).ok()?,
        python: split(rest[0]),
        abi: split(rest[1]),
        platform: split(rest[2]),
    })
}

impl WheelPinConfig {
    fn cp_tag(&self) -> Result<String> {
        let (major, minor) = self.python_version.split_once('.').ok_or_else(|| {
            anyhow!(
                "pythonVersion must be MAJOR.MINOR, got {:?}",
                self.python_version
            )
        })?;
        Ok(format!("cp{major}{minor}"))
    }

    /// Lower is better; `None` means incompatible.
    fn tag_rank(&self, wheel: &WheelName) -> Result<Option<usize>> {
        let cp = self.cp_tag()?;
        let major = cp[2..3].to_owned();
        let py_ok = wheel
            .python
            .iter()
            .any(|t| t == &cp || t == &format!("py{major}") || t == &format!("py{}", &cp[2..]));
        let abi_ok = wheel
            .abi
            .iter()
            .any(|t| t == &cp || t == "abi3" || t == "none");
        if !py_ok || !abi_ok {
            return Ok(None);
        }
        let plat = wheel
            .platform
            .iter()
            .filter_map(|t| {
                if t == "any" {
                    Some(self.platform_tags.len())
                } else {
                    self.platform_tags.iter().position(|p| p == t)
                }
            })
            .min();
        // Prefer exact CPython ABI over abi3 over none at equal platform rank.
        let abi = if wheel.abi.contains(&cp) {
            0
        } else if wheel.abi.iter().any(|t| t == "abi3") {
            1
        } else {
            2
        };
        Ok(plat.map(|p| p * 3 + abi))
    }

    fn marker_env(&self) -> Result<MarkerEnvironment> {
        let full = format!("{}.0", self.python_version);
        MarkerEnvironmentBuilder {
            implementation_name: "cpython",
            implementation_version: &full,
            os_name: "posix",
            platform_machine: "x86_64",
            platform_python_implementation: "CPython",
            platform_release: "",
            platform_system: "Linux",
            platform_version: "",
            python_full_version: &full,
            python_version: &self.python_version,
            sys_platform: "linux",
        }
        .try_into()
        .map_err(|e| anyhow!("marker environment: {e}"))
    }

    fn python_full(&self) -> Result<Version> {
        Version::from_str(&format!("{}.0", self.python_version))
            .map_err(|e| anyhow!("pythonVersion: {e}"))
    }
}

fn decode_html(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#43;", "+")
}

fn parse_index(page_url: &reqwest::Url, html: &str) -> Result<Vec<IndexFile>> {
    static ANCHOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)<a\s([^>]*)>(.*?)</a>"#).unwrap());
    static ATTR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*=\s*"([^"]*)""#).unwrap());
    let mut files = Vec::new();
    for anchor in ANCHOR.captures_iter(html) {
        let attrs: BTreeMap<String, String> = ATTR
            .captures_iter(&anchor[1])
            .map(|c| (c[1].to_ascii_lowercase(), decode_html(&c[2])))
            .collect();
        if attrs.contains_key("data-yanked") {
            continue;
        }
        let Some(href) = attrs.get("href") else {
            continue;
        };
        let mut url = page_url
            .join(href)
            .with_context(|| format!("resolve {href}"))?;
        let fragment = url.fragment().map(str::to_owned);
        url.set_fragment(None);
        let filename = decode_html(anchor[2].trim());
        if !filename.ends_with(".whl") {
            continue;
        }
        let Some(sha256) = fragment.as_deref().and_then(|f| f.strip_prefix("sha256=")) else {
            continue; // unhashed files are never pinned
        };
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let metadata = attrs
            .get("data-core-metadata")
            .or_else(|| attrs.get("data-dist-info-metadata"))
            .filter(|v| v.as_str() != "false")
            .map(|v| v.strip_prefix("sha256=").map(|h| h.to_ascii_lowercase()));
        files.push(IndexFile {
            filename,
            url: url.to_string(),
            sha256: sha256.to_ascii_lowercase(),
            requires_python: attrs.get("data-requires-python").cloned(),
            metadata_sha256: metadata,
        });
    }
    Ok(files)
}

struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    async fn get_bounded(&self, url: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
            }
            let response = match self
                .client
                .get(url)
                .header("Accept", "text/html")
                .timeout(Duration::from_secs(60))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last = Some(anyhow!(e));
                    continue;
                }
            };
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                last = Some(anyhow!("{url}: HTTP {status}"));
                continue;
            }
            if !status.is_success() {
                bail!("{url}: HTTP {status}");
            }
            if response.content_length().is_some_and(|n| n > limit as u64) {
                bail!("{url}: response larger than {limit} bytes");
            }
            let mut body = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                if chunk.len() > limit - body.len() {
                    bail!("{url}: response larger than {limit} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(Some(body));
        }
        Err(last.unwrap_or_else(|| anyhow!("{url}: request failed")))
    }

    /// `GET` with `Range: bytes=<range>`; the server must answer 206.
    async fn get_range(
        &self,
        url: &str,
        range: &str,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<u64>)> {
        let mut response = self
            .client
            .get(url)
            .header("Range", format!("bytes={range}"))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .with_context(|| format!("{url}: range {range}"))?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            bail!(
                "{url}: server does not honour byte ranges (HTTP {})",
                response.status()
            );
        }
        let total = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok());
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if chunk.len() > limit - body.len() {
                bail!("{url}: range response larger than {limit} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, total))
    }

    /// Read `*.dist-info/METADATA` straight out of a remote wheel with range requests
    /// (end-of-central-directory, ZIP64 when needed, central directory, one entry).
    async fn metadata_by_range(&self, url: &str) -> Result<Vec<u8>> {
        let (tail, total) = self.get_range(url, "-65558", 65558).await?;
        let total = total.ok_or_else(|| anyhow!("{url}: no Content-Range total"))?;
        let tail_start = total - tail.len() as u64;
        let eocd = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
            .ok_or_else(|| anyhow!("{url}: no ZIP end-of-central-directory record"))?;
        let mut cd_size = u64::from(le32(&tail, eocd + 12)?);
        let mut cd_offset = u64::from(le32(&tail, eocd + 16)?);
        if cd_size == 0xffff_ffff || cd_offset == 0xffff_ffff {
            let locator = eocd
                .checked_sub(20)
                .ok_or_else(|| anyhow!("{url}: truncated ZIP64 locator"))?;
            if tail[locator..locator + 4] != [0x50, 0x4b, 0x06, 0x07] {
                bail!("{url}: ZIP64 locator missing");
            }
            let record = le64(&tail, locator + 8)?;
            let bytes = if record >= tail_start {
                tail[(record - tail_start) as usize..].to_vec()
            } else {
                self.get_range(url, &format!("{record}-{}", record + 55), 56)
                    .await?
                    .0
            };
            if bytes.len() < 56 || bytes[..4] != [0x50, 0x4b, 0x06, 0x06] {
                bail!("{url}: invalid ZIP64 end-of-central-directory record");
            }
            cd_size = le64(&bytes, 40)?;
            cd_offset = le64(&bytes, 48)?;
        }
        if cd_size as usize > MAX_CENTRAL_DIRECTORY_BYTES {
            bail!("{url}: central directory of {cd_size} bytes exceeds the limit");
        }
        let cd = if cd_offset >= tail_start {
            tail[(cd_offset - tail_start) as usize..][..cd_size as usize].to_vec()
        } else {
            self.get_range(
                url,
                &format!("{cd_offset}-{}", cd_offset + cd_size - 1),
                MAX_CENTRAL_DIRECTORY_BYTES,
            )
            .await?
            .0
        };
        let entry = find_metadata_entry(&cd).with_context(|| format!("{url}: METADATA entry"))?;
        let header = self
            .get_range(url, &format!("{}-{}", entry.offset, entry.offset + 29), 30)
            .await?
            .0;
        if header.len() < 30 || header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            bail!("{url}: invalid local file header");
        }
        let data_start =
            entry.offset + 30 + u64::from(le16(&header, 26)?) + u64::from(le16(&header, 28)?);
        if entry.compressed > MAX_METADATA_BYTES as u64 {
            bail!("{url}: METADATA entry too large");
        }
        let raw = self
            .get_range(
                url,
                &format!("{data_start}-{}", data_start + entry.compressed.max(1) - 1),
                MAX_METADATA_BYTES,
            )
            .await?
            .0;
        match entry.method {
            0 => Ok(raw),
            8 => {
                use std::io::Read;
                let mut out = Vec::new();
                flate2::read::DeflateDecoder::new(&raw[..])
                    .take(MAX_METADATA_BYTES as u64 + 1)
                    .read_to_end(&mut out)?;
                if out.len() > MAX_METADATA_BYTES {
                    bail!("{url}: METADATA larger than {MAX_METADATA_BYTES} bytes");
                }
                Ok(out)
            }
            m => bail!("{url}: unsupported ZIP compression method {m}"),
        }
    }

    async fn project_files(
        &self,
        config: &WheelPinConfig,
        project: &str,
    ) -> Result<Vec<IndexFile>> {
        for index in &config.indexes {
            let page = format!("{}/{}/", index.trim_end_matches('/'), project);
            let page_url =
                reqwest::Url::parse(&page).with_context(|| format!("index URL {page}"))?;
            if let Some(body) = self.get_bounded(&page, MAX_INDEX_BYTES).await? {
                let html = String::from_utf8(body).with_context(|| format!("{page}: not UTF-8"))?;
                let files = parse_index(&page_url, &html)?;
                if !files.is_empty() {
                    return Ok(files);
                }
            }
        }
        bail!("no hashed wheels for {project} on {:?}", config.indexes)
    }

    async fn requires(&self, file: &IndexFile, project: &str) -> Result<Vec<Requirement>> {
        let Some(expected) = &file.metadata_sha256 else {
            // No PEP 658 metadata (e.g. download.pytorch.org): read METADATA from the wheel by
            // range requests. Only dependency discovery uses it; Nix verifies the wheel sha256.
            let body = self
                .metadata_by_range(&file.url)
                .await
                .with_context(|| format!("{project}: no PEP 658 metadata and range read failed"))?;
            return parse_requires_dist(&String::from_utf8_lossy(&body));
        };
        let url = format!("{}.metadata", file.url);
        let body = self
            .get_bounded(&url, MAX_METADATA_BYTES)
            .await?
            .ok_or_else(|| anyhow!("{url}: metadata advertised but missing"))?;
        if let Some(expected) = expected {
            let actual = hex(&Sha256::digest(&body));
            if &actual != expected {
                bail!("{url}: metadata sha256 {actual} != advertised {expected}");
            }
        }
        parse_requires_dist(&String::from_utf8_lossy(&body))
    }
}

struct ZipEntry {
    method: u16,
    compressed: u64,
    offset: u64,
}

fn le16(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2)
            .ok_or_else(|| anyhow!("truncated ZIP data"))?
            .try_into()?,
    ))
}

fn le32(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or_else(|| anyhow!("truncated ZIP data"))?
            .try_into()?,
    ))
}

fn le64(b: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        b.get(at..at + 8)
            .ok_or_else(|| anyhow!("truncated ZIP data"))?
            .try_into()?,
    ))
}

/// Locate the top-level `<name>.dist-info/METADATA` entry in a ZIP central directory.
fn find_metadata_entry(cd: &[u8]) -> Result<ZipEntry> {
    let mut at = 0;
    while at + 46 <= cd.len() {
        if cd[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            bail!("corrupt central directory at {at}");
        }
        let method = le16(cd, at + 10)?;
        let mut compressed = u64::from(le32(cd, at + 20)?);
        let uncompressed = le32(cd, at + 24)?;
        let name_len = usize::from(le16(cd, at + 28)?);
        let extra_len = usize::from(le16(cd, at + 30)?);
        let comment_len = usize::from(le16(cd, at + 32)?);
        let mut offset = u64::from(le32(cd, at + 42)?);
        let name = cd
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| anyhow!("truncated entry name"))?;
        let extra = cd
            .get(at + 46 + name_len..at + 46 + name_len + extra_len)
            .ok_or_else(|| anyhow!("truncated extra field"))?;
        let name = String::from_utf8_lossy(name);
        let is_metadata = name
            .strip_suffix("/METADATA")
            .is_some_and(|dir| dir.ends_with(".dist-info") && !dir.contains('/'));
        if is_metadata {
            // ZIP64 extended information holds whichever 32-bit fields are saturated, in order.
            let mut e = 0;
            while e + 4 <= extra.len() {
                let id = le16(extra, e)?;
                let size = usize::from(le16(extra, e + 2)?);
                if id == 0x0001 {
                    let mut p = e + 4;
                    if uncompressed == 0xffff_ffff {
                        p += 8;
                    }
                    if compressed == 0xffff_ffff {
                        compressed = le64(extra, p)?;
                        p += 8;
                    }
                    if offset == 0xffff_ffff {
                        offset = le64(extra, p)?;
                    }
                }
                e += 4 + size;
            }
            return Ok(ZipEntry {
                method,
                compressed,
                offset,
            });
        }
        at += 46 + name_len + extra_len + comment_len;
    }
    bail!("no top-level *.dist-info/METADATA entry")
}

/// Wheels are fetched over https; plain http is accepted only for loopback mirrors.
fn secure_url(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(u) if u.scheme() == "https" => true,
        Ok(u) if u.scheme() == "http" => u.host_str().is_some_and(|h| {
            h == "localhost"
                || h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_requires_dist(metadata: &str) -> Result<Vec<Requirement>> {
    let mut out = Vec::new();
    for line in metadata.lines() {
        if line.is_empty() {
            break; // end of headers; the body is the long description
        }
        if let Some(value) = line.strip_prefix("Requires-Dist:") {
            out.push(
                Requirement::from_str(value.trim())
                    .map_err(|e| anyhow!("Requires-Dist {value:?}: {e}"))?,
            );
        }
    }
    Ok(out)
}

fn local_label(version: &Version) -> Option<String> {
    let local = version.local();
    (!local.is_empty()).then(|| {
        local
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(".")
    })
}

fn select<'a>(
    config: &WheelPinConfig,
    project: &str,
    specifiers: &VersionSpecifiers,
    require_local: Option<&str>,
    files: &'a [IndexFile],
) -> Result<(&'a IndexFile, Version)> {
    let python = config.python_full()?;
    let mut best: Option<(&IndexFile, Version, usize)> = None;
    for file in files {
        let Some(wheel) = parse_wheel_filename(&file.filename) else {
            continue;
        };
        if wheel.project != project {
            continue;
        }
        if !specifiers.contains(&wheel.version) {
            continue;
        }
        if local_label(&wheel.version).as_deref() != require_local {
            continue;
        }
        if let Some(rp) = &file.requires_python {
            if let Ok(spec) = VersionSpecifiers::from_str(rp) {
                if !spec.contains(&python) {
                    continue;
                }
            }
        }
        let Some(rank) = config.tag_rank(&wheel)? else {
            continue;
        };
        let better = match &best {
            None => true,
            Some((_, v, r)) => wheel.version > *v || (wheel.version == *v && rank < *r),
        };
        if better {
            best = Some((file, wheel.version.clone(), rank));
        }
    }
    best.map(|(f, v, _)| (f, v)).ok_or_else(|| {
        anyhow!(
            "no compatible wheel for {project} {specifiers}{} (cp{} / {:?})",
            require_local.map(|l| format!(" +{l}")).unwrap_or_default(),
            config.python_version.replace('.', ""),
            config.platform_tags
        )
    })
}

/// Resolve the full pin set from the indexes.
pub async fn resolve(config: &WheelPinConfig, client: reqwest::Client) -> Result<Sidecar> {
    let fetcher = Fetcher { client };
    let env = config.marker_env()?;
    let external: BTreeSet<String> = config
        .provided_externally
        .iter()
        .map(|n| normalize(n))
        .collect();
    let mut queue: VecDeque<(String, VersionSpecifiers, bool, String)> = VecDeque::new();
    for (name, spec) in &config.packages {
        let specifiers = VersionSpecifiers::from_str(spec)
            .map_err(|e| anyhow!("{name}: specifier {spec:?}: {e}"))?;
        queue.push_back((normalize(name), specifiers, true, "declared".into()));
    }
    let mut pinned: BTreeMap<String, (PinnedWheel, Version)> = BTreeMap::new();
    let mut requires: BTreeMap<String, Vec<String>> = BTreeMap::new();
    while let Some((project, specifiers, declared, origin)) = queue.pop_front() {
        if let Some((_, version)) = pinned.get(&project) {
            if !specifiers.contains(version) {
                bail!(
                    "conflict: {project} {version} is pinned, but {origin} requires {specifiers}"
                );
            }
            continue;
        }
        let files = fetcher.project_files(config, &project).await?;
        let require_local = if declared {
            config.local_version.as_deref()
        } else {
            None
        };
        let (file, version) = select(config, &project, &specifiers, require_local, &files)?;
        let mut deps = BTreeSet::new();
        if config.include_requires {
            for req in fetcher.requires(file, &project).await? {
                if !req.evaluate_markers(&env, &[]) {
                    continue;
                }
                let dep = normalize(req.name.as_ref());
                deps.insert(dep.clone());
                if external.contains(&dep) {
                    continue;
                }
                let spec = match req.version_or_url {
                    None => VersionSpecifiers::empty(),
                    Some(VersionOrUrl::VersionSpecifier(s)) => s,
                    Some(VersionOrUrl::Url(_)) => {
                        bail!("{project} requires {dep} by direct URL; not pinnable")
                    }
                };
                queue.push_back((dep, spec, false, format!("{project} {version}")));
            }
        }
        requires.insert(project.clone(), deps.into_iter().collect());
        pinned.insert(
            project.clone(),
            (
                PinnedWheel {
                    pname: project,
                    version: version.to_string(),
                    filename: file.filename.clone(),
                    url: file.url.clone(),
                    sha256: file.sha256.clone(),
                },
                version,
            ),
        );
    }
    Ok(Sidecar {
        schema_version: SIDECAR_SCHEMA_VERSION,
        name: config.name.clone(),
        python_version: config.python_version.clone(),
        local_version: config.local_version.clone(),
        wheels: pinned.into_values().map(|(w, _)| w).collect(),
        requires,
        provided_externally: external.into_iter().collect(),
    })
}

/// Newest version of each declared package ignoring its specifier (same local label), so
/// `--check-current` can show upgrades the constraints are blocking.
pub async fn latest_available(
    config: &WheelPinConfig,
    client: reqwest::Client,
) -> Result<BTreeMap<String, String>> {
    let fetcher = Fetcher { client };
    let mut out = BTreeMap::new();
    for name in config.packages.keys() {
        let project = normalize(name);
        let files = fetcher.project_files(config, &project).await?;
        let (_, version) = select(
            config,
            &project,
            &VersionSpecifiers::empty(),
            config.local_version.as_deref(),
            &files,
        )?;
        out.insert(project, version.to_string());
    }
    Ok(out)
}

pub fn read_sidecar(path: &Path) -> Result<Option<Sidecar>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn render(sidecar: &Sidecar) -> Result<String> {
    Ok(serde_json::to_string_pretty(sidecar)? + "\n")
}

pub fn diff(old: Option<&Sidecar>, new: &Sidecar) -> Vec<Change> {
    let before: BTreeMap<&str, &str> = old
        .map(|s| {
            s.wheels
                .iter()
                .map(|w| (w.pname.as_str(), w.version.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let after: BTreeMap<&str, &str> = new
        .wheels
        .iter()
        .map(|w| (w.pname.as_str(), w.version.as_str()))
        .collect();
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| Change {
            pname: (*k).to_owned(),
            from: before.get(*k).map(|s| (*s).to_owned()),
            to: after.get(*k).map(|s| (*s).to_owned()),
        })
        .collect()
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wheel-pins");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| format!("rename to {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Resolve and write (unless `dry_run`). Byte-identical output is reported as current.
pub async fn update(
    config: &WheelPinConfig,
    client: reqwest::Client,
    dry_run: bool,
) -> Result<UpdateStatus> {
    let anchor = config
        .output_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let _guard = if dry_run {
        None
    } else {
        // First run: the sidecar's directory may not exist yet.
        fs::create_dir_all(anchor).with_context(|| format!("create {}", anchor.display()))?;
        Some(crate::mutation::Mutation::acquire(anchor).with_context(|| {
            format!(
                "acquire the cache-pin mutation lock for {}",
                anchor.display()
            )
        })?)
    };
    let before = crate::mutation::read(&config.output_file)?;
    let old = read_sidecar(&config.output_file)?;
    let new = resolve(config, client).await?;
    verify(config, &new)?;
    let rendered = render(&new)?;
    if before.as_deref() == Some(rendered.as_bytes()) {
        return Ok(UpdateStatus::Current);
    }
    let changes = diff(old.as_ref(), &new);
    if dry_run {
        return Ok(UpdateStatus::WouldUpdate(changes));
    }
    crate::mutation::unchanged(&config.output_file, &before)?;
    write_atomic(&config.output_file, &rendered)?;
    Ok(UpdateStatus::Updated(changes))
}

/// Offline verification that a sidecar still satisfies its declaration.
pub fn verify(config: &WheelPinConfig, sidecar: &Sidecar) -> Result<()> {
    let mut problems = Vec::new();
    if sidecar.schema_version != SIDECAR_SCHEMA_VERSION {
        problems.push(format!(
            "schemaVersion {} (expected {SIDECAR_SCHEMA_VERSION})",
            sidecar.schema_version
        ));
    }
    if sidecar.python_version != config.python_version
        || sidecar.local_version != config.local_version
    {
        problems.push("pythonVersion/localVersion differ from the declaration".into());
    }
    let external: BTreeSet<String> = config
        .provided_externally
        .iter()
        .map(|n| normalize(n))
        .collect();
    let by_name: BTreeMap<&str, &PinnedWheel> = sidecar
        .wheels
        .iter()
        .map(|w| (w.pname.as_str(), w))
        .collect();
    for (name, spec) in &config.packages {
        let project = normalize(name);
        let Some(wheel) = by_name.get(project.as_str()) else {
            problems.push(format!("declared package {project} is not pinned"));
            continue;
        };
        let version = Version::from_str(&wheel.version).map_err(|e| anyhow!("{project}: {e}"))?;
        let specifiers =
            VersionSpecifiers::from_str(spec).map_err(|e| anyhow!("{project}: {e}"))?;
        if !specifiers.contains(&version) {
            problems.push(format!("{project} {version} does not satisfy {spec}"));
        }
        if local_label(&version) != config.local_version {
            problems.push(format!(
                "{project} {version} lacks local label {:?}",
                config.local_version
            ));
        }
    }
    for wheel in &sidecar.wheels {
        match parse_wheel_filename(&wheel.filename) {
            Some(w) if w.project == wheel.pname && w.version.to_string() == wheel.version => {
                if config.tag_rank(&w)?.is_none() {
                    problems.push(format!(
                        "{} is not compatible with cp{} {:?}",
                        wheel.filename,
                        config.python_version.replace('.', ""),
                        config.platform_tags
                    ));
                }
            }
            _ => problems.push(format!(
                "{}: filename does not match {} {}",
                wheel.filename, wheel.pname, wheel.version
            )),
        }
        if wheel.sha256.len() != 64 || !wheel.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            problems.push(format!("{}: invalid sha256", wheel.filename));
        }
        if !secure_url(&wheel.url) {
            problems.push(format!(
                "{}: URL must be https (plain http only to loopback)",
                wheel.filename
            ));
        }
    }
    for (project, deps) in &sidecar.requires {
        for dep in deps {
            if !by_name.contains_key(dep.as_str()) && !external.contains(dep) {
                problems.push(format!(
                    "{project} requires {dep}, which is neither pinned nor provided externally"
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        bail!(
            "wheel pin {} is not valid:\n  {}",
            config.name,
            problems.join("\n  ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WheelPinConfig {
        WheelPinConfig {
            name: "torch-rocm".into(),
            indexes: vec!["https://example.invalid/whl".into()],
            packages: BTreeMap::from([("torch".into(), "==2.14.1".into())]),
            local_version: Some("rocm7.2".into()),
            python_version: "3.14".into(),
            platform_tags: vec![
                "manylinux_2_28_x86_64".into(),
                "manylinux2014_x86_64".into(),
            ],
            include_requires: true,
            provided_externally: BTreeSet::from(["filelock".into()]),
            output_file: "wheels.json".into(),
        }
    }

    #[test]
    fn names_normalize_per_pep503() {
        assert_eq!(normalize("Intel_CMPLR.lib-rt"), "intel-cmplr-lib-rt");
    }

    #[test]
    fn wheel_filenames_and_tags() {
        let c = config();
        let w = parse_wheel_filename("torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl")
            .unwrap();
        assert_eq!(w.project, "torch");
        assert_eq!(local_label(&w.version).as_deref(), Some("rocm7.2"));
        assert_eq!(c.tag_rank(&w).unwrap(), Some(0));
        let any =
            parse_wheel_filename("tbb-2023.1.0-py2.py3-none-manylinux_2_28_x86_64.whl").unwrap();
        assert_eq!(c.tag_rank(&any).unwrap(), Some(2));
        let other =
            parse_wheel_filename("torch-2.14.1-cp313-cp313-manylinux_2_28_x86_64.whl").unwrap();
        assert_eq!(c.tag_rank(&other).unwrap(), None);
        let mac = parse_wheel_filename("torch-2.14.1-cp314-cp314-macosx_11_0_arm64.whl").unwrap();
        assert_eq!(c.tag_rank(&mac).unwrap(), None);
    }

    #[test]
    fn index_parsing_rejects_yanked_and_unhashed() {
        let url = reqwest::Url::parse("https://example.invalid/whl/torch/").unwrap();
        let html = concat!(
            r#"<a href="/whl/torch-2.14.1%2Brocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl#sha256=AAAA00000000000000000000000000000000000000000000000000000000aaaa" data-dist-info-metadata="sha256=bbbb000000000000000000000000000000000000000000000000000000000000">torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl</a>"#,
            r#"<a href="/whl/torch-2.14.0-cp314-cp314-manylinux_2_28_x86_64.whl" >torch-2.14.0-cp314-cp314-manylinux_2_28_x86_64.whl</a>"#,
            r#"<a href="/whl/torch-2.14.2-cp314-cp314-manylinux_2_28_x86_64.whl#sha256=cccc000000000000000000000000000000000000000000000000000000000000" data-yanked="">torch-2.14.2-cp314-cp314-manylinux_2_28_x86_64.whl</a>"#,
        );
        let files = parse_index(&url, html).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url, "https://example.invalid/whl/torch-2.14.1%2Brocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl");
        assert_eq!(
            files[0].sha256,
            "aaaa00000000000000000000000000000000000000000000000000000000aaaa"
        );
        assert!(matches!(files[0].metadata_sha256, Some(Some(_))));
    }

    #[test]
    fn selection_honours_specifier_local_label_and_requires_python() {
        let c = config();
        let mk = |name: &str, rp: Option<&str>| IndexFile {
            filename: name.into(),
            url: format!("https://example.invalid/{name}"),
            sha256: "0".repeat(64),
            requires_python: rp.map(str::to_owned),
            metadata_sha256: None,
        };
        let files = vec![
            mk(
                "torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl",
                None,
            ),
            mk(
                "torch-2.14.1+rocm7.1-cp314-cp314-manylinux_2_28_x86_64.whl",
                None,
            ),
            mk(
                "torch-2.15.0+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl",
                None,
            ),
            mk("torch-2.14.1-cp314-cp314-manylinux_2_28_x86_64.whl", None),
        ];
        let spec = VersionSpecifiers::from_str("==2.14.1").unwrap();
        let (f, v) = select(&c, "torch", &spec, Some("rocm7.2"), &files).unwrap();
        assert_eq!(v.to_string(), "2.14.1+rocm7.2");
        assert!(f.filename.contains("rocm7.2"));
        let any = VersionSpecifiers::from_str(">=2.14").unwrap();
        assert_eq!(
            select(&c, "torch", &any, Some("rocm7.2"), &files)
                .unwrap()
                .1
                .to_string(),
            "2.15.0+rocm7.2"
        );
        let old_python = vec![mk(
            "torch-2.15.0+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl",
            Some(">=3.15"),
        )];
        assert!(select(&c, "torch", &any, Some("rocm7.2"), &old_python).is_err());
    }

    #[test]
    fn requires_dist_markers_are_evaluated_for_linux_x86_64() {
        let c = config();
        let env = c.marker_env().unwrap();
        let reqs = parse_requires_dist(
            "Metadata-Version: 2.1\nName: torch\nRequires-Dist: filelock\nRequires-Dist: triton-rocm==3.8.0; platform_system == \"Linux\" and platform_machine == \"x86_64\"\nRequires-Dist: pywin32; sys_platform == \"win32\"\nRequires-Dist: opt-einsum>=3.3; extra == \"opt-einsum\"\n\nRequires-Dist: not-a-header\n",
        )
        .unwrap();
        let active: Vec<String> = reqs
            .iter()
            .filter(|r| r.evaluate_markers(&env, &[]))
            .map(|r| normalize(r.name.as_ref()))
            .collect();
        assert_eq!(active, vec!["filelock", "triton-rocm"]);
    }

    fn sidecar() -> Sidecar {
        Sidecar {
            schema_version: 1,
            name: "torch-rocm".into(),
            python_version: "3.14".into(),
            local_version: Some("rocm7.2".into()),
            wheels: vec![PinnedWheel {
                pname: "torch".into(),
                version: "2.14.1+rocm7.2".into(),
                filename: "torch-2.14.1+rocm7.2-cp314-cp314-manylinux_2_28_x86_64.whl".into(),
                url: "https://example.invalid/torch.whl".into(),
                sha256: "a".repeat(64),
            }],
            requires: BTreeMap::from([("torch".into(), vec!["filelock".into()])]),
            provided_externally: vec!["filelock".into()],
        }
    }

    #[test]
    fn verify_accepts_valid_and_rejects_drift() {
        let c = config();
        verify(&c, &sidecar()).unwrap();

        let mut s = sidecar();
        s.requires.insert(
            "torch".into(),
            vec!["filelock".into(), "triton-rocm".into()],
        );
        assert!(verify(&c, &s)
            .unwrap_err()
            .to_string()
            .contains("triton-rocm"));

        let mut c2 = config();
        c2.packages.insert("torch".into(), ">=2.15".into());
        assert!(verify(&c2, &sidecar())
            .unwrap_err()
            .to_string()
            .contains("does not satisfy"));

        let mut c3 = config();
        c3.packages.insert("torchvision".into(), "".into());
        assert!(verify(&c3, &sidecar())
            .unwrap_err()
            .to_string()
            .contains("not pinned"));

        let mut s = sidecar();
        s.wheels[0].url = "http://example.invalid/torch.whl".into();
        assert!(verify(&c, &s).is_err());
    }

    #[test]
    fn only_https_or_loopback_http_is_secure() {
        assert!(secure_url("https://download.pytorch.org/whl/x.whl"));
        assert!(secure_url("http://127.0.0.1:8080/x.whl"));
        assert!(secure_url("http://[::1]/x.whl"));
        assert!(secure_url("http://localhost/x.whl"));
        assert!(!secure_url("http://download.pytorch.org/x.whl"));
        assert!(!secure_url("http://10.0.0.1/x.whl"));
        assert!(!secure_url("ftp://example.invalid/x.whl"));
    }

    #[test]
    fn zip64_extra_field_overrides_saturated_sizes() {
        let name = b"torch-2.14.1.dist-info/METADATA";
        let mut cd = Vec::new();
        // a non-matching entry first
        let other = b"torch/lib/libtorch.so";
        cd.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        cd.extend_from_slice(&[0u8; 24]);
        cd.extend_from_slice(&(other.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 16]);
        cd.extend_from_slice(other);
        // the METADATA entry with ZIP64 extra: uncompressed, compressed, offset
        cd.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 8, 0]);
        cd.extend_from_slice(&[0u8; 8]);
        cd.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        cd.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&28u16.to_le_bytes());
        cd.extend_from_slice(&[0u8; 10]);
        cd.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        cd.extend_from_slice(name);
        cd.extend_from_slice(&1u16.to_le_bytes());
        cd.extend_from_slice(&24u16.to_le_bytes());
        cd.extend_from_slice(&5000u64.to_le_bytes());
        cd.extend_from_slice(&1234u64.to_le_bytes());
        cd.extend_from_slice(&6_000_000_000u64.to_le_bytes());
        let e = find_metadata_entry(&cd).unwrap();
        assert_eq!((e.method, e.compressed, e.offset), (8, 1234, 6_000_000_000));
    }

    #[test]
    fn diff_reports_version_changes() {
        let old = sidecar();
        let mut new = sidecar();
        new.wheels[0].version = "2.14.2+rocm7.2".into();
        let changes = diff(Some(&old), &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from.as_deref(), Some("2.14.1+rocm7.2"));
    }
}
