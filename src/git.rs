// Git operations for detecting push/commit times.
// Uses git CLI to get commit timestamps.

use anyhow::{Context, Result};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Trait for git operations, allowing test implementations.
pub trait GitClient {
    /// Get the timestamp of the last commit on the current branch.
    fn get_last_commit_time(&self) -> Result<SystemTime>;

    /// Get the hash of HEAD. Used to detect local ref changes cheaply.
    fn get_head_hash(&self) -> Result<String>;
}

/// Real git client that uses the `git` CLI.
pub struct RealGitClient;

impl GitClient for RealGitClient {
    fn get_last_commit_time(&self) -> Result<SystemTime> {
        get_last_commit_time_from_git()
    }

    fn get_head_hash(&self) -> Result<String> {
        get_head_hash_from_git()
    }
}

fn get_head_hash_from_git() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("Failed to run 'git rev-parse'")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get HEAD hash: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Get the Unix timestamp of the last commit using `git log`.
fn get_last_commit_time_from_git() -> Result<SystemTime> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct"])
        .output()
        .context("Failed to run 'git log'. Is this a git repository?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get last commit time: {}", stderr.trim());
    }

    let timestamp_str = String::from_utf8_lossy(&output.stdout);
    let timestamp: u64 = timestamp_str
        .trim()
        .parse()
        .context("Failed to parse commit timestamp")?;

    Ok(UNIX_EPOCH + Duration::from_secs(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test git client that returns a fixed timestamp.
    pub struct TestGitClient {
        pub last_commit_time: SystemTime,
        pub head_hash: String,
    }

    impl GitClient for TestGitClient {
        fn get_last_commit_time(&self) -> Result<SystemTime> {
            Ok(self.last_commit_time)
        }
        fn get_head_hash(&self) -> Result<String> {
            Ok(self.head_hash.clone())
        }
    }

    #[test]
    fn test_git_client_returns_time() {
        let now = SystemTime::now();
        let client = TestGitClient {
            last_commit_time: now,
            head_hash: "abc123".to_string(),
        };
        let result = client.get_last_commit_time().unwrap();
        assert_eq!(result, now);
    }
}
