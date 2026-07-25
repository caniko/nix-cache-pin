use crate::config::PinConfig;
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A single flake input may be represented by several named requirements.
/// The evaluator settings stay shared while package and target requirements
/// are combined into one search.
#[derive(Debug, Clone)]
pub struct PinGroup {
    pub input_name: String,
    pub members: Vec<PinConfig>,
    pub merged: PinConfig,
}

pub fn group_configs(configs: Vec<PinConfig>) -> Result<Vec<PinGroup>> {
    let mut grouped: BTreeMap<String, Vec<PinConfig>> = BTreeMap::new();
    for cfg in configs {
        grouped.entry(cfg.input_name.clone()).or_default().push(cfg);
    }

    grouped
        .into_iter()
        .map(|(input_name, mut members)| {
            members.sort_by(|a, b| a.name.cmp(&b.name));
            merge_group(input_name, members)
        })
        .collect()
}

fn merge_group(input_name: String, members: Vec<PinConfig>) -> Result<PinGroup> {
    let names = members
        .iter()
        .map(|cfg| cfg.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let first = members
        .first()
        .ok_or_else(|| Error::Config("cannot merge an empty pin group".to_string()))?;

    for cfg in members.iter().skip(1) {
        check_equal(
            &input_name,
            &names,
            "flakeRef",
            &first.flake_ref,
            &cfg.flake_ref,
        )?;
        check_equal(
            &input_name,
            &names,
            "attrPrefix",
            &first.attr_prefix,
            &cfg.attr_prefix,
        )?;
        check_equal(
            &input_name,
            &names,
            "pythonPackages",
            &first.python_packages,
            &cfg.python_packages,
        )?;
        check_equal(&input_name, &names, "caches", &first.caches, &cfg.caches)?;
        check_equal(
            &input_name,
            &names,
            "hydraJobset",
            &first.hydra_jobset,
            &cfg.hydra_jobset,
        )?;
        check_equal(
            &input_name,
            &names,
            "hydraUrl",
            &first.hydra_url,
            &cfg.hydra_url,
        )?;
        check_equal(
            &input_name,
            &names,
            "hydraJobPattern",
            &first.hydra_job_pattern,
            &cfg.hydra_job_pattern,
        )?;
        check_equal(
            &input_name,
            &names,
            "hydraRevInput",
            &first.hydra_rev_input,
            &cfg.hydra_rev_input,
        )?;
        check_equal(&input_name, &names, "branch", &first.branch, &cfg.branch)?;
        check_equal(
            &input_name,
            &names,
            "flakeOutput",
            &first.flake_output,
            &cfg.flake_output,
        )?;
        check_equal(
            &input_name,
            &names,
            "consumerFlakeRef",
            &first.consumer_flake_ref,
            &cfg.consumer_flake_ref,
        )?;
        check_equal(
            &input_name,
            &names,
            "failFast",
            &first.fail_fast,
            &cfg.fail_fast,
        )?;
        check_equal(&input_name, &names, "arch", &first.arch, &cfg.arch)?;
        check_equal(
            &input_name,
            &names,
            "lockOnly",
            &first.lock_only,
            &cfg.lock_only,
        )?;
        check_equal(
            &input_name,
            &names,
            "verifyClosure",
            &first.verify_closure,
            &cfg.verify_closure,
        )?;
    }

    let mut merged = first.clone();
    merged.name = format!("{}[{}]", input_name, names);
    merged.depth = members
        .iter()
        .map(|cfg| cfg.depth)
        .max()
        .unwrap_or(first.depth);
    merged.branch_fallbacks = stable_union(
        members
            .iter()
            .flat_map(|cfg| cfg.branch_fallbacks.iter().cloned()),
    );
    merged.packages = stable_union(members.iter().flat_map(|cfg| cfg.packages.iter().cloned()));
    let required: BTreeSet<String> = merged.packages.iter().cloned().collect();
    merged.wish_packages = stable_union(
        members
            .iter()
            .flat_map(|cfg| cfg.wish_packages.iter().cloned())
            .filter(|pkg| !required.contains(pkg)),
    );

    let mut targets = BTreeMap::new();
    for cfg in &members {
        for (pkg, target) in &cfg.consumer_targets {
            if let Some(previous) = targets.insert(pkg.clone(), target.clone()) {
                if previous != *target {
                    return Err(Error::PinMerge {
                        input_name,
                        pins: names,
                        reason: format!("consumer target for {pkg} differs"),
                    });
                }
            }
        }
    }
    merged.consumer_targets = targets;

    let mut constraints = HashMap::new();
    for cfg in &members {
        for (pkg, constraint) in &cfg.version_constraints {
            if let Some(previous) = constraints.insert(pkg.clone(), constraint.clone()) {
                if previous != constraint.clone() {
                    return Err(Error::PinMerge {
                        input_name,
                        pins: names,
                        reason: format!("version constraint for {pkg} differs"),
                    });
                }
            }
        }
    }
    merged.version_constraints = constraints;

    Ok(PinGroup {
        input_name,
        members,
        merged,
    })
}

fn check_equal<T: PartialEq + std::fmt::Debug>(
    input_name: &str,
    pins: &str,
    field: &str,
    expected: &T,
    actual: &T,
) -> Result<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(Error::PinMerge {
            input_name: input_name.to_string(),
            pins: pins.to_string(),
            reason: format!("{field} differs ({expected:?} vs {actual:?})"),
        })
    }
}

fn stable_union<I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut set = BTreeSet::new();
    set.extend(values);
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, input: &str, packages: &[&str]) -> PinConfig {
        PinConfig::from_json(&format!(
            r#"{{
                "name":"{name}","packages":{},"inputName":"{input}",
                "attrPrefix":"pkgs","pythonPackages":null,"caches":["cache"],
                "hydraJobset":"jobset","hydraUrl":"hydra","hydraJobPattern":"pattern",
                "hydraRevInput":"nixpkgs","depth":2,"branch":"nixos-unstable",
                "flakeRef":"github:NixOS/nixpkgs","flakeOutput":"legacyPackages",
                "failFast":false,"arch":"x86_64-linux"
            }}"#,
            serde_json::to_string(&packages).unwrap()
        ))
        .unwrap()
    }

    #[test]
    fn groups_same_input_and_unions_packages() {
        let groups = group_configs(vec![
            cfg("b", "nixpkgs", &["b"]),
            cfg("a", "nixpkgs", &["a"]),
        ])
        .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members[0].name, "a");
        assert_eq!(groups[0].merged.packages, ["a", "b"]);
    }

    #[test]
    fn keeps_different_inputs_independent() {
        let groups = group_configs(vec![
            cfg("rocm", "nixpkgs-rocm", &["a"]),
            cfg("cuda", "nixpkgs-cuda", &["b"]),
        ])
        .unwrap();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn rejects_incompatible_sources() {
        let mut other = cfg("b", "nixpkgs", &["b"]);
        other.flake_ref = "github:other/source".into();
        let err = group_configs(vec![cfg("a", "nixpkgs", &["a"]), other]).unwrap_err();
        assert!(err.to_string().contains("flakeRef differs"));
    }
}
