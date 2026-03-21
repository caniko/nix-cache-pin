use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinConfig {
    pub name: String,
    pub packages: Vec<String>,
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
            full_attr_prefix: None,
        };
        assert_eq!(cfg.full_attr_prefix(), "python313Packages");
    }

    #[test]
    fn test_full_attr_prefix_without_python() {
        let cfg = PinConfig {
            name: "test".into(),
            packages: vec![],
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
        assert_eq!(cfg.full_attr_prefix(), "python313Packages");
    }
}
