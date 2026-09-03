use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub struct SourcePinsOptions {
    pub name: String,
    pub lock_file: PathBuf,
    pub output_file: PathBuf,
    pub nix_bin: PathBuf,
    pub workers: usize,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    Current,
    WouldUpdate { added: usize, removed: usize },
    Updated { sources: usize },
}

pub fn update(options: &SourcePinsOptions) -> Result<UpdateStatus> {
    if options.workers == 0 {
        bail!("source-pin worker count must be greater than zero");
    }

    let raw = fs::read_to_string(&options.lock_file)
        .with_context(|| format!("read {}", options.lock_file.display()))?;
    let sources = parse_cargo_git_sources(&raw)?;
    let output_exists = options.output_file.is_file();
    let (added, removed) = source_delta(&options.output_file, &sources)?;

    if output_exists && added.is_empty() && removed.is_empty() {
        return Ok(UpdateStatus::Current);
    }
    if options.dry_run {
        return Ok(UpdateStatus::WouldUpdate {
            added: added.len(),
            removed: removed.len(),
        });
    }

    let hashes = prefetch_all(&sources, &options.nix_bin, options.workers)?;
    write_sidecar(&options.output_file, &options.name, &hashes)?;
    Ok(UpdateStatus::Updated {
        sources: hashes.len(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitHash {
    source: String,
    hash: String,
}

fn parse_cargo_git_sources(raw: &str) -> Result<Vec<String>> {
    let re = Regex::new(r#"(?m)^\s*source = "git\+(?P<source>[^"]+)""#)?;
    let mut sources = BTreeSet::new();
    for captures in re.captures_iter(raw) {
        let encoded = captures
            .name("source")
            .ok_or_else(|| anyhow!("missing source capture"))?
            .as_str();
        sources.insert(format!("git+{}", percent_decode(encoded)?));
    }
    Ok(sources.into_iter().collect())
}

fn source_delta(path: &Path, sources: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((sources.to_vec(), Vec::new()));
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };

    let present = parse_sidecar_sources(&raw)?;
    let expected: BTreeSet<String> = sources.iter().cloned().collect();
    Ok((
        expected.difference(&present).cloned().collect(),
        present.difference(&expected).cloned().collect(),
    ))
}

fn parse_sidecar_sources(raw: &str) -> Result<BTreeSet<String>> {
    let re = Regex::new(r#"(?m)^\s*"(?P<source>git\+[^"]+)"\s*="#)?;
    Ok(re
        .captures_iter(raw)
        .filter_map(|captures| {
            captures
                .name("source")
                .map(|source| source.as_str().to_string())
        })
        .collect())
}

fn prefetch_all(sources: &[String], nix_bin: &Path, workers: usize) -> Result<Vec<GitHash>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }

    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    let worker_count = workers.min(sources.len());

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next = &next;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(source) = sources.get(index) else {
                    break;
                };
                if sender
                    .send((index, prefetch_source(source, nix_bin)))
                    .is_err()
                {
                    break;
                }
            });
        }
        drop(sender);

        let mut results: Vec<_> = receiver.into_iter().collect();
        results.sort_by_key(|(index, _)| *index);

        let mut hashes = Vec::with_capacity(results.len());
        let mut failures = Vec::new();
        for (index, result) in results {
            match result {
                Ok(hash) => hashes.push(hash),
                Err(error) => failures.push(format!("{}: {error:#}", sources[index])),
            }
        }
        if failures.is_empty() {
            Ok(hashes)
        } else {
            bail!(
                "cargo git dependency prefetch failed:\n{}",
                failures.join("\n")
            )
        }
    })
}

fn prefetch_source(source: &str, nix_bin: &Path) -> Result<GitHash> {
    let fetch_url = prefetch_url(source)?;
    let output = Command::new(nix_bin)
        .args(["flake", "prefetch", &fetch_url])
        .output()
        .with_context(|| format!("spawn {} flake prefetch {fetch_url}", nix_bin.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "`{} flake prefetch {fetch_url}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
            nix_bin.display(),
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let hash = extract_prefetch_hash(&stdout)
        .or_else(|| extract_prefetch_hash(&stderr))
        .ok_or_else(|| anyhow!("nix flake prefetch output did not contain a hash"))?;

    Ok(GitHash {
        source: source.to_string(),
        hash,
    })
}

fn prefetch_url(source: &str) -> Result<String> {
    let (without_fragment, fragment) = source.split_once('#').unwrap_or((source, ""));
    let (remote, query) = without_fragment
        .split_once('?')
        .unwrap_or((without_fragment, ""));

    let mut rev = None;
    let mut reference = None;
    for parameter in query.split('&').filter(|parameter| !parameter.is_empty()) {
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        match key {
            "rev" => rev = Some(value),
            "ref" | "branch" | "tag" => reference = Some(value),
            _ => {}
        }
    }
    if !fragment.is_empty() {
        rev = Some(fragment);
    }

    let mut parameters = Vec::new();
    if let Some(reference) = reference {
        parameters.push(format!("ref={reference}"));
    }
    if let Some(rev) = rev {
        parameters.push(format!("rev={rev}"));
    }

    if parameters.is_empty() {
        Ok(remote.to_string())
    } else {
        Ok(format!("{remote}?{}", parameters.join("&")))
    }
}

fn extract_prefetch_hash(output: &str) -> Option<String> {
    let re = Regex::new(r#"hash '([^']+)'"#).ok()?;
    re.captures(output)
        .and_then(|captures| captures.get(1))
        .map(|hash| hash.as_str().to_string())
}

fn write_sidecar(path: &Path, name: &str, hashes: &[GitHash]) -> Result<()> {
    let mut output = String::new();
    output.push_str("# Generated by nix-cache-pin source-pins; do not edit manually.\n");
    output.push_str(&format!("# Source pin:  {}\n", nix_comment_escape(name)));
    output.push_str("{\n");
    for item in hashes {
        let short: String = item.source.chars().take(80).collect();
        output.push_str(&format!("  # {}\n", nix_comment_escape(&short)));
        output.push_str(&format!(
            "  \"{}\" = \"{}\";\n",
            nix_string_escape(&item.source),
            nix_string_escape(&item.hash)
        ));
    }
    output.push_str("}\n");

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("source-pins");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(output.as_bytes())
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("rename {} to {}", temporary.display(), path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("invalid percent escape in cargo git source: {input}");
            }
            let high = hex_value(bytes[index + 1])
                .ok_or_else(|| anyhow!("invalid percent escape in cargo git source: {input}"))?;
            let low = hex_value(bytes[index + 2])
                .ok_or_else(|| anyhow!("invalid percent escape in cargo git source: {input}"))?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).context("decoded cargo git source is not UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn nix_string_escape(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace("${", r"\${")
        .replace('"', "\\\"")
}

fn nix_comment_escape(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nix-cache-pin-source-pins-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn parses_and_decodes_cargo_git_sources() {
        let sources = parse_cargo_git_sources(
            r#"source = "git+https://github.com/stempler/pklr?branch=fix%2Fpreserve#dda90""#,
        )
        .unwrap();
        assert_eq!(
            sources,
            ["git+https://github.com/stempler/pklr?branch=fix/preserve#dda90"]
        );
    }

    #[test]
    fn builds_prefetch_urls_from_resolved_revisions() {
        assert_eq!(
            prefetch_url("git+https://codeberg.org/caniko/fleetix.git#abc").unwrap(),
            "git+https://codeberg.org/caniko/fleetix.git?rev=abc"
        );
        assert_eq!(
            prefetch_url("git+https://github.com/stempler/pklr?branch=fix/preserve#abc").unwrap(),
            "git+https://github.com/stempler/pklr?ref=fix/preserve&rev=abc"
        );
        assert_eq!(
            prefetch_url("git+https://example.test/repo?rev=short#full").unwrap(),
            "git+https://example.test/repo?rev=full"
        );
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = temp_dir();
        let lock = dir.join("Cargo.lock");
        let output = dir.join("hashes.nix");
        fs::write(&lock, r#"source = "git+https://example.test/repo#abc""#).unwrap();
        fs::write(&output, "original").unwrap();

        let status = update(&SourcePinsOptions {
            name: "test".into(),
            lock_file: lock,
            output_file: output.clone(),
            nix_bin: "unused".into(),
            workers: 1,
            dry_run: true,
        })
        .unwrap();

        assert_eq!(
            status,
            UpdateStatus::WouldUpdate {
                added: 1,
                removed: 0
            }
        );
        assert_eq!(fs::read_to_string(output).unwrap(), "original");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prefetch_failure_does_not_replace_sidecar() {
        let dir = temp_dir();
        let lock = dir.join("Cargo.lock");
        let output = dir.join("hashes.nix");
        let nix = dir.join("nix");
        fs::write(&lock, r#"source = "git+https://example.test/repo#abc""#).unwrap();
        fs::write(&output, "original").unwrap();
        fs::write(&nix, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&nix, fs::Permissions::from_mode(0o700)).unwrap();

        let result = update(&SourcePinsOptions {
            name: "test".into(),
            lock_file: lock,
            output_file: output.clone(),
            nix_bin: nix,
            workers: 1,
            dry_run: false,
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(output).unwrap(), "original");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_entries_are_removed() {
        let dir = temp_dir();
        let lock = dir.join("Cargo.lock");
        let output = dir.join("hashes.nix");
        fs::write(&lock, "").unwrap();
        fs::write(
            &output,
            "{\n  \"git+https://example.test/stale#abc\" = \"sha256-old\";\n}\n",
        )
        .unwrap();

        let status = update(&SourcePinsOptions {
            name: "test".into(),
            lock_file: lock,
            output_file: output.clone(),
            nix_bin: "unused".into(),
            workers: 1,
            dry_run: false,
        })
        .unwrap();

        assert_eq!(status, UpdateStatus::Updated { sources: 0 });
        assert!(!fs::read_to_string(output).unwrap().contains("stale"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nix_strings_escape_interpolation() {
        assert_eq!(nix_string_escape(r#"${value}\""#), r#"\${value}\\\""#);
    }
}
