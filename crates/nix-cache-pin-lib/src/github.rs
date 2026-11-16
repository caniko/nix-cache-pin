use crate::error::{Error, Result};
use crate::ext::RevisionOrder;
use crate::flakeref;

fn transient(stderr: &str) -> bool {
    stderr.split("HTTP ").skip(1).any(|part| {
        matches!(
            part.split(|c: char| !c.is_ascii_digit()).next(),
            Some("502" | "503" | "504")
        )
    })
}

async fn gh_api(args: &[&str]) -> std::io::Result<(std::process::Output, usize)> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let output = tokio::process::Command::new("gh")
            .arg("api")
            .args(args)
            .output()
            .await?;
        let delay = match attempts {
            1 => 2,
            2 => 8,
            _ => return Ok((output, attempts)),
        };
        if output.status.success() || !transient(&String::from_utf8_lossy(&output.stderr)) {
            return Ok((output, attempts));
        }
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::transient;

    #[test]
    fn retries_only_transient_gateway_errors() {
        for code in [502, 503, 504] {
            assert!(transient(&format!("gh: Server Error (HTTP {code})")));
        }
        for stderr in [
            "gh: Not Found (HTTP 404)",
            "gh: Unauthorized (HTTP 401)",
            "gh: API rate limit exceeded (HTTP 403)",
            "",
        ] {
            assert!(!transient(stderr));
        }
    }
}

/// List recent commits from a GitHub repo using `gh api`.
pub async fn list_commits(owner_repo: &str, branch: &str, depth: usize) -> Result<Vec<String>> {
    let (output, attempts) = gh_api(&[
        &format!("repos/{owner_repo}/commits?sha={branch}&per_page={depth}"),
        "--jq",
        ".[].sha",
    ])
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::GitHub(format!(
            "failed to list commits for {owner_repo}@{branch} with depth {depth}: {} (after {attempts} attempt(s))",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|l| l.trim().to_string()).collect())
}

/// Resolve a short revision to its full SHA through the GitHub API.
pub async fn resolve_commit(owner_repo: &str, short: &str) -> Result<String> {
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner_repo}/commits/{short}"),
            "--jq",
            ".sha",
        ])
        .output()
        .await
        .map_err(|error| Error::GitHub(format!("failed to run gh: {error}")))?;

    if !output.status.success() {
        return Err(Error::GitHub(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::GitHub(format!(
            "unexpected commit SHA for {short}: {sha:?}"
        )));
    }
    Ok(sha)
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
    let (output, attempts) = gh_api(&[
        &format!("repos/{owner_repo}/compare/{compare}"),
        "--jq",
        ".status",
    ])
    .await
    .map_err(|e| Error::GitHub(format!("failed to run gh: {e}")))?;
    if !output.status.success() {
        return Err(Error::GitHub(format!(
            "{} (after {attempts} attempt(s))",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "identical" => Ok(RevisionOrder::Equal),
        "ahead" => Ok(RevisionOrder::Newer),
        "behind" => Ok(RevisionOrder::Older),
        "diverged" => Ok(RevisionOrder::Divergent),
        _ => Ok(RevisionOrder::Unknown),
    }
}
