use crate::error::Result;
use crate::merge::PinGroup;
use serde::{Deserialize, Serialize};
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

/// Targets of a previously written manifest; other fields are regenerated.
#[derive(Debug, Deserialize)]
struct ExistingManifest {
    targets: Vec<ManifestTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestTarget {
    input_name: String,
    revision: String,
    branch: String,
    branch_fallbacks: Vec<String>,
    members: Vec<ManifestMember>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMember {
    name: String,
    packages: Vec<String>,
    wish_packages: Vec<String>,
}

pub fn write(path: &Path, groups: &[PinGroup], revisions: &[(String, String)]) -> Result<()> {
    let guard = crate::mutation::Mutation::acquire(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    write_held(path, groups, revisions, &guard)
}

pub(crate) fn write_held(
    path: &Path,
    groups: &[PinGroup],
    revisions: &[(String, String)],
    _guard: &crate::mutation::Mutation,
) -> Result<()> {
    let before = crate::mutation::read(path)?;
    // A run may cover only some inputs (one selected pin): keep the recorded
    // targets of every other input instead of dropping them.
    let mut targets: Vec<ManifestTarget> = match &before {
        Some(bytes) => serde_json::from_slice::<ExistingManifest>(bytes)
            .map_err(|error| {
                crate::error::Error::Config(format!(
                    "cannot preserve existing {}: {error}",
                    path.display()
                ))
            })?
            .targets
            .into_iter()
            .filter(|target| {
                !groups
                    .iter()
                    .any(|group| group.input_name == target.input_name)
            })
            .collect(),
        None => Vec::new(),
    };
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
    if let Err(error) = crate::mutation::unchanged(path, &before) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
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

    #[test]
    fn a_partial_run_preserves_other_inputs() {
        let config = |name: &str, input: &str| {
            PinConfig::from_json(&format!(
                r#"{{"name":"{name}","packages":["hello"],"inputName":"{input}","attrPrefix":"pkgs","pythonPackages":null,"caches":[],"hydraJobset":"jobset","hydraUrl":"hydra","hydraJobPattern":"pattern","hydraRevInput":"nixpkgs","depth":1,"branch":"nixos-unstable","flakeRef":"github:NixOS/nixpkgs","flakeOutput":"legacyPackages","failFast":false,"arch":"x86_64-linux"}}"#
            ))
            .unwrap()
        };
        // A private directory: the mutation lock is per directory.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache-pin.lock.json");
        let both = group_configs(vec![config("a", "alpha"), config("b", "beta")]).unwrap();
        write(
            &path,
            &both,
            &[("alpha".into(), "a1".into()), ("beta".into(), "b1".into())],
        )
        .unwrap();
        let first = std::fs::read_to_string(&path).unwrap();

        let only_alpha = group_configs(vec![config("a", "alpha")]).unwrap();
        write(&path, &only_alpha, &[("alpha".into(), "a2".into())]).unwrap();
        // Only the run's input changes; beta's recorded target is byte-identical.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            first.replace("\"a1\"", "\"a2\"")
        );

        std::fs::write(&path, "not json").unwrap();
        assert!(write(&path, &only_alpha, &[("alpha".into(), "a3".into())]).is_err());
    }
}
