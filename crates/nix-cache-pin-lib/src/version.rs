use crate::config::VersionConstraint;
use crate::error::{Error, Result};
use semver::{Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDecision {
    pub version: String,
    pub rejected_by: Vec<String>,
}

impl VersionDecision {
    pub fn is_allowed(&self) -> bool {
        self.rejected_by.is_empty()
    }
}

pub fn evaluate_version_rule(version: &str, rule: &VersionConstraint) -> Result<VersionDecision> {
    let mut rejected_by = Vec::new();

    if let Some(target) = &rule.target {
        if !matches_constraint(version, target)? {
            rejected_by.push(format!("target '{target}' did not match"));
        }
    }

    for taint in &rule.taints {
        if matches_constraint(version, taint)? {
            rejected_by.push(format!("taint '{taint}' matched"));
        }
    }

    Ok(VersionDecision {
        version: version.to_string(),
        rejected_by,
    })
}

pub fn matches_constraint(version: &str, expression: &str) -> Result<bool> {
    let version = normalize_version(version)?;
    let parts: Vec<_> = expression
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();

    if parts.is_empty() {
        return Err(Error::Config("empty version constraint".into()));
    }

    for part in parts {
        if !matches_single_constraint(&version, part)? {
            return Ok(false);
        }
    }

    Ok(true)
}

fn matches_single_constraint(version: &Version, expression: &str) -> Result<bool> {
    let (operator, operand) = split_operator(expression);
    let operand = normalize_version(operand)?;

    match operator {
        None | Some("=") => Ok(version == &operand),
        Some("!=") => Ok(version != &operand),
        Some(op @ ("<" | "<=" | ">" | ">=" | "~" | "^")) => {
            let req = VersionReq::parse(&format!("{op}{operand}")).map_err(|e| {
                Error::Config(format!("invalid version constraint '{expression}': {e}"))
            })?;
            Ok(req.matches(version))
        }
        Some(op) => Err(Error::Config(format!(
            "unsupported version constraint operator '{op}' in '{expression}'"
        ))),
    }
}

fn split_operator(expression: &str) -> (Option<&str>, &str) {
    let expression = expression.trim();
    for operator in ["!=", "<=", ">=", "=", "<", ">", "~", "^"] {
        if let Some(rest) = expression.strip_prefix(operator) {
            return (Some(operator), rest.trim());
        }
    }
    (None, expression)
}

fn normalize_version(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    let token = trimmed
        .split_whitespace()
        .next()
        .ok_or_else(|| Error::Config("empty version".into()))?;

    let mut numeric = String::new();
    for ch in token.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            numeric.push(ch);
        } else {
            break;
        }
    }

    if numeric.is_empty() {
        return Err(Error::Config(format!("invalid version '{raw}'")));
    }

    let mut parts: Vec<&str> = numeric.split('.').filter(|part| !part.is_empty()).collect();
    if parts.len() > 3 {
        parts.truncate(3);
    }
    while parts.len() < 3 {
        parts.push("0");
    }

    let normalized = parts.join(".");
    Version::parse(&normalized).map_err(|e| {
        Error::Config(format!(
            "invalid version '{raw}' normalized as '{normalized}': {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(target: Option<&str>, taints: &[&str]) -> VersionConstraint {
        VersionConstraint {
            target: target.map(ToOwned::to_owned),
            taints: taints.iter().map(|s| s.to_string()).collect(),
            version_attr: "version".into(),
        }
    }

    #[test]
    fn test_exact_constraint() {
        assert!(matches_constraint("7.0.8", "7.0.8").unwrap());
        assert!(matches_constraint("7", "7.0.0").unwrap());
        assert!(!matches_constraint("7.0.9", "7.0.8").unwrap());
    }

    #[test]
    fn test_comparison_constraint() {
        assert!(matches_constraint("7.0.6", "< 7.0.8").unwrap());
        assert!(!matches_constraint("7.0.9", "< 7.0.8").unwrap());
        assert!(matches_constraint("7.0.9-cachyos-lto", ">= 7.0.8").unwrap());
    }

    #[test]
    fn test_not_equal_and_comma_range() {
        assert!(matches_constraint("7.0.7", ">= 7.0.6, < 7.0.8").unwrap());
        assert!(!matches_constraint("7.0.8", ">= 7.0.6, < 7.0.8").unwrap());
        assert!(matches_constraint("7.0.9", "!= 7.0.8").unwrap());
        assert!(!matches_constraint("7.0.8", "!= 7.0.8").unwrap());
    }

    #[test]
    fn test_tilde_and_caret_constraints() {
        assert!(matches_constraint("7.0.9", "~7.0.8").unwrap());
        assert!(matches_constraint("7.1.0", "^7.0.8").unwrap());
        assert!(!matches_constraint("8.0.0", "^7.0.8").unwrap());
    }

    #[test]
    fn test_evaluate_target_and_taints() {
        let allowed =
            evaluate_version_rule("7.0.6", &rule(Some("< 7.0.8"), &[">= 7.0.8"])).unwrap();
        assert!(allowed.is_allowed());

        let rejected =
            evaluate_version_rule("7.0.9", &rule(Some("< 7.0.8"), &[">= 7.0.8"])).unwrap();
        assert_eq!(rejected.rejected_by.len(), 2);
    }
}
