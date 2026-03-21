use crate::error::{Error, Result};

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
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(Error::GitHub(stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|l| l.trim().to_string()).collect())
}
