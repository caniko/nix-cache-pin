use crate::error::{Error, Result};
use crate::ext::RevisionOrder;
use crate::flakeref;

/// List recent commits from a GitHub repo using `gh api`.
pub async fn list_commits(owner_repo: &str, branch: &str, depth: usize) -> Result<Vec<String>> {
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner_repo}/commits?sha={branch}&per_page={depth}"),
            "--jq",
            ".[].sha",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::GitHub(format!(
            "failed to list commits for {owner_repo}@{branch} with depth {depth}: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|l| l.trim().to_string()).collect())
}

/// Compare revisions using GitHub's ancestry-aware compare endpoint.
///
/// Unlike a bounded recent-commit listing, this works when the current pin is
/// many commits behind the branch tip. A non-linear history is reported as
/// divergent and all unrecognised responses fail closed as `Unknown`.
pub async fn compare_revisions(
    flake_ref: &str,
    _branch: &str,
    current: &str,
    candidate: &str,
    _depth: usize,
) -> Result<RevisionOrder> {
    if current == candidate {
        return Ok(RevisionOrder::Equal);
    }

    let Some(owner_repo) = flakeref::extract_github_repo(flake_ref) else {
        return Ok(RevisionOrder::Unknown);
    };
    let compare = format!("{current}...{candidate}");
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner_repo}/compare/{compare}"),
            "--jq",
            ".status",
        ])
        .output()
        .await
        .map_err(|e| Error::GitHub(format!("failed to run gh: {e}")))?;
    if !output.status.success() {
        return Err(Error::GitHub(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "identical" => Ok(RevisionOrder::Equal),
        "ahead" => Ok(RevisionOrder::Newer),
        "behind" => Ok(RevisionOrder::Older),
        "diverged" => Ok(RevisionOrder::Divergent),
        _ => Ok(RevisionOrder::Unknown),
    }
}
