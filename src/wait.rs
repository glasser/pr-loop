// Wait-until-actionable polling logic.
// Blocks until PR state changes to something requiring action.

use crate::checks::{CheckStatus, ChecksClient, ChecksSummary};
use crate::git::GitClient;
use crate::threads::{ThreadsClient, CLAUDE_MARKER};
use anyhow::Result;
use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Snapshot of PR state for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    /// IDs of threads that need a response (unresolved, last comment not from Claude)
    pub actionable_thread_ids: HashSet<String>,
    /// IDs of all unresolved threads (regardless of who commented last)
    pub unresolved_thread_ids: HashSet<String>,
    /// Names of failed CI checks
    pub failed_check_names: HashSet<String>,
    /// Names of pending CI checks
    pub pending_check_names: HashSet<String>,
}

impl PrSnapshot {
    /// Returns true if the PR is currently actionable (needs work).
    pub fn is_actionable(&self) -> bool {
        !self.actionable_thread_ids.is_empty() || !self.failed_check_names.is_empty()
    }

    /// Returns true if CI is "happy" - all checks passed, none pending or failed.
    pub fn is_ci_happy(&self) -> bool {
        self.failed_check_names.is_empty() && self.pending_check_names.is_empty()
    }

    /// Returns true if the PR is "happy" - CI passing and no actionable comments.
    pub fn is_happy(&self) -> bool {
        self.is_ci_happy() && self.actionable_thread_ids.is_empty()
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

    let pending_check_names: HashSet<String> = checks_summary
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Pending)
        .map(|c| c.name.clone())
        .collect();

    // Fetch threads, excluding paperclip threads (preserved for human review)
    let threads: Vec<_> = threads_client
        .fetch_threads(owner, repo, pr_number)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| !t.has_paperclip())
        .collect();

    // All unresolved threads (regardless of who commented last)
    let unresolved_thread_ids: HashSet<String> = threads
        .iter()
        .filter(|t| !t.is_resolved)
        .map(|t| t.id.clone())
        .collect();

    // Actionable threads (unresolved AND last comment not from Claude)
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
        unresolved_thread_ids,
        failed_check_names,
        pending_check_names,
    })
}

/// Result of waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    /// PR became actionable (has work to do)
    Actionable,
    /// PR is "happy" (CI passing, no comments needing response)
    Happy,
    /// Timeout reached
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

/// Wait until PR is actionable or "happy" (CI passing, no comments, min time since last push).
/// Returns Happy when the PR is in a good state, Actionable if work is needed, or Timeout.
pub fn wait_until_actionable_or_happy(
    checks_client: &dyn ChecksClient,
    threads_client: &dyn ThreadsClient,
    git_client: &dyn GitClient,
    owner: &str,
    repo: &str,
    pr_number: u64,
    include_patterns: &[String],
    exclude_patterns: &[String],
    timeout_secs: u64,
    poll_interval_secs: u64,
    min_wait_after_push_secs: u64,
) -> Result<WaitResult> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(poll_interval_secs);
    let min_wait_after_push = Duration::from_secs(min_wait_after_push_secs);

    eprintln!(
        "Waiting for PR to become actionable or happy (timeout: {}s, polling every {}s)...",
        timeout_secs, poll_interval_secs
    );

    loop {
        if start.elapsed() >= timeout {
            return Ok(WaitResult::Timeout);
        }

        let snapshot = capture_snapshot(
            checks_client,
            threads_client,
            owner,
            repo,
            pr_number,
            include_patterns,
            exclude_patterns,
        )?;

        // If actionable (comments or failures), return immediately
        if snapshot.is_actionable() {
            return Ok(WaitResult::Actionable);
        }

        // Check if "happy": CI passing (no failures, no pending) and no comments
        if snapshot.is_happy() {
            // Also need to wait min time after last push to ensure CI has triggered
            let last_commit_time = git_client.get_last_commit_time()?;
            let elapsed_since_commit = SystemTime::now()
                .duration_since(last_commit_time)
                .unwrap_or(Duration::ZERO);

            if elapsed_since_commit >= min_wait_after_push {
                return Ok(WaitResult::Happy);
            } else {
                let remaining = min_wait_after_push - elapsed_since_commit;
                eprintln!(
                    "PR looks happy but waiting {}s more to ensure CI has triggered...",
                    remaining.as_secs()
                );
            }
        }

        thread::sleep(poll_interval);
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

        fn fetch_thread_by_comment_id(&self, comment_id: &str) -> Result<ReviewThread> {
            self.threads
                .iter()
                .find(|t| t.comments.iter().any(|c| c.id == comment_id))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Comment not found: {}", comment_id))
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
                id: format!("comment_{}", id),
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

    #[test]
    fn snapshot_is_ci_happy_all_passing() {
        let snapshot = PrSnapshot {
            actionable_thread_ids: HashSet::new(),
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: HashSet::new(),
            pending_check_names: HashSet::new(),
        };
        assert!(snapshot.is_ci_happy());
    }

    #[test]
    fn snapshot_is_ci_happy_with_pending() {
        let mut pending = HashSet::new();
        pending.insert("build".to_string());
        let snapshot = PrSnapshot {
            actionable_thread_ids: HashSet::new(),
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: HashSet::new(),
            pending_check_names: pending,
        };
        assert!(!snapshot.is_ci_happy());
    }

    #[test]
    fn snapshot_is_ci_happy_with_failures() {
        let mut failed = HashSet::new();
        failed.insert("test".to_string());
        let snapshot = PrSnapshot {
            actionable_thread_ids: HashSet::new(),
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: failed,
            pending_check_names: HashSet::new(),
        };
        assert!(!snapshot.is_ci_happy());
    }

    #[test]
    fn snapshot_is_happy_no_comments_ci_passing() {
        let snapshot = PrSnapshot {
            actionable_thread_ids: HashSet::new(),
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: HashSet::new(),
            pending_check_names: HashSet::new(),
        };
        assert!(snapshot.is_happy());
    }

    #[test]
    fn snapshot_is_happy_with_comments() {
        let mut threads = HashSet::new();
        threads.insert("T1".to_string());
        let snapshot = PrSnapshot {
            actionable_thread_ids: threads,
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: HashSet::new(),
            pending_check_names: HashSet::new(),
        };
        assert!(!snapshot.is_happy());
    }

    #[test]
    fn snapshot_is_happy_with_pending_ci() {
        let mut pending = HashSet::new();
        pending.insert("build".to_string());
        let snapshot = PrSnapshot {
            actionable_thread_ids: HashSet::new(),
            unresolved_thread_ids: HashSet::new(),
            failed_check_names: HashSet::new(),
            pending_check_names: pending,
        };
        assert!(!snapshot.is_happy());
    }

    #[test]
    fn snapshot_ignores_paperclip_threads() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        // An unresolved thread with a paperclip should be ignored
        let threads_client = TestThreadsClient {
            threads: vec![ReviewThread {
                id: "T1".to_string(),
                is_resolved: false,
                path: Some("test.rs".to_string()),
                line: Some(1),
                comments: vec![ThreadComment {
                    id: "C1".to_string(),
                    author: "reviewer".to_string(),
                    body: ":paperclip: This is for human review".to_string(),
                }],
            }],
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

        // Paperclip thread should not appear in either actionable or unresolved
        assert!(snapshot.actionable_thread_ids.is_empty());
        assert!(snapshot.unresolved_thread_ids.is_empty());
        assert!(snapshot.is_happy());
    }

    #[test]
    fn snapshot_ignores_paperclip_thread_where_only_one_comment_has_marker() {
        let checks_client = TestChecksClient {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        // Thread has paperclip in only one comment but entire thread is excluded
        let threads_client = TestThreadsClient {
            threads: vec![ReviewThread {
                id: "T1".to_string(),
                is_resolved: false,
                path: Some("test.rs".to_string()),
                line: Some(1),
                comments: vec![
                    ThreadComment {
                        id: "C1".to_string(),
                        author: "reviewer".to_string(),
                        body: "Please fix this".to_string(),
                    },
                    ThreadComment {
                        id: "C2".to_string(),
                        author: "reviewer".to_string(),
                        body: ":paperclip: But note this for human review".to_string(),
                    },
                ],
            }],
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

        assert!(snapshot.actionable_thread_ids.is_empty());
        assert!(snapshot.unresolved_thread_ids.is_empty());
    }
}
