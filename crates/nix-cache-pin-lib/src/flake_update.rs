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

/// Replace the pinned revision in flake.nix content.
#[must_use]
pub fn replace_rev(
    flake_nix_content: &str,
    flake_ref: &str,
    old_rev: &str,
    new_rev: &str,
) -> String {
    let old_url = flakeref::append_rev(flake_ref, old_rev);
    let new_url = flakeref::append_rev(flake_ref, new_rev);
    flake_nix_content.replacen(&old_url, &new_url, 1)
}

/// Run `nix flake lock --update-input <input_name>`.
pub async fn run_flake_lock(input_name: &str) -> Result<()> {
    let status = tokio::process::Command::new("nix")
        .args(["flake", "lock", "--update-input", input_name])
        .status()
        .await?;

    if status.success() {
        Ok(())
    } else {
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
    let content = std::fs::read_to_string(flake_nix_path)?;
    let updated = replace_rev(&content, flake_ref, old_rev, new_rev);
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
    let content = tokio::fs::read_to_string(flake_nix_path).await?;
    let updated = replace_rev(&content, flake_ref, old_rev, new_rev);
    tokio::fs::write(flake_nix_path, updated).await?;
    Ok(())
}

/// Update only one input in flake.lock using a temporary output lock. The
/// source URL in flake.nix is left untouched and unrelated dirty lock nodes
/// are preserved.
pub async fn update_flake_lock_only(
    lock_path: &Path,
    input_name: &str,
    candidate_flake_ref: &str,
) -> Result<()> {
    let baseline_content = tokio::fs::read_to_string(lock_path).await?;
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
    let merged = merge_lock_update(&baseline, &updated, input_name)?;
    let merged_content = format!(
        "{}\n",
        serde_json::to_string_pretty(&merged)
            .map_err(|e| Error::FlakeNix(format!("failed to serialize merged lock: {e}")))?
    );

    tokio::fs::write(&temporary, merged_content).await?;
    tokio::fs::rename(&temporary, lock_path).await?;
    Ok(())
}

fn merge_lock_update(baseline: &Value, updated: &Value, input_name: &str) -> Result<Value> {
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
    let mut pending = VecDeque::from([updated_node_name]);

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

        let merged = merge_lock_update(&baseline, &updated, "nixpkgs").unwrap();
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
    fn test_replace_rev() {
        let content = r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/oldrev123";"#;
        let updated = replace_rev(content, "github:NixOS/nixpkgs", "oldrev123", "newrev456");
        assert_eq!(
            updated,
            r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/newrev456";"#
        );
    }

    #[tokio::test]
    async fn test_update_flake_nix_async_replaces_first_matching_revision() {
        let path = std::env::temp_dir().join(format!(
            "nix-cache-pin-test-{}-flake.nix",
            std::process::id()
        ));
        let content = r#"nixpkgs.url = "github:NixOS/nixpkgs/oldrev123";"#;
        std::fs::write(&path, content).unwrap();

        update_flake_nix_async(&path, "github:NixOS/nixpkgs", "oldrev123", "newrev456")
            .await
            .unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            updated,
            r#"nixpkgs.url = "github:NixOS/nixpkgs/newrev456";"#
        );
    }
}
