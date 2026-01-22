// Wait-until-actionable polling logic.
// Blocks until PR state changes to something requiring action.

use crate::checks::{CheckStatus, ChecksClient, ChecksSummary};
use crate::threads::{ThreadsClient, CLAUDE_MARKER};
use anyhow::Result;
use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

/// Snapshot of PR state for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    /// IDs of threads that need a response (unresolved, last comment not from Claude)
    pub actionable_thread_ids: HashSet<String>,
    /// Names of failed CI checks
    pub failed_check_names: HashSet<String>,
}

impl PrSnapshot {
    /// Returns true if the PR is currently actionable.
    pub fn is_actionable(&self) -> bool {
        !self.actionable_thread_ids.is_empty() || !self.failed_check_names.is_empty()
    }
}

/// Capture current PR state as a snapshot.
pub fn capture_snapshot(
    checks_client: &dyn ChecksClient,
    threads_client: &dyn ThreadsClient,
    owner: &str,
    repo: &str,
    pr_number: u64,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<PrSnapshot> {
    // Fetch checks
    let checks = checks_client.fetch_checks(owner, repo, pr_number).unwrap_or_default();
    let filtered = crate::checks::filter_checks(checks, include_patterns, exclude_patterns)?;
    let checks_summary = ChecksSummary { checks: filtered };

    let failed_check_names: HashSet<String> = checks_summary
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.name.clone())
        .collect();

    // Fetch threads
    let threads = threads_client
        .fetch_threads(owner, repo, pr_number)
        .unwrap_or_default();

    let actionable_thread_ids: HashSet<String> = threads
        .into_iter()
        .filter(|t| {
            if t.is_resolved {
                return false;
            }
            match t.comments.last() {
                Some(comment) => !comment.body.starts_with(CLAUDE_MARKER),
                None => false,
            }
        })
        .map(|t| t.id)
        .collect();

    Ok(PrSnapshot {
        actionable_thread_ids,
        failed_check_names,
    })
}

/// Result of waiting for actionable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    /// PR became actionable
    Actionable,
    /// Timeout reached without becoming actionable
    Timeout,
}

/// Wait until PR becomes actionable or timeout is reached.
pub fn wait_until_actionable(
    checks_client: &dyn ChecksClient,
    threads_client: &dyn ThreadsClient,
    owner: &str,
    repo: &str,
    pr_number: u64,
    include_patterns: &[String],
    exclude_patterns: &[String],
    timeout_secs: u64,
    poll_interval_secs: u64,
) -> Result<WaitResult> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(poll_interval_secs);

    // Check immediately first
    let snapshot = capture_snapshot(
        checks_client,
        threads_client,
        owner,
        repo,
        pr_number,
        include_patterns,
        exclude_patterns,
    )?;

    if snapshot.is_actionable() {
        return Ok(WaitResult::Actionable);
    }

    eprintln!(
        "Waiting for PR to become actionable (timeout: {}s, polling every {}s)...",
        timeout_secs, poll_interval_secs
    );

    loop {
        if start.elapsed() >= timeout {
            return Ok(WaitResult::Timeout);
        }

        thread::sleep(poll_interval);

        let snapshot = capture_snapshot(
            checks_client,
            threads_client,
            owner,
            repo,
            pr_number,
            include_patterns,
            exclude_patterns,
        )?;

        if snapshot.is_actionable() {
            return Ok(WaitResult::Actionable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{Check, CheckStatus};
    use crate::threads::{ReviewThread, ThreadComment};

    struct TestChecksClient {
        checks: Vec<Check>,
    }

    impl ChecksClient for TestChecksClient {
        fn fetch_checks(&self, _owner: &str, _repo: &str, _pr: u64) -> Result<Vec<Check>> {
            Ok(self.checks.clone())
        }
    }

    struct TestThreadsClient {
        threads: Vec<ReviewThread>,
    }

    impl ThreadsClient for TestThreadsClient {
        fn fetch_threads(&self, _owner: &str, _repo: &str, _pr: u64) -> Result<Vec<ReviewThread>> {
            Ok(self.threads.clone())
        }
    }

    fn make_check(name: &str, status: CheckStatus) -> Check {
        Check {
            name: name.to_string(),
            status,
            url: None,
        }
    }

    fn make_thread(id: &str, resolved: bool, last_comment_body: &str) -> ReviewThread {
        ReviewThread {
            id: id.to_string(),
            is_resolved: resolved,
            path: Some("test.rs".to_string()),
            line: Some(1),
            comments: vec![ThreadComment {
                author: "reviewer".to_string(),
                body: last_comment_body.to_string(),
            }],
        }
    }

    #[test]
    fn snapshot_actionable_with_failed_checks() {
        let checks_client = TestChecksClient {
            checks: vec![
                make_check("build", CheckStatus::Pass),
                make_check("test", CheckStatus::Fail),
            ],
        };
        let threads_client = TestThreadsClient { threads: vec![] };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(snapshot.is_actionable());
        assert!(snapshot.failed_check_names.contains("test"));
        assert!(snapshot.actionable_thread_ids.is_empty());
    }

    #[test]
    fn snapshot_actionable_with_unresolved_thread() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads_client = TestThreadsClient {
            threads: vec![make_thread("T1", false, "Please fix this")],
        };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(snapshot.is_actionable());
        assert!(snapshot.actionable_thread_ids.contains("T1"));
    }

    #[test]
    fn snapshot_not_actionable_resolved_thread() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads_client = TestThreadsClient {
            threads: vec![make_thread("T1", true, "Please fix this")],
        };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(!snapshot.is_actionable());
    }

    #[test]
    fn snapshot_not_actionable_claude_replied() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads_client = TestThreadsClient {
            threads: vec![make_thread("T1", false, "🤖 From Claude: Fixed!")],
        };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(!snapshot.is_actionable());
    }

    #[test]
    fn snapshot_not_actionable_all_passing() {
        let checks_client = TestChecksClient {
            checks: vec![
                make_check("build", CheckStatus::Pass),
                make_check("test", CheckStatus::Pass),
            ],
        };
        let threads_client = TestThreadsClient { threads: vec![] };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(!snapshot.is_actionable());
    }

    #[test]
    fn snapshot_pending_checks_not_actionable() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pending)],
        };
        let threads_client = TestThreadsClient { threads: vec![] };

        let snapshot = capture_snapshot(
            &checks_client,
            &threads_client,
            "owner",
            "repo",
            1,
            &[],
            &[],
        )
        .unwrap();

        assert!(!snapshot.is_actionable());
    }
}
