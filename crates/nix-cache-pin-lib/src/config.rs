use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionConstraint {
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub taints: Vec<String>,
    #[serde(default = "default_version_attr")]
    pub version_attr: String,
}

fn default_version_attr() -> String {
    "version".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinConfig {
    pub name: String,
    pub packages: Vec<String>,
    #[serde(default)]
    pub wish_packages: Vec<String>,
    /// Optional paths in the consuming flake. When present, package names
    /// listed in `packages`/`wishPackages` select these paths instead of
    /// evaluating the source flake directly. This preserves follows,
    /// overlays, and consumer-specific package overrides.
    #[serde(default)]
    pub consumer_flake_ref: Option<String>,
    #[serde(default)]
    pub consumer_targets: BTreeMap<String, String>,
    pub input_name: String,
    pub attr_prefix: String,
    pub python_packages: Option<String>,
    pub caches: Vec<String>,
    pub hydra_jobset: String,
    pub hydra_url: String,
    pub hydra_job_pattern: String,
    pub hydra_rev_input: String,
    pub depth: usize,
    pub branch: String,
    pub flake_ref: String,
    pub flake_output: String,
    pub fail_fast: bool,
    pub arch: String,
    #[serde(default)]
    pub lock_only: bool,
    #[serde(default)]
    pub verify_closure: bool,
    #[serde(default)]
    pub version_constraints: HashMap<String, VersionConstraint>,
    /// Optional override from JSON; computed from attr_prefix + python_packages if absent.
    full_attr_prefix: Option<String>,
}

impl PinConfig {
    pub fn full_attr_prefix(&self) -> &str {
        if let Some(ref fap) = self.full_attr_prefix {
            return fap;
        }
        if let Some(ref pp) = self.python_packages {
            pp
        } else {
            &self.attr_prefix
        }
    }

    pub fn from_json(json: &str) -> crate::error::Result<Self> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn from_file(path: &std::path::Path) -> crate::error::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_json(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_attr_prefix_with_python() {
        let cfg = PinConfig {
            name: "test".into(),
            packages: vec![],
            wish_packages: vec![],
            consumer_flake_ref: None,
            consumer_targets: BTreeMap::new(),
            input_name: "nixpkgs".into(),
            attr_prefix: "pkgsRocm".into(),
            python_packages: Some("python313Packages".into()),
            caches: vec![],
            hydra_jobset: "nixpkgs/trunk".into(),
            hydra_url: "https://hydra.nixos.org".into(),
            hydra_job_pattern: "{jobset}/{pkg}.{arch}".into(),
            hydra_rev_input: "nixpkgs".into(),
            depth: 15,
            branch: "nixpkgs-unstable".into(),
            flake_ref: "github:NixOS/nixpkgs".into(),
            flake_output: "legacyPackages".into(),
            fail_fast: false,
            arch: "x86_64-linux".into(),
            lock_only: false,
            verify_closure: false,
            version_constraints: HashMap::new(),
            full_attr_prefix: None,
        };
        assert_eq!(cfg.full_attr_prefix(), "python313Packages");
    }

    #[test]
    fn test_full_attr_prefix_without_python() {
        let cfg = PinConfig {
            name: "test".into(),
            packages: vec![],
            wish_packages: vec![],
            consumer_flake_ref: None,
            consumer_targets: BTreeMap::new(),
            input_name: "nixpkgs".into(),
            attr_prefix: "pkgsRocm".into(),
            python_packages: None,
            caches: vec![],
            hydra_jobset: "nixpkgs/trunk".into(),
            hydra_url: "https://hydra.nixos.org".into(),
            hydra_job_pattern: "{jobset}/{pkg}.{arch}".into(),
            hydra_rev_input: "nixpkgs".into(),
            depth: 15,
            branch: "nixpkgs-unstable".into(),
            flake_ref: "github:NixOS/nixpkgs".into(),
            flake_output: "legacyPackages".into(),
            fail_fast: false,
            arch: "x86_64-linux".into(),
            lock_only: false,
            verify_closure: false,
            version_constraints: HashMap::new(),
            full_attr_prefix: None,
        };
        assert_eq!(cfg.full_attr_prefix(), "pkgsRocm");
    }

    #[test]
    fn test_full_attr_prefix_json_override() {
        let json = r#"{
            "name": "test",
            "packages": [],
            "inputName": "nixpkgs",
            "attrPrefix": "pkgsRocm",
            "pythonPackages": "python313Packages",
            "caches": [],
            "hydraJobset": "nixpkgs/trunk",
            "hydraUrl": "https://hydra.nixos.org",
            "hydraJobPattern": "{jobset}/{pkg}.{arch}",
            "hydraRevInput": "nixpkgs",
            "depth": 15,
            "branch": "nixpkgs-unstable",
            "flakeRef": "github:NixOS/nixpkgs",
            "flakeOutput": "legacyPackages",
            "failFast": false,
            "arch": "x86_64-linux",
            "fullAttrPrefix": "overridden"
        }"#;
        let cfg = PinConfig::from_json(json).unwrap();
        assert_eq!(cfg.full_attr_prefix(), "overridden");
    }

    #[test]
    fn test_deserialize_minimal() {
        let json = r#"{
            "name": "rocm",
            "packages": ["torchWithRocm", "torchvision"],
            "inputName": "nixpkgs-rocm",
            "attrPrefix": "pkgsRocm",
            "pythonPackages": "python313Packages",
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
        }"#;
        let cfg = PinConfig::from_json(json).unwrap();
        assert_eq!(cfg.name, "rocm");
        assert_eq!(cfg.packages, vec!["torchWithRocm", "torchvision"]);
        assert!(cfg.wish_packages.is_empty());
        assert!(cfg.consumer_flake_ref.is_none());
        assert!(cfg.consumer_targets.is_empty());
        assert_eq!(cfg.full_attr_prefix(), "python313Packages");
        assert!(cfg.version_constraints.is_empty());
    }

    #[test]
    fn test_deserialize_version_constraints() {
        let json = r#"{
            "name": "cachyos",
            "packages": ["linux-cachyos-latest-lto-zen4"],
            "inputName": "nix-cachyos-kernel",
            "attrPrefix": "packages",
            "pythonPackages": null,
            "caches": ["https://cache.nixos.org"],
            "hydraJobset": "nixpkgs/trunk",
            "hydraUrl": "https://hydra.nixos.org",
            "hydraJobPattern": "{jobset}/packages.{arch}.{pkg}",
            "hydraRevInput": "flake",
            "depth": 15,
            "branch": "main",
            "flakeRef": "github:xddxdd/nix-cachyos-kernel",
            "flakeOutput": "packages",
            "failFast": false,
            "arch": "x86_64-linux",
            "versionConstraints": {
                "linux-cachyos-latest-lto-zen4": {
                    "target": "< 7.0.8",
                    "taints": [">= 7.0.8"]
                }
            }
        }"#;
        let cfg = PinConfig::from_json(json).unwrap();
        let rule = cfg
            .version_constraints
            .get("linux-cachyos-latest-lto-zen4")
            .unwrap();
        assert_eq!(rule.target.as_deref(), Some("< 7.0.8"));
        assert_eq!(rule.taints, vec![">= 7.0.8"]);
        assert_eq!(rule.version_attr, "version");
    }

    #[test]
    fn test_deserialize_wish_packages() {
        let json = r#"{
            "name": "rocm",
            "packages": ["blender"],
            "wishPackages": ["obs-studio-plugins.obs-backgroundremoval"],
            "inputName": "nixpkgs-rocm",
            "attrPrefix": "pkgsRocm",
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
        }"#;
        let cfg = PinConfig::from_json(json).unwrap();
        assert_eq!(
            cfg.wish_packages,
            vec!["obs-studio-plugins.obs-backgroundremoval"]
        );
    }

    #[test]
    fn test_deserialize_consumer_targets_and_lock_only() {
        let json = r#"{
            "name": "aarch64",
            "packages": ["rauthy"],
            "consumerFlakeRef": ".",
            "consumerTargets": {
                "rauthy": "cachePinTargets.aarch64.rauthy"
            },
            "inputName": "nixpkgs",
            "attrPrefix": "pkgs",
            "pythonPackages": null,
            "caches": ["https://cache.nixos.org"],
            "hydraJobset": "nixpkgs/trunk",
            "hydraUrl": "https://hydra.nixos.org",
            "hydraJobPattern": "{jobset}/{pkg}.{arch}",
            "hydraRevInput": "nixpkgs",
            "depth": 15,
            "branch": "nixos-unstable",
            "flakeRef": "github:NixOS/nixpkgs",
            "flakeOutput": "legacyPackages",
            "failFast": false,
            "arch": "aarch64-linux",
            "lockOnly": true
        }"#;
        let cfg = PinConfig::from_json(json).unwrap();
        assert_eq!(cfg.consumer_flake_ref.as_deref(), Some("."));
        assert_eq!(
            cfg.consumer_targets.get("rauthy").map(String::as_str),
            Some("cachePinTargets.aarch64.rauthy")
        );
        assert!(cfg.lock_only);
    }
}
