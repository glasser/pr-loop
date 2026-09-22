// Commit message reword requests, modeled as plain PR (issue) comments with
// a structured marker so they survive between `pr-loop` invocations without
// needing any local state. Posted by a human (typically via the web UI's
// click-to-edit on a commit), consumed by Claude via `pr-loop reword-commit`.
//
// Unlike review threads, GitHub's GraphQL API has no mutation to create a
// top-level PR comment scoped the way we need, so this uses the REST API
// (`gh api /repos/.../issues/...`) instead — `gh_actions.rs` already does
// the same for GitHub Actions data.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// Every reword-request comment body starts with this, followed by the full
/// commit SHA, a blank line, then the new commit message verbatim.
pub const REWORD_MARKER_PREFIX: &str = "🔧 pr-loop: reword commit ";

/// A pending request to reword one commit's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEditRequest {
    /// The REST comment ID (not a GraphQL node ID) — passed back to
    /// `delete_request` once the reword has been applied.
    pub comment_id: String,
    /// The full SHA of the commit to reword, as it existed when the request
    /// was filed. May no longer be reachable if the branch was rebased since.
    pub sha: String,
    pub new_message: String,
}

/// Build the body for a new reword-request comment.
pub fn format_reword_request(sha: &str, new_message: &str) -> String {
    format!("{}{}\n\n{}", REWORD_MARKER_PREFIX, sha, new_message)
}

/// Parse a comment body back into (sha, new_message), if it's a reword
/// request. Returns None for any other comment.
pub fn parse_reword_request(body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix(REWORD_MARKER_PREFIX)?;
    let (first_line, remainder) = rest.split_once('\n')?;
    let sha = first_line.trim();
    if sha.is_empty() {
        return None;
    }
    let message = remainder.trim_start_matches('\n');
    Some((sha.to_string(), message.to_string()))
}

/// Trait for reading/writing commit-reword requests, allowing test
/// implementations.
pub trait CommitEditsClient {
    /// Fetch all pending reword requests currently posted on the PR.
    fn fetch_pending(&self, owner: &str, repo: &str, pr_number: u64) -> Result<Vec<CommitEditRequest>>;

    /// File a new reword request.
    fn post_request(&self, owner: &str, repo: &str, pr_number: u64, sha: &str, new_message: &str) -> Result<()>;

    /// Remove a request (once applied, or if withdrawn).
    fn delete_request(&self, owner: &str, repo: &str, comment_id: &str) -> Result<()>;
}

/// Real client that uses `gh api` against the REST API.
pub struct RealCommitEditsClient;

#[derive(Deserialize)]
struct IssueComment {
    id: u64,
    body: String,
}

impl CommitEditsClient for RealCommitEditsClient {
    fn fetch_pending(&self, owner: &str, repo: &str, pr_number: u64) -> Result<Vec<CommitEditRequest>> {
        // Not paginated beyond the first 100 comments — PRs with more total
        // conversation comments than that are rare enough not to be worth
        // the extra complexity here.
        let path = format!("/repos/{}/{}/issues/{}/comments?per_page=100", owner, repo, pr_number);
        let output = Command::new("gh")
            .args(["api", &path])
            .output()
            .context("Failed to run 'gh api' for issue comments")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api issue comments failed: {}", stderr.trim());
        }

        let comments: Vec<IssueComment> =
            serde_json::from_slice(&output.stdout).context("parse issue comments")?;

        Ok(comments
            .into_iter()
            .filter_map(|c| {
                let (sha, new_message) = parse_reword_request(&c.body)?;
                Some(CommitEditRequest {
                    comment_id: c.id.to_string(),
                    sha,
                    new_message,
                })
            })
            .collect())
    }

    fn post_request(&self, owner: &str, repo: &str, pr_number: u64, sha: &str, new_message: &str) -> Result<()> {
        let path = format!("/repos/{}/{}/issues/{}/comments", owner, repo, pr_number);
        let body = format_reword_request(sha, new_message);
        let output = Command::new("gh")
            .args(["api", &path, "-f", &format!("body={}", body)])
            .output()
            .context("Failed to run 'gh api' to post reword request")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api post reword request failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn delete_request(&self, owner: &str, repo: &str, comment_id: &str) -> Result<()> {
        let path = format!("/repos/{}/{}/issues/comments/{}", owner, repo, comment_id);
        let output = Command::new("gh")
            .args(["api", "-X", "DELETE", &path])
            .output()
            .context("Failed to run 'gh api' to delete reword request")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api delete reword request failed: {}", stderr.trim());
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct TestCommitEditsClient {
        pub pending: Vec<CommitEditRequest>,
        pub posted: Mutex<Vec<(String, String)>>,
        pub deleted: Mutex<Vec<String>>,
        pub fail: bool,
    }

    impl CommitEditsClient for TestCommitEditsClient {
        fn fetch_pending(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<Vec<CommitEditRequest>> {
            if self.fail {
                anyhow::bail!("Test failure");
            }
            Ok(self.pending.clone())
        }

        fn post_request(&self, _owner: &str, _repo: &str, _pr_number: u64, sha: &str, new_message: &str) -> Result<()> {
            if self.fail {
                anyhow::bail!("Test failure");
            }
            self.posted.lock().unwrap().push((sha.to_string(), new_message.to_string()));
            Ok(())
        }

        fn delete_request(&self, _owner: &str, _repo: &str, comment_id: &str) -> Result<()> {
            if self.fail {
                anyhow::bail!("Test failure");
            }
            self.deleted.lock().unwrap().push(comment_id.to_string());
            Ok(())
        }
    }

    #[test]
    fn format_and_parse_roundtrip() {
        let body = format_reword_request("abc123", "New message\n\nwith a body");
        let (sha, message) = parse_reword_request(&body).unwrap();
        assert_eq!(sha, "abc123");
        assert_eq!(message, "New message\n\nwith a body");
    }

    #[test]
    fn format_and_parse_single_line() {
        let body = format_reword_request("deadbeef", "Just a headline");
        let (sha, message) = parse_reword_request(&body).unwrap();
        assert_eq!(sha, "deadbeef");
        assert_eq!(message, "Just a headline");
    }

    #[test]
    fn parse_rejects_unrelated_comment() {
        assert!(parse_reword_request("Looks good to me!").is_none());
    }

    #[test]
    fn parse_rejects_missing_sha() {
        // Marker present but no newline after it (malformed).
        assert!(parse_reword_request(REWORD_MARKER_PREFIX).is_none());
    }

    #[test]
    fn parse_rejects_empty_sha() {
        let body = format!("{}\n\nmessage", REWORD_MARKER_PREFIX);
        assert!(parse_reword_request(&body).is_none());
    }

    #[test]
    fn test_client_records_post_and_delete() {
        let client = TestCommitEditsClient::default();
        client.post_request("o", "r", 1, "sha1", "msg").unwrap();
        client.delete_request("o", "r", "123").unwrap();
        assert_eq!(*client.posted.lock().unwrap(), vec![("sha1".to_string(), "msg".to_string())]);
        assert_eq!(*client.deleted.lock().unwrap(), vec!["123".to_string()]);
    }

    #[test]
    fn test_client_fetch_pending() {
        let client = TestCommitEditsClient {
            pending: vec![CommitEditRequest {
                comment_id: "1".to_string(),
                sha: "abc".to_string(),
                new_message: "fix typo".to_string(),
            }],
            ..Default::default()
        };
        let pending = client.fetch_pending("o", "r", 1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sha, "abc");
    }
}
