use crate::error::Result;
use crate::merge::PinGroup;
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    source_of_truth: &'static str,
    targets: Vec<ManifestTarget>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestTarget {
    input_name: String,
    revision: String,
    branch: String,
    branch_fallbacks: Vec<String>,
    members: Vec<ManifestMember>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMember {
    name: String,
    packages: Vec<String>,
    wish_packages: Vec<String>,
}

pub fn write(path: &Path, groups: &[PinGroup], revisions: &[(String, String)]) -> Result<()> {
    let mut targets = Vec::with_capacity(groups.len());
    for group in groups {
        let revision = revisions
            .iter()
            .find(|(input, _)| input == &group.input_name)
            .map(|(_, revision)| revision.clone())
            .unwrap_or_default();
        targets.push(ManifestTarget {
            input_name: group.input_name.clone(),
            revision,
            branch: group.merged.branch.clone(),
            branch_fallbacks: group.merged.branch_fallbacks.clone(),
            members: group
                .members
                .iter()
                .map(|member| ManifestMember {
                    name: member.name.clone(),
                    packages: member.packages.clone(),
                    wish_packages: member.wish_packages.clone(),
                })
                .collect(),
        });
    }
    targets.sort_by(|a, b| a.input_name.cmp(&b.input_name));
    let manifest = Manifest {
        schema_version: 1,
        source_of_truth: "flake.lock",
        targets,
    };
    let content = format!("{}\n", serde_json::to_string_pretty(&manifest)?);

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(
        ".{}.cache-pin-{}-{stamp}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest"),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, content.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PinConfig;
    use crate::merge::group_configs;

    #[test]
    fn writes_a_deterministic_manifest_shape() {
        let cfg = PinConfig::from_json(
            r#"{"name":"a","packages":["hello"],"inputName":"nixpkgs","attrPrefix":"pkgs","pythonPackages":null,"caches":[],"hydraJobset":"jobset","hydraUrl":"hydra","hydraJobPattern":"pattern","hydraRevInput":"nixpkgs","depth":1,"branch":"nixos-unstable","flakeRef":"github:NixOS/nixpkgs","flakeOutput":"legacyPackages","failFast":false,"arch":"x86_64-linux"}"#,
        )
        .unwrap();
        let groups = group_configs(vec![cfg]).unwrap();
        let path =
            std::env::temp_dir().join(format!("cache-pin-manifest-{}.json", std::process::id()));
        write(&path, &groups, &[("nixpkgs".into(), "abc".into())]).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["sourceOfTruth"], "flake.lock");
        assert_eq!(value["targets"][0]["revision"], "abc");
    }
}
