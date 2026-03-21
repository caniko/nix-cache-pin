/// Append a revision to a flake reference based on its scheme.
/// - `git+*` schemes use `?rev=<rev>`
/// - All others (github:, gitlab:, sourcehut:) use `/<rev>`
pub fn append_rev(flake_ref: &str, rev: &str) -> String {
    if flake_ref.starts_with("git+") {
        format!("{flake_ref}?rev={rev}")
    } else {
        format!("{flake_ref}/{rev}")
    }
}

/// Build a regex pattern to match a flake ref with any revision in flake.nix.
pub fn flake_ref_rev_pattern(flake_ref: &str) -> String {
    let escaped = regex::escape(flake_ref);
    let rev_group = "(?P<rev>[0-9a-f]+)";
    if flake_ref.starts_with("git+") {
        format!("{escaped}\\?rev={rev_group}")
    } else {
        format!("{escaped}/{rev_group}")
    }
}

/// Extract `owner/repo` from a `github:owner/repo` flake ref, or `None`.
pub fn extract_github_repo(flake_ref: &str) -> Option<&str> {
    flake_ref.strip_prefix("github:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_rev_github() {
        assert_eq!(
            append_rev("github:NixOS/nixpkgs", "abc123"),
            "github:NixOS/nixpkgs/abc123"
        );
    }

    #[test]
    fn test_append_rev_gitlab() {
        assert_eq!(
            append_rev("gitlab:foo/bar", "def456"),
            "gitlab:foo/bar/def456"
        );
    }

    #[test]
    fn test_append_rev_git_https() {
        assert_eq!(
            append_rev("git+https://gitlab.com/foo/bar", "abc123"),
            "git+https://gitlab.com/foo/bar?rev=abc123"
        );
    }

    #[test]
    fn test_append_rev_git_ssh() {
        assert_eq!(
            append_rev("git+ssh://git@github.com/foo/bar", "abc"),
            "git+ssh://git@github.com/foo/bar?rev=abc"
        );
    }

    #[test]
    fn test_flake_ref_rev_pattern_github() {
        let pattern = flake_ref_rev_pattern("github:NixOS/nixpkgs");
        assert!(pattern.contains("(?P<rev>[0-9a-f]+)"));
        assert!(pattern.ends_with("(?P<rev>[0-9a-f]+)"));

        let re = regex::Regex::new(&pattern).unwrap();
        let url = r#"github:NixOS/nixpkgs/abc123def456"#;
        let caps = re.captures(url).unwrap();
        assert_eq!(&caps["rev"], "abc123def456");
    }

    #[test]
    fn test_flake_ref_rev_pattern_in_flake_nix() {
        let pattern = flake_ref_rev_pattern("github:NixOS/nixpkgs");
        let input_pattern = format!(r#"nixpkgs\-rocm\.url = "{pattern}""#);
        let re = regex::Regex::new(&input_pattern).unwrap();

        let test_url = r#"nixpkgs-rocm.url = "github:NixOS/nixpkgs/abc123def456""#;
        let caps = re.captures(test_url).unwrap();
        assert_eq!(&caps["rev"], "abc123def456");
    }

    #[test]
    fn test_flake_ref_rev_pattern_git_https() {
        let pattern = flake_ref_rev_pattern("git+https://gitlab.com/foo/bar");
        let re = regex::Regex::new(&pattern).unwrap();

        let url = "git+https://gitlab.com/foo/bar?rev=abc123def456";
        let caps = re.captures(url).unwrap();
        assert_eq!(&caps["rev"], "abc123def456");
    }

    #[test]
    fn test_extract_github_repo() {
        assert_eq!(
            extract_github_repo("github:NixOS/nixpkgs"),
            Some("NixOS/nixpkgs")
        );
        assert_eq!(
            extract_github_repo("github:xddxdd/nix-cachyos-kernel"),
            Some("xddxdd/nix-cachyos-kernel")
        );
    }

    #[test]
    fn test_extract_github_repo_non_github() {
        assert_eq!(extract_github_repo("git+https://gitlab.com/foo/bar"), None);
    }
}
