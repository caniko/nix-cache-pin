use crate::error::{Error, Result};
use crate::flakeref;
use std::path::Path;

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

    let re = regex::Regex::new(&pattern).map_err(|e| Error::FlakeNix(e.to_string()))?;

    match re.captures(flake_nix_content) {
        Some(caps) => Ok(caps["rev"].to_string()),
        None => Err(Error::FlakeNix(format!(
            "could not find {input_name}.url pattern in flake.nix"
        ))),
    }
}

/// Replace the pinned revision in flake.nix content.
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
    fn test_replace_rev() {
        let content = r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/oldrev123";"#;
        let updated = replace_rev(content, "github:NixOS/nixpkgs", "oldrev123", "newrev456");
        assert_eq!(
            updated,
            r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/newrev456";"#
        );
    }
}
