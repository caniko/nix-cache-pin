use crate::config::PinConfig;
use crate::error::{Error, Result};
use crate::ext::{ExternalCommands, RevisionOrder};
use crate::hydra::{self, HydraStatus};
use crate::merge::group_configs;
use crate::narinfo::{self, Availability, PackageCheckResult};
use crate::runner;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Ready,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionRelation {
    Equal,
    Newer,
    Older,
    Divergent,
    Unknown,
}

impl From<RevisionOrder> for RevisionRelation {
    fn from(value: RevisionOrder) -> Self {
        match value {
            RevisionOrder::Equal => Self::Equal,
            RevisionOrder::Newer => Self::Newer,
            RevisionOrder::Older => Self::Older,
            RevisionOrder::Divergent => Self::Divergent,
            RevisionOrder::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Accept,
    Retain,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAvailability {
    Cached,
    Missing,
    Unknown,
}

impl From<Availability> for EvidenceAvailability {
    fn from(value: Availability) -> Self {
        match value {
            Availability::Cached => Self::Cached,
            Availability::Missing => Self::Missing,
            Availability::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    Compatible,
    Incompatible,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydraAvailability {
    Observed,
    NotObserved,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WishDecision {
    Keep,
    Promote,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    CurrentRevisionUnavailable,
    SearchFailed,
    NoCandidate,
    RevisionOlder,
    RevisionDivergent,
    RevisionUnknown,
    RequiredPackageMissing,
    RequiredPackageUnknown,
    RequiredPackageIncompatible,
    WishPromotionRequired,
    WishEvidenceUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub reason: String,
    pub pin: String,
    pub package: Option<String>,
    pub blocking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionPolicy {
    pub current: Option<String>,
    pub candidate: Option<String>,
    pub relation: Option<RevisionRelation>,
    pub decision: PolicyDecision,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub cache_url: Option<String>,
    pub store_path: Option<String>,
    pub revision: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageEvidence {
    pub package: String,
    pub availability: EvidenceAvailability,
    pub compatibility: Compatibility,
    pub provenance: Provenance,
    pub version: Option<String>,
    pub rejected_by: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HydraEvidence {
    pub availability: HydraAvailability,
    pub url: String,
    pub store_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WishEvidence {
    pub package: String,
    pub hydra: HydraEvidence,
    pub current: Option<PackageEvidence>,
    pub candidate: Option<PackageEvidence>,
    pub decision: WishDecision,
    pub reason: Option<DiagnosticCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinPlan {
    pub pin: String,
    pub members: Vec<String>,
    pub input: String,
    pub state: PlanState,
    pub revision: RevisionPolicy,
    pub required_packages: Vec<PackageEvidence>,
    pub wishes: Vec<WishEvidence>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePinPlan {
    pub schema_version: u32,
    pub state: PlanState,
    pub pins: Vec<PinPlan>,
    pub fail_before_write_failures: Vec<Diagnostic>,
}

impl CachePinPlan {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == PlanState::Ready
    }
}

/// Search and materialize a read-only plan. This function never calls an apply
/// or lock-update path; all candidate policy remains owned by the existing
/// runner, revision comparison, and narinfo verifier.
pub async fn build<E: ExternalCommands + 'static>(
    configs: Vec<PinConfig>,
    ext: &Arc<E>,
) -> Result<CachePinPlan> {
    let groups = group_configs(configs)?;
    let members: Vec<Vec<String>> = groups
        .iter()
        .map(|group| group.members.iter().map(|cfg| cfg.name.clone()).collect())
        .collect();
    let configs: Vec<PinConfig> = groups.into_iter().map(|group| group.merged).collect();
    let currents: Vec<Result<String>> = configs.iter().map(runner::current_revision).collect();
    let results = runner::find_all(configs, false, ext).await;
    let client = reqwest::Client::new();
    let mut pins = Vec::with_capacity(results.len());

    for ((result, current), members) in results.into_iter().zip(currents).zip(members) {
        pins.push(build_pin(&client, result, current, members, ext).await);
    }

    let fail_before_write_failures = pins
        .iter()
        .flat_map(|pin| pin.diagnostics.iter().filter(|item| item.blocking).cloned())
        .collect::<Vec<_>>();
    let state = if fail_before_write_failures.is_empty() {
        PlanState::Ready
    } else {
        PlanState::Blocked
    };

    Ok(CachePinPlan {
        schema_version: SCHEMA_VERSION,
        state,
        pins,
        fail_before_write_failures,
    })
}

async fn build_pin<E: ExternalCommands + 'static>(
    client: &reqwest::Client,
    result: runner::FindResult,
    current: Result<String>,
    members: Vec<String>,
    ext: &Arc<E>,
) -> PinPlan {
    let runner::FindResult {
        config, target_rev, ..
    } = result;
    let mut diagnostics = Vec::new();
    let current = match current {
        Ok(revision) => Some(revision),
        Err(error) => {
            diagnostics.push(diagnostic(
                &config,
                DiagnosticCode::CurrentRevisionUnavailable,
                None,
                error.to_string(),
            ));
            None
        }
    };
    let candidate = match target_rev {
        Ok(Some(revision)) => Some(revision),
        Ok(None) => {
            diagnostics.push(diagnostic(
                &config,
                DiagnosticCode::NoCandidate,
                None,
                "no revision found with all required packages cached".to_string(),
            ));
            None
        }
        Err(error) => {
            diagnostics.extend(error_diagnostics(&config, error));
            None
        }
    };

    let relation = revision_relation(&config, current.as_deref(), candidate.as_deref(), ext).await;
    let relation = match relation {
        Ok(relation) => relation,
        Err(error) => {
            diagnostics.push(diagnostic(
                &config,
                DiagnosticCode::SearchFailed,
                None,
                error.to_string(),
            ));
            None
        }
    };
    add_relation_diagnostic(&config, relation, &mut diagnostics);

    let required_packages = if let Some(revision) = candidate.as_deref().or(current.as_deref()) {
        let mut evidence_cfg = config.clone();
        evidence_cfg.fail_fast = false;
        let verified = narinfo::verify_required_at_rev(client, &evidence_cfg, revision, ext).await;
        verified
            .results
            .iter()
            .map(|result| {
                add_required_diagnostic(&config, result, &mut diagnostics);
                package_evidence(revision, result)
            })
            .collect()
    } else {
        Vec::new()
    };

    let wishes = wish_evidence(
        client,
        &config,
        current.as_deref(),
        candidate.as_deref(),
        ext,
        &mut diagnostics,
    )
    .await;
    let revision = revision_policy(current, candidate, relation);
    let state = if diagnostics.iter().any(|item| item.blocking) {
        PlanState::Blocked
    } else {
        PlanState::Ready
    };

    PinPlan {
        pin: config.name,
        members,
        input: config.input_name,
        state,
        revision,
        required_packages,
        wishes,
        diagnostics,
    }
}

async fn revision_relation<E: ExternalCommands + 'static>(
    cfg: &PinConfig,
    current: Option<&str>,
    candidate: Option<&str>,
    ext: &Arc<E>,
) -> Result<Option<RevisionRelation>> {
    let (Some(current), Some(candidate)) = (current, candidate) else {
        return Ok(None);
    };
    if current == candidate {
        return Ok(Some(RevisionRelation::Equal));
    }
    ext.compare_revisions(&cfg.flake_ref, &cfg.branch, current, candidate, cfg.depth)
        .await
        .map(|relation| Some(relation.into()))
}

fn revision_policy(
    current: Option<String>,
    candidate: Option<String>,
    relation: Option<RevisionRelation>,
) -> RevisionPolicy {
    let (decision, reason) = match relation {
        Some(RevisionRelation::Newer) => (PolicyDecision::Accept, "candidate is newer"),
        Some(RevisionRelation::Equal) => (PolicyDecision::Retain, "current pin is acceptable"),
        Some(RevisionRelation::Older) => (PolicyDecision::Block, "candidate is older"),
        Some(RevisionRelation::Divergent) => (PolicyDecision::Block, "histories diverge"),
        Some(RevisionRelation::Unknown) => (PolicyDecision::Block, "revision order is unknown"),
        None => (
            PolicyDecision::Block,
            "current or candidate revision is unavailable",
        ),
    };
    RevisionPolicy {
        current,
        candidate,
        relation,
        decision,
        reason: reason.to_string(),
    }
}

fn add_relation_diagnostic(
    cfg: &PinConfig,
    relation: Option<RevisionRelation>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let item = match relation {
        Some(RevisionRelation::Older) => Some((
            DiagnosticCode::RevisionOlder,
            "candidate revision is older than the current pin",
        )),
        Some(RevisionRelation::Divergent) => Some((
            DiagnosticCode::RevisionDivergent,
            "candidate and current revisions have divergent history",
        )),
        Some(RevisionRelation::Unknown) => Some((
            DiagnosticCode::RevisionUnknown,
            "candidate revision ordering could not be proven",
        )),
        _ => None,
    };
    if let Some((code, reason)) = item {
        push_diagnostic(diagnostics, diagnostic(cfg, code, None, reason.to_string()));
    }
}

fn add_required_diagnostic(
    cfg: &PinConfig,
    result: &PackageCheckResult,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let item = if compatibility(result) == Compatibility::Incompatible {
        Some(DiagnosticCode::RequiredPackageIncompatible)
    } else {
        match result.availability {
            Availability::Cached => None,
            Availability::Missing => Some(DiagnosticCode::RequiredPackageMissing),
            Availability::Unknown => Some(DiagnosticCode::RequiredPackageUnknown),
        }
    };
    if let Some(code) = item {
        push_diagnostic(
            diagnostics,
            diagnostic(
                cfg,
                code,
                Some(result.package.clone()),
                result.failure().unwrap_or_else(|| {
                    format!(
                        "required package {} is {}",
                        result.package,
                        result.availability.as_str()
                    )
                }),
            ),
        );
    }
}

async fn wish_evidence<E: ExternalCommands + 'static>(
    client: &reqwest::Client,
    cfg: &PinConfig,
    current_revision: Option<&str>,
    candidate_revision: Option<&str>,
    ext: &Arc<E>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<WishEvidence> {
    if cfg.wish_packages.is_empty() {
        return Vec::new();
    }
    let mut evidence_cfg = cfg.clone();
    evidence_cfg.fail_fast = false;
    let current = if let Some(revision) = current_revision {
        narinfo::verify_narinfo_at_rev(
            client,
            &evidence_cfg,
            revision,
            &evidence_cfg.wish_packages,
            ext,
        )
        .await
        .results
    } else {
        Vec::new()
    };
    let candidate = if let Some(revision) = candidate_revision {
        narinfo::verify_narinfo_at_rev(
            client,
            &evidence_cfg,
            revision,
            &evidence_cfg.wish_packages,
            ext,
        )
        .await
        .results
    } else {
        Vec::new()
    };
    let mut wishes = Vec::with_capacity(cfg.wish_packages.len());

    for package in &cfg.wish_packages {
        let hydra = hydra::query_hydra_build(client, cfg, package).await;
        let hydra_evidence = HydraEvidence {
            availability: if hydra.error.is_some() {
                HydraAvailability::Unknown
            } else if hydra.status == HydraStatus::OnHydra {
                HydraAvailability::Observed
            } else {
                HydraAvailability::NotObserved
            },
            url: hydra::build_hydra_job_url(cfg, package),
            store_path: hydra.store_path,
            reason: hydra.error,
        };
        let current = current_revision.and_then(|revision| {
            current
                .iter()
                .find(|result| result.package == *package)
                .map(|result| package_evidence(revision, result))
        });
        let candidate = candidate_revision.and_then(|revision| {
            candidate
                .iter()
                .find(|result| result.package == *package)
                .map(|result| package_evidence(revision, result))
        });
        let observed = diagnostics.iter().any(|item| {
            item.code == DiagnosticCode::WishPromotionRequired
                && item.package.as_deref() == Some(package)
        }) || hydra_evidence.availability == HydraAvailability::Observed
            || [&current, &candidate]
                .into_iter()
                .flatten()
                .any(|item| item.availability == EvidenceAvailability::Cached);
        let unknown = hydra_evidence.availability == HydraAvailability::Unknown
            || current_revision.is_some()
                && current
                    .as_ref()
                    .is_none_or(|item| item.availability == EvidenceAvailability::Unknown)
            || candidate_revision.is_some()
                && candidate
                    .as_ref()
                    .is_none_or(|item| item.availability == EvidenceAvailability::Unknown);
        let (decision, reason) = if observed {
            (
                WishDecision::Promote,
                Some(DiagnosticCode::WishPromotionRequired),
            )
        } else if unknown || current_revision.is_none() {
            (
                WishDecision::Block,
                Some(DiagnosticCode::WishEvidenceUnknown),
            )
        } else {
            (WishDecision::Keep, None)
        };
        if let Some(code) = reason {
            let message = match code {
                DiagnosticCode::WishPromotionRequired => {
                    format!("wish package {package} is now available and must be promoted")
                }
                _ => format!("wish package {package} availability is unknown"),
            };
            push_diagnostic(
                diagnostics,
                diagnostic(cfg, code, Some(package.clone()), message),
            );
        }
        wishes.push(WishEvidence {
            package: package.clone(),
            hydra: hydra_evidence,
            current,
            candidate,
            decision,
            reason,
        });
    }
    wishes
}

fn package_evidence(revision: &str, result: &PackageCheckResult) -> PackageEvidence {
    PackageEvidence {
        package: result.package.clone(),
        availability: result.availability.into(),
        compatibility: compatibility(result),
        provenance: Provenance {
            cache_url: result.cache.clone(),
            store_path: result.store_path.clone(),
            revision: revision.to_string(),
            target: result
                .target
                .clone()
                .unwrap_or_else(|| result.package.clone()),
        },
        version: result.version.clone(),
        rejected_by: result.version_rejected_by.clone(),
        reason: result.failure(),
    }
}

fn compatibility(result: &PackageCheckResult) -> Compatibility {
    if !result.version_rejected_by.is_empty() {
        Compatibility::Incompatible
    } else if result.store_path.is_none() || result.version_error.is_some() {
        Compatibility::Unknown
    } else {
        Compatibility::Compatible
    }
}

fn error_diagnostics(cfg: &PinConfig, error: Error) -> Vec<Diagnostic> {
    match error {
        Error::WishPackagesBuilt { location, packages } => packages
            .split(',')
            .map(str::trim)
            .map(|package| {
                diagnostic(
                    cfg,
                    DiagnosticCode::WishPromotionRequired,
                    Some(package.to_string()),
                    format!("wish package {package} is available at {location}"),
                )
            })
            .collect(),
        Error::RevisionPolicy { relation, .. } if relation == "divergent history" => {
            vec![diagnostic(
                cfg,
                DiagnosticCode::RevisionDivergent,
                None,
                relation,
            )]
        }
        Error::RevisionPolicy { relation, .. } => vec![diagnostic(
            cfg,
            DiagnosticCode::RevisionOlder,
            None,
            relation,
        )],
        Error::RevisionOrderUnknown { .. } => vec![diagnostic(
            cfg,
            DiagnosticCode::RevisionUnknown,
            None,
            error.to_string(),
        )],
        error => vec![diagnostic(
            cfg,
            DiagnosticCode::SearchFailed,
            None,
            error.to_string(),
        )],
    }
}

fn diagnostic(
    cfg: &PinConfig,
    code: DiagnosticCode,
    package: Option<String>,
    reason: String,
) -> Diagnostic {
    Diagnostic {
        code,
        reason,
        pin: cfg.name.clone(),
        package,
        blocking: true,
    }
}

fn push_diagnostic(diagnostics: &mut Vec<Diagnostic>, item: Diagnostic) {
    if !diagnostics
        .iter()
        .any(|current| current.code == item.code && current.package == item.package)
    {
        diagnostics.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_fixtures_round_trip() {
        for fixture in [
            include_str!("../tests/fixtures/plan-provenance.json"),
            include_str!("../tests/fixtures/plan-incompatible-downgrade-wish.json"),
            include_str!("../tests/fixtures/plan-aggregate-failure.json"),
        ] {
            let plan: CachePinPlan = serde_json::from_str(fixture).unwrap();
            assert_eq!(plan.schema_version, SCHEMA_VERSION);
            let encoded = serde_json::to_string(&plan).unwrap();
            assert_eq!(
                serde_json::from_str::<CachePinPlan>(&encoded).unwrap(),
                plan
            );
        }
    }

    #[test]
    fn aggregate_fixture_blocks_every_write() {
        let plan: CachePinPlan = serde_json::from_str(include_str!(
            "../tests/fixtures/plan-aggregate-failure.json"
        ))
        .unwrap();
        assert_eq!(plan.state, PlanState::Blocked);
        assert!(!plan.is_ready());
        assert_eq!(plan.fail_before_write_failures.len(), 2);
    }

    #[test]
    fn provenance_fixture_is_public_and_complete() {
        let plan: CachePinPlan =
            serde_json::from_str(include_str!("../tests/fixtures/plan-provenance.json")).unwrap();
        let evidence = &plan.pins[0].required_packages[0];
        assert_eq!(
            evidence.provenance.cache_url.as_deref(),
            Some("https://cache.nixos.org")
        );
        assert_eq!(evidence.provenance.revision, "bbbbbbbb");
        assert_eq!(evidence.provenance.target, "packages.x86_64-linux.hello");
    }

    #[test]
    fn package_result_maps_incompatibility_and_provenance() {
        let result = PackageCheckResult {
            package: "kernel".to_string(),
            target: Some("packages.x86_64-linux.kernel".to_string()),
            cached: true,
            availability: Availability::Cached,
            cache: Some("https://cache.example.test".to_string()),
            store_path: Some("/nix/store/hash-kernel".to_string()),
            error: None,
            version: Some("7.0.9".to_string()),
            version_error: None,
            version_rejected_by: vec!["target '< 7.0.8' did not match".to_string()],
        };

        let evidence = package_evidence("candidate", &result);

        assert_eq!(evidence.compatibility, Compatibility::Incompatible);
        assert_eq!(evidence.availability, EvidenceAvailability::Cached);
        assert_eq!(evidence.provenance.revision, "candidate");
        assert_eq!(evidence.provenance.target, "packages.x86_64-linux.kernel");
    }
}
