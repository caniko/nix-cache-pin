use crate::error::{Error, Result};
use crate::flakeref;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Read the current pinned revision from flake.nix.
pub fn read_current_rev(
    flake_nix_content: &str,
    input_name: &str,
    flake_ref: &str,
) -> Result<String> {
    let rev_pattern = flakeref::flake_ref_rev_pattern(flake_ref);
    // Escape dots in input name for regex
    let escaped_input = input_name.replace('.', r"\.");
    // Match both `input.url = "..."` (dot notation) and block format:
    //   input = {
    //     url = "...";
    let pattern = format!(r#"{escaped_input}(?:\.url|\s*=\s*\{{\s*url)\s*=\s*"{rev_pattern}""#);

    let re = regex::Regex::new(&pattern).map_err(|e| {
        Error::FlakeNix(format!(
            "failed to build revision matcher for input '{input_name}' and flake ref '{flake_ref}': {e}"
        ))
    })?;

    match re.captures(flake_nix_content) {
        Some(caps) => Ok(caps["rev"].to_string()),
        None => Err(Error::FlakeNix(format!(
            "could not find pinned URL for input '{input_name}' with flake ref '{flake_ref}' in flake.nix"
        ))),
    }
}

/// Read the locked revision for a top-level input without consulting or
/// modifying flake.nix.
pub fn read_current_locked_rev(lock_path: &Path, input_name: &str) -> Result<String> {
    let content = std::fs::read_to_string(lock_path)?;
    let lock: Value = serde_json::from_str(&content)
        .map_err(|e| Error::FlakeNix(format!("failed to parse {}: {e}", lock_path.display())))?;
    let node_name = lock
        .pointer(&format!("/nodes/root/inputs/{input_name}"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::FlakeNix(format!("lock file has no root input '{input_name}'")))?;
    lock.pointer(&format!("/nodes/{node_name}/locked/rev"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::FlakeNix(format!(
                "lock file input '{input_name}' has no locked revision"
            ))
        })
}

/// Replace a unique pinned URL in flake.nix content.
///
/// Returns the original content if the URL is missing or ambiguous. Prefer
/// [`replace_input_rev`] when the input name is known.
#[must_use]
pub fn replace_rev(
    flake_nix_content: &str,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> String {
    let old_url = flakeref::append_rev(flake_ref, old_rev);
    let new_url = flakeref::append_rev(flake_ref, new_rev);
    // This compatibility API has no input identity. Never guess when two
    // inputs share a URL; callers needing an update must use replace_input_rev.
    if flake_nix_content.matches(&old_url).count() != 1 {
        return flake_nix_content.to_string();
    }
    flake_nix_content.replacen(&old_url, &new_url, 1)
}

/// Replace the literal pinned URL of exactly one named input.
///
/// Supports dotted declarations and input blocks with `url` as their first
/// attribute, like [`read_current_rev`]. Missing, stale, or ambiguous source
/// declarations fail before writing. Other inputs may share the old URL.
pub fn replace_input_rev(
    content: &str,
    input_name: &str,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> Result<String> {
    let input = regex::escape(input_name);
    let old_url = regex::escape(&flakeref::append_rev(flake_ref, old_rev));
    let pattern = format!(
        r#"(?m)(?:^|[;{{])\s*(?:inputs\s*\.\s*)?(?:{input}|"{input}")(?:\s*\.\s*url|\s*=\s*\{{\s*url)\s*=\s*"(?P<url>{old_url})""#
    );
    let re = regex::Regex::new(&pattern)
        .map_err(|error| Error::FlakeNix(format!("invalid input URL matcher: {error}")))?;
    let mut matches = re.captures_iter(content);
    let matched = matches.next().ok_or_else(|| {
        Error::FlakeNix(format!(
            "expected source URL for input '{input_name}' was not found; refusing stale update"
        ))
    })?;
    if matches.next().is_some() {
        return Err(Error::FlakeNix(format!(
            "multiple source URLs for input '{input_name}'; refusing ambiguous update"
        )));
    }
    let url = matched
        .name("url")
        .expect("URL matcher has a named capture");
    let mut updated = content.to_string();
    updated.replace_range(url.range(), &flakeref::append_rev(flake_ref, new_rev));
    Ok(updated)
}

/// Update one named input on disk while preserving all other declarations.
pub async fn update_input_flake_nix_async(
    path: &Path,
    input_name: &str,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> Result<()> {
    let _guard = crate::mutation::Mutation::acquire(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let content = std::fs::read_to_string(path)?;
    let updated = replace_input_rev(&content, input_name, flake_ref, old_rev, new_rev)?;
    crate::mutation::unchanged(path, &Some(content.into_bytes()))?;
    std::fs::write(path, updated)?;
    Ok(())
}

/// Run `nix flake lock --update-input <input_name>`.
pub async fn run_flake_lock(input_name: &str) -> Result<()> {
    let _guard = crate::mutation::Mutation::acquire(Path::new("."))?;
    let path = Path::new("flake.lock");
    let before = crate::mutation::read(path)?;
    let source_before = crate::mutation::read(Path::new("flake.nix"))?;
    let temporary = Path::new(".").join(format!(".cache-pin-lock-{}.json", std::process::id()));
    if temporary.exists() {
        return Err(Error::FlakeNix(format!(
            "stale temporary lock {}; inspect before retrying",
            temporary.display()
        )));
    }
    let status = tokio::process::Command::new("nix")
        .args(["flake", "lock", "--update-input", input_name])
        .arg("--output-lock-file")
        .arg(&temporary)
        .status()
        .await?;

    if status.success() {
        let result = (|| -> Result<()> {
            crate::mutation::unchanged(path, &before)?;
            crate::mutation::unchanged(Path::new("flake.nix"), &source_before)?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    } else {
        let _ = std::fs::remove_file(&temporary);
        Err(Error::FlakeNix(format!(
            "nix flake lock --update-input {input_name} failed with status {status}"
        )))
    }
}

/// Update flake.nix on disk: read, replace rev, write back.
pub fn update_flake_nix(
    flake_nix_path: &Path,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> Result<()> {
    let _guard = crate::mutation::Mutation::acquire(
        flake_nix_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let content = std::fs::read_to_string(flake_nix_path)?;
    let updated = replace_rev(&content, flake_ref, old_rev, new_rev);
    if updated == content && old_rev != new_rev {
        return Err(Error::FlakeNix(
            "expected source revision was not found uniquely; refusing stale or ambiguous update"
                .into(),
        ));
    }
    crate::mutation::unchanged(flake_nix_path, &Some(content.into_bytes()))?;
    std::fs::write(flake_nix_path, updated)?;
    Ok(())
}

/// Async variant for callers already running on the Tokio runtime.
pub async fn update_flake_nix_async(
    flake_nix_path: &Path,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> Result<()> {
    update_flake_nix(flake_nix_path, flake_ref, old_rev, new_rev)
}

/// Update only one input in flake.lock using a temporary output lock. The
/// source URL in flake.nix is left untouched and unrelated dirty lock nodes
/// are preserved.
pub async fn update_flake_lock_only(
    lock_path: &Path,
    input_name: &str,
    candidate_flake_ref: &str,
) -> Result<()> {
    let guard = crate::mutation::Mutation::acquire(
        lock_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    update_flake_lock_held(lock_path, input_name, candidate_flake_ref, None, &guard).await
}

pub(crate) async fn update_flake_lock_held(
    lock_path: &Path,
    input_name: &str,
    candidate_flake_ref: &str,
    source_revision: Option<&str>,
    _guard: &crate::mutation::Mutation,
) -> Result<()> {
    let baseline_content = tokio::fs::read_to_string(lock_path).await?;
    let source_before = crate::mutation::read(Path::new("flake.nix"))?;
    let baseline: Value = serde_json::from_str(&baseline_content)
        .map_err(|e| Error::FlakeNix(format!("failed to parse {}: {e}", lock_path.display())))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::FlakeNix(format!("system clock is before UNIX_EPOCH: {e}")))?
        .as_nanos();
    let temporary = lock_path.with_file_name(format!(
        ".{}.cache-pin-{}-{stamp}.lock",
        lock_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("flake"),
        std::process::id()
    ));
    let lock_path_string = lock_path.to_string_lossy().into_owned();
    let temporary_string = temporary.to_string_lossy().into_owned();

    let status = tokio::process::Command::new("nix")
        .args([
            "flake",
            "lock",
            ".",
            "--override-input",
            input_name,
            candidate_flake_ref,
            "--reference-lock-file",
            &lock_path_string,
            "--output-lock-file",
            &temporary_string,
        ])
        .status()
        .await?;

    if !status.success() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(Error::FlakeNix(format!(
            "nix flake lock --override-input {input_name} failed with status {status}"
        )));
    }

    let updated_content = tokio::fs::read_to_string(&temporary).await?;
    let updated: Value = serde_json::from_str(&updated_content)
        .map_err(|e| Error::FlakeNix(format!("failed to parse temporary cache-pin lock: {e}")))?;
    let merged = merge_lock_update(&baseline, &updated, input_name, source_revision)?;
    let merged_content = format!(
        "{}\n",
        serde_json::to_string_pretty(&merged)
            .map_err(|e| Error::FlakeNix(format!("failed to serialize merged lock: {e}")))?
    );

    tokio::fs::write(&temporary, merged_content).await?;
    if let Err(error) = crate::mutation::unchanged(lock_path, &Some(baseline_content.into_bytes()))
        .and_then(|()| crate::mutation::unchanged(Path::new("flake.nix"), &source_before))
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    tokio::fs::rename(&temporary, lock_path).await?;
    Ok(())
}

fn merge_lock_update(
    baseline: &Value,
    updated: &Value,
    input_name: &str,
    source_revision: Option<&str>,
) -> Result<Value> {
    let mut merged = baseline.clone();
    let updated_input = updated
        .pointer(&format!("/nodes/root/inputs/{input_name}"))
        .cloned()
        .ok_or_else(|| {
            Error::FlakeNix(format!("temporary lock has no root input '{input_name}'"))
        })?;
    let updated_node_name = updated_input
        .as_str()
        .ok_or_else(|| {
            Error::FlakeNix(format!(
                "temporary lock input '{input_name}' is not a node name"
            ))
        })?
        .to_string();
    merged
        .pointer_mut("/nodes/root/inputs")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::FlakeNix("baseline lock has no root inputs".to_string()))?
        .insert(input_name.to_string(), updated_input);

    let baseline_nodes = baseline
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::FlakeNix("baseline lock has no nodes".to_string()))?;
    let updated_nodes = updated
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::FlakeNix("temporary lock has no nodes".to_string()))?;
    let merged_nodes = merged
        .get_mut("nodes")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::FlakeNix("baseline lock has no mutable nodes".to_string()))?;

    let mut reachable = HashSet::new();
    let mut pending = VecDeque::from([updated_node_name.clone()]);

    while let Some(name) = pending.pop_front() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(node) = updated_nodes.get(&name) else {
            return Err(Error::FlakeNix(format!(
                "temporary lock is missing reachable node '{name}'"
            )));
        };
        if let Some(inputs) = node.get("inputs").and_then(Value::as_object) {
            for input in inputs.values() {
                match input {
                    Value::String(child) => pending.push_back(child.clone()),
                    Value::Array(children) => {
                        for child in children {
                            if let Some(child) = child.as_str() {
                                pending.push_back(child.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    for name in reachable {
        let node = updated_nodes
            .get(&name)
            .expect("reachable nodes were checked above");
        if baseline_nodes.get(&name) != Some(node) {
            merged_nodes.insert(name, node.clone());
        }
    }
    if let Some(revision) = source_revision {
        // --override-input resolves the new lock while flake.nix is still
        // staged, so Nix keeps the old declaration in `original`. Only source
        // pins change that declaration; lock-only pins must retain it.
        let node = merged_nodes.get_mut(&updated_node_name).ok_or_else(|| {
            Error::FlakeNix(format!("merged lock is missing input '{input_name}'"))
        })?;
        if node.pointer("/locked/rev").and_then(Value::as_str) != Some(revision) {
            return Err(Error::FlakeNix(format!(
                "resolved revision for input '{input_name}' differs from staged source"
            )));
        }
        let original_rev = node
            .pointer_mut("/original/rev")
            .filter(|rev| rev.is_string())
            .ok_or_else(|| {
                Error::FlakeNix(format!(
                    "input '{input_name}' has no literal original revision; refusing source update"
                ))
            })?;
        *original_rev = Value::String(revision.to_string());
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_current_rev() {
        let content = r#"
{
  inputs = {
    nixpkgs-rocm.url = "github:NixOS/nixpkgs/abc123def456789012345678901234567890abcd";
  };
  outputs = _: {};
}
"#;
        let rev = read_current_rev(content, "nixpkgs-rocm", "github:NixOS/nixpkgs").unwrap();
        assert_eq!(rev, "abc123def456789012345678901234567890abcd");
    }

    #[test]
    fn test_read_current_rev_git_plus() {
        let content = r#"
{
  inputs = {
    my-input.url = "git+https://gitlab.com/foo/bar?rev=abc123def456789012345678901234567890abcd";
  };
}
"#;
        let rev = read_current_rev(content, "my-input", "git+https://gitlab.com/foo/bar").unwrap();
        assert_eq!(rev, "abc123def456789012345678901234567890abcd");
    }

    #[test]
    fn test_read_current_rev_block_format() {
        let content = r#"
{
  inputs = {
    nix-cachyos-kernel = {
      url = "github:xddxdd/nix-cachyos-kernel/1fba6b310fc783186697bf5e27e3bea5b1e6def4";
      inputs.flake-parts.follows = "flake-parts";
    };
  };
}
"#;
        let rev = read_current_rev(
            content,
            "nix-cachyos-kernel",
            "github:xddxdd/nix-cachyos-kernel",
        )
        .unwrap();
        assert_eq!(rev, "1fba6b310fc783186697bf5e27e3bea5b1e6def4");
    }

    #[test]
    fn test_read_current_rev_not_found() {
        let content = r#"{ inputs = {}; }"#;
        assert!(read_current_rev(content, "nixpkgs", "github:NixOS/nixpkgs").is_err());
    }

    #[test]
    fn test_read_current_locked_rev() {
        let path = std::env::temp_dir().join(format!(
            "nix-cache-pin-test-{}-flake.lock",
            std::process::id()
        ));
        let content = serde_json::json!({
            "nodes": {
                "root": {"inputs": {"nixpkgs": "nixpkgs_1"}},
                "nixpkgs_1": {"locked": {"rev": "abc123"}}
            }
        });
        std::fs::write(&path, serde_json::to_string(&content).unwrap()).unwrap();

        assert_eq!(read_current_locked_rev(&path, "nixpkgs").unwrap(), "abc123");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_merge_lock_update_only_copies_selected_reachable_nodes() {
        let baseline = serde_json::json!({
            "nodes": {
                "root": {"inputs": {
                    "nixpkgs": "nixpkgs_1",
                    "unrelated": "unrelated_1"
                }},
                "nixpkgs_1": {"locked": {"rev": "old"}},
                "unrelated_1": {"locked": {"rev": "keep"}}
            }
        });
        let updated = serde_json::json!({
            "nodes": {
                "root": {"inputs": {
                    "nixpkgs": "nixpkgs_2",
                    "unrelated": "unrelated_1"
                }},
                "nixpkgs_2": {"locked": {"rev": "new"}}
                ,"unrelated_2": {"locked": {"rev": "must-not-copy"}}
            }
        });

        let merged = merge_lock_update(&baseline, &updated, "nixpkgs", None).unwrap();
        assert_eq!(
            merged.pointer("/nodes/root/inputs/nixpkgs"),
            Some(&Value::String("nixpkgs_2".to_string()))
        );
        assert_eq!(
            merged.pointer("/nodes/unrelated_1/locked/rev"),
            Some(&Value::String("keep".to_string()))
        );
        assert_eq!(
            merged.pointer("/nodes/nixpkgs_2/locked/rev"),
            Some(&Value::String("new".to_string()))
        );
        assert!(merged.pointer("/nodes/unrelated_2").is_none());
    }

    #[test]
    fn source_pins_advance_original_revision_but_lock_only_pins_preserve_it() {
        let baseline = serde_json::json!({
            "nodes": {
                "root": {"inputs": {"gpu": "gpu_2", "other": "other"}},
                "gpu_2": {
                    "original": {"type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "old"},
                    "locked": {"rev": "old"}
                },
                "other": {"original": {"rev": "old"}, "locked": {"rev": "old"}}
            }
        });
        // Nix --override-input preserves the original source declaration while
        // resolving the target. Source edits are still staged at this point.
        let mut resolved = baseline.clone();
        resolved["nodes"]["gpu_2"]["locked"]["rev"] = Value::String("new".into());
        let source = merge_lock_update(&baseline, &resolved, "gpu", Some("new")).unwrap();
        assert_eq!(source["nodes"]["gpu_2"]["original"]["rev"], "new");
        assert_eq!(source["nodes"]["gpu_2"]["original"]["repo"], "nixpkgs");
        assert_eq!(
            source["nodes"]["gpu_2"]["locked"],
            resolved["nodes"]["gpu_2"]["locked"]
        );
        assert_eq!(source["nodes"]["other"], baseline["nodes"]["other"]);
        let lock_only = merge_lock_update(&baseline, &resolved, "gpu", None).unwrap();
        assert_eq!(lock_only, resolved);
        assert!(merge_lock_update(&baseline, &resolved, "gpu", Some("unexpected")).is_err());
        resolved["nodes"]["gpu_2"]["original"] = serde_json::json!({"ref": "branch"});
        assert!(merge_lock_update(&baseline, &resolved, "gpu", Some("new")).is_err());
        assert_eq!(
            merge_lock_update(&baseline, &resolved, "gpu", None).unwrap(),
            resolved
        );
    }

    #[test]
    fn test_replace_rev() {
        let content = r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/oldrev123";"#;
        let updated = replace_rev(content, "github:NixOS/nixpkgs", "oldrev123", "newrev456");
        assert_eq!(
            updated,
            r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/newrev456";"#
        );
    }

    #[test]
    fn named_updates_keep_shared_urls_with_their_inputs_in_either_order() {
        let content = r#"{
  inputs = {
    nixpkgs-rocm.url = "github:NixOS/nixpkgs/oldrev123";
    nixpkgs-cuda.url = "github:NixOS/nixpkgs/oldrev123";
    untouched.url = "github:NixOS/nixpkgs/newcuda456";
  };
}"#;
        for names in [["cuda", "rocm"], ["rocm", "cuda"]] {
            let mut updated = content.to_string();
            for name in names {
                updated = replace_input_rev(
                    &updated,
                    &format!("nixpkgs-{name}"),
                    "github:NixOS/nixpkgs",
                    "oldrev123",
                    &format!("new{name}456"),
                )
                .unwrap();
            }
            assert_eq!(
                updated,
                content
                    .replace(
                        "nixpkgs-rocm.url = \"github:NixOS/nixpkgs/oldrev123",
                        "nixpkgs-rocm.url = \"github:NixOS/nixpkgs/newrocm456"
                    )
                    .replace(
                        "nixpkgs-cuda.url = \"github:NixOS/nixpkgs/oldrev123",
                        "nixpkgs-cuda.url = \"github:NixOS/nixpkgs/newcuda456"
                    )
            );
        }
    }

    #[test]
    fn named_update_supports_blocks_quoted_names_and_git_urls() {
        for declaration in ["inputs.\"gpu\".url", "gpu = { url", "inputs.gpu = {\n url"] {
            let content = format!("{declaration} = \"git+https://example.org/repo?rev=old\";");
            let updated = replace_input_rev(
                &content,
                "gpu",
                "git+https://example.org/repo",
                "old",
                "new",
            )
            .unwrap();
            assert_eq!(updated, content.replace("?rev=old", "?rev=new"));
        }
    }

    #[test]
    fn named_update_rejects_missing_stale_duplicate_and_lookalike_inputs() {
        for content in [
            "other-gpu.url = \"github:owner/repo/old\";",
            "gpu.url = \"github:owner/repo/stale\";",
            "gpu.url = \"github:owner/repo/old\";\ngpu.url = \"github:owner/repo/old\";",
            "# gpu.url = \"github:owner/repo/old\";",
        ] {
            assert!(
                replace_input_rev(content, "gpu", "github:owner/repo", "old", "new").is_err(),
                "{content}"
            );
        }
    }

    #[tokio::test]
    async fn legacy_source_update_rejects_shared_urls_without_writing() {
        let directory = std::env::temp_dir().join(format!(
            "nix-cache-pin-test-{}-ambiguous-source-update",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("flake.nix");
        let content = r#"{
  inputs.nixpkgs-rocm.url = "github:NixOS/nixpkgs/oldrev123";
  inputs.nixpkgs-cuda.url = "github:NixOS/nixpkgs/oldrev123";
}"#;
        std::fs::write(&path, content).unwrap();
        let result =
            update_flake_nix_async(&path, "github:NixOS/nixpkgs", "oldrev123", "newrev456").await;
        let after = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            result.is_err(),
            "unnamed source updates must reject ambiguous URLs"
        );
        assert_eq!(after, content);
    }

    #[tokio::test]
    async fn test_update_flake_nix_async_replaces_first_matching_revision() {
        let directory = std::env::temp_dir().join(format!(
            "nix-cache-pin-test-{}-source-update",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("flake.nix");
        let content = r#"nixpkgs.url = "github:NixOS/nixpkgs/oldrev123";"#;
        std::fs::write(&path, content).unwrap();

        update_flake_nix_async(&path, "github:NixOS/nixpkgs", "oldrev123", "newrev456")
            .await
            .unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(
            updated,
            r#"nixpkgs.url = "github:NixOS/nixpkgs/newrev456";"#
        );
    }
}
