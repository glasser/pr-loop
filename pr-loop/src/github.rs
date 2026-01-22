// GitHub API interactions and context detection.
// Uses `gh` CLI for repo/PR detection and API calls.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// Context about the current repository and PR.
#[derive(Debug, Clone)]
pub struct PrContext {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
}


/// Trait for GitHub operations, allowing test implementations.
pub trait GitHubClient {
    /// Detect the current repo from git context.
    fn detect_repo(&self) -> Result<(String, String)>;

    /// Detect the PR number for the current branch.
    fn detect_pr(&self, owner: &str, repo: &str) -> Result<u64>;
}

/// Real GitHub client that uses the `gh` CLI.
pub struct RealGitHubClient;

impl GitHubClient for RealGitHubClient {
    fn detect_repo(&self) -> Result<(String, String)> {
        detect_repo_from_gh()
    }

    fn detect_pr(&self, owner: &str, repo: &str) -> Result<u64> {
        detect_pr_from_gh(owner, repo)
    }
}

#[derive(Deserialize)]
struct GhRepoView {
    owner: GhOwner,
    name: String,
}

#[derive(Deserialize)]
struct GhOwner {
    login: String,
}

#[derive(Deserialize)]
struct GhPrView {
    number: u64,
}

/// Detect repo using `gh repo view --json`.
fn detect_repo_from_gh() -> Result<(String, String)> {
    let output = Command::new("gh")
        .args(["repo", "view", "--json", "owner,name"])
        .output()
        .context("Failed to run 'gh repo view'. Is this a git repository?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to detect repository: {}", stderr.trim());
    }

    let view: GhRepoView =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh repo view output")?;

    Ok((view.owner.login, view.name))
}

/// Detect PR for current branch using `gh pr view --json`.
fn detect_pr_from_gh(_owner: &str, _repo: &str) -> Result<u64> {
    // Don't pass --repo here; gh pr view auto-detects the current branch's PR
    // only when no repo is specified. With --repo, it requires an explicit PR identifier.
    let output = Command::new("gh")
        .args(["pr", "view", "--json", "number"])
        .output()
        .context("Failed to run 'gh pr view'")?;

    if !output.status.success() {
        anyhow::bail!("No PR found for current branch. Create a PR or use --pr flag.");
    }

    let view: GhPrView =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh pr view output")?;

    Ok(view.number)
}

/// Resolve PR context from CLI args and/or auto-detection.
pub fn resolve_pr_context(
    client: &dyn GitHubClient,
    repo_arg: Option<&str>,
    pr_arg: Option<u64>,
) -> Result<PrContext> {
    // Resolve repo (from arg or auto-detect)
    let (owner, repo) = if let Some(repo_str) = repo_arg {
        parse_repo_arg(repo_str)?
    } else {
        client.detect_repo()?
    };

    // Resolve PR number (from arg or auto-detect)
    let pr_number = if let Some(pr) = pr_arg {
        pr
    } else {
        client.detect_pr(&owner, &repo)?
    };

    Ok(PrContext {
        owner,
        repo,
        pr_number,
    })
}

/// Parse "owner/repo" format from CLI arg.
fn parse_repo_arg(repo_str: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = repo_str.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!(
            "Invalid repo format '{}'. Expected 'owner/repo'.",
            repo_str
        );
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test GitHub client that returns fixed values.
    pub struct TestGitHubClient {
        pub repo: Option<(String, String)>,
        pub pr_number: Option<u64>,
    }

    impl GitHubClient for TestGitHubClient {
        fn detect_repo(&self) -> Result<(String, String)> {
            self.repo
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No repo configured in test"))
        }

        fn detect_pr(&self, _owner: &str, _repo: &str) -> Result<u64> {
            self.pr_number
                .ok_or_else(|| anyhow::anyhow!("No PR configured in test"))
        }
    }

    #[test]
    fn parse_repo_arg_valid() {
        let (owner, repo) = parse_repo_arg("glasser/pr-loop-test-repo").unwrap();
        assert_eq!(owner, "glasser");
        assert_eq!(repo, "pr-loop-test-repo");
    }

    #[test]
    fn parse_repo_arg_invalid() {
        assert!(parse_repo_arg("invalid").is_err());
        assert!(parse_repo_arg("a/b/c").is_err());
    }

    #[test]
    fn resolve_with_all_args() {
        let client = TestGitHubClient {
            repo: None,
            pr_number: None,
        };

        let ctx = resolve_pr_context(&client, Some("owner/repo"), Some(42)).unwrap();
        assert_eq!(ctx.owner, "owner");
        assert_eq!(ctx.repo, "repo");
        assert_eq!(ctx.pr_number, 42);
    }

    #[test]
    fn resolve_with_auto_detect() {
        let client = TestGitHubClient {
            repo: Some(("detected-owner".to_string(), "detected-repo".to_string())),
            pr_number: Some(123),
        };

        let ctx = resolve_pr_context(&client, None, None).unwrap();
        assert_eq!(ctx.owner, "detected-owner");
        assert_eq!(ctx.repo, "detected-repo");
        assert_eq!(ctx.pr_number, 123);
    }

    #[test]
    fn resolve_mixed_args_and_detect() {
        let client = TestGitHubClient {
            repo: Some(("detected-owner".to_string(), "detected-repo".to_string())),
            pr_number: Some(999),
        };

        // Repo from arg, PR from detection
        let ctx = resolve_pr_context(&client, Some("arg-owner/arg-repo"), None).unwrap();
        assert_eq!(ctx.owner, "arg-owner");
        assert_eq!(ctx.repo, "arg-repo");
        assert_eq!(ctx.pr_number, 999);
    }

}
