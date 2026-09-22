// PR analysis and decision engine.
// Determines the recommended next action based on PR state.

use crate::checks::ChecksSummary;
use crate::commit_edits::CommitEditRequest;
use crate::threads::{find_actionable_threads, ActionableThread, ReviewThread};

/// The recommended next action for the PR.
#[derive(Debug, Clone)]
pub enum NextAction {
    /// A human has requested one or more commit messages be reworded (via
    /// the web UI's click-to-edit, or by hand). These are quick, mechanical,
    /// and don't require judgment, so they take priority over everything
    /// else — get them out of the way first.
    RewordCommits {
        requests: Vec<CommitEditRequest>,
    },
    /// There are review comments that need a response.
    RespondToComments {
        threads: Vec<ActionableThread>,
        /// True if there are also CI failures to be aware of.
        also_has_ci_failures: bool,
        /// True if CI is still pending.
        ci_pending: bool,
    },
    /// CI has failed and there are no pending review comments.
    FixCiFailures {
        failed_check_names: Vec<String>,
    },
    /// CI is still running, no other action needed.
    WaitForCi {
        pending_check_names: Vec<String>,
    },
    /// Everything is good - all checks passed, no pending comments.
    PrReady,
}

/// Analyze PR state and determine the next action.
pub fn analyze_pr(
    checks: &ChecksSummary,
    threads: Vec<ReviewThread>,
    pending_reword_requests: Vec<CommitEditRequest>,
) -> NextAction {
    let actionable_threads = find_actionable_threads(threads);
    let failed_checks = checks.failed();
    let pending_checks = checks.pending();

    // Priority 1: Reword requested commit messages
    if !pending_reword_requests.is_empty() {
        return NextAction::RewordCommits {
            requests: pending_reword_requests,
        };
    }

    // Priority 2: Respond to review comments
    if !actionable_threads.is_empty() {
        return NextAction::RespondToComments {
            threads: actionable_threads,
            also_has_ci_failures: !failed_checks.is_empty(),
            ci_pending: !pending_checks.is_empty(),
        };
    }

    // Priority 3: Fix CI failures
    if !failed_checks.is_empty() {
        return NextAction::FixCiFailures {
            failed_check_names: failed_checks.iter().map(|c| c.name.clone()).collect(),
        };
    }

    // Priority 4: Wait for CI
    if !pending_checks.is_empty() {
        return NextAction::WaitForCi {
            pending_check_names: pending_checks.iter().map(|c| c.name.clone()).collect(),
        };
    }

    // All good!
    NextAction::PrReady
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{Check, CheckStatus};
    use crate::threads::ThreadComment;

    fn make_check(name: &str, status: CheckStatus) -> Check {
        Check {
            name: name.to_string(),
            status,
            url: None,
        }
    }

    fn make_comment(author: &str, body: &str) -> ThreadComment {
        ThreadComment {
            id: format!("comment_{}", body.len()),
            author: author.to_string(),
            body: body.to_string(),
            diff_hunk: None,
            url: None,
            created_at: None,
        }
    }

    fn make_thread(id: &str, resolved: bool, comments: Vec<ThreadComment>) -> ReviewThread {
        ReviewThread {
            id: id.to_string(),
            is_resolved: resolved,
            is_outdated: false,
            path: Some("src/main.rs".to_string()),
            line: Some(42),
            comments,
        }
    }

    #[test]
    fn analyze_pr_ready() {
        let checks = ChecksSummary {
            checks: vec![
                make_check("build", CheckStatus::Pass),
                make_check("test", CheckStatus::Pass),
            ],
        };
        let threads = vec![]; // No threads

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::PrReady => {}
            other => panic!("Expected PrReady, got {:?}", other),
        }
    }

    #[test]
    fn analyze_pr_ready_with_resolved_threads() {
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads = vec![make_thread(
            "T1",
            true,
            vec![make_comment("reviewer", "Looks good!")],
        )];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::PrReady => {}
            other => panic!("Expected PrReady, got {:?}", other),
        }
    }

    #[test]
    fn analyze_respond_to_comments() {
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads = vec![make_thread(
            "T1",
            false,
            vec![make_comment("reviewer", "Please fix this")],
        )];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::RespondToComments {
                threads,
                also_has_ci_failures,
                ci_pending,
            } => {
                assert_eq!(threads.len(), 1);
                assert!(!also_has_ci_failures);
                assert!(!ci_pending);
            }
            other => panic!("Expected RespondToComments, got {:?}", other),
        }
    }

    #[test]
    fn analyze_respond_with_ci_failures() {
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Fail)],
        };
        let threads = vec![make_thread(
            "T1",
            false,
            vec![make_comment("reviewer", "Question?")],
        )];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::RespondToComments {
                also_has_ci_failures,
                ..
            } => {
                assert!(also_has_ci_failures);
            }
            other => panic!("Expected RespondToComments, got {:?}", other),
        }
    }

    #[test]
    fn analyze_fix_ci_failures() {
        let checks = ChecksSummary {
            checks: vec![
                make_check("build", CheckStatus::Pass),
                make_check("test", CheckStatus::Fail),
            ],
        };
        let threads = vec![]; // No actionable threads

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::FixCiFailures { failed_check_names } => {
                assert_eq!(failed_check_names, vec!["test"]);
            }
            other => panic!("Expected FixCiFailures, got {:?}", other),
        }
    }

    #[test]
    fn analyze_wait_for_ci() {
        let checks = ChecksSummary {
            checks: vec![
                make_check("build", CheckStatus::Pass),
                make_check("test", CheckStatus::Pending),
            ],
        };
        let threads = vec![];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::WaitForCi { pending_check_names } => {
                assert_eq!(pending_check_names, vec!["test"]);
            }
            other => panic!("Expected WaitForCi, got {:?}", other),
        }
    }

    #[test]
    fn analyze_comments_take_priority_over_ci() {
        // Even with CI failures, responding to comments is highest priority
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Fail)],
        };
        let threads = vec![make_thread(
            "T1",
            false,
            vec![make_comment("reviewer", "Fix this")],
        )];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::RespondToComments { .. } => {}
            other => panic!("Expected RespondToComments, got {:?}", other),
        }
    }

    #[test]
    fn analyze_ci_failures_over_pending() {
        // CI failures take priority over pending
        let checks = ChecksSummary {
            checks: vec![
                make_check("build", CheckStatus::Fail),
                make_check("test", CheckStatus::Pending),
            ],
        };
        let threads = vec![];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::FixCiFailures { .. } => {}
            other => panic!("Expected FixCiFailures, got {:?}", other),
        }
    }

    #[test]
    fn analyze_ignores_paperclip_thread() {
        // A paperclip thread should not be treated as actionable
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads = vec![make_thread(
            "T1",
            false,
            vec![make_comment("reviewer", ":paperclip: For human review only")],
        )];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::PrReady => {}
            other => panic!("Expected PrReady, got {:?}", other),
        }
    }

    #[test]
    fn analyze_ignores_paperclip_but_sees_normal_threads() {
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        let threads = vec![
            make_thread(
                "T1",
                false,
                vec![make_comment("reviewer", ":paperclip: Sticky note for human")],
            ),
            make_thread(
                "T2",
                false,
                vec![make_comment("reviewer", "Please fix this")],
            ),
        ];

        match analyze_pr(&checks, threads, vec![]) {
            NextAction::RespondToComments { threads, .. } => {
                assert_eq!(threads.len(), 1);
                assert_eq!(threads[0].thread.id, "T2");
            }
            other => panic!("Expected RespondToComments, got {:?}", other),
        }
    }

    fn make_reword_request(sha: &str) -> CommitEditRequest {
        CommitEditRequest {
            comment_id: format!("comment_{}", sha),
            sha: sha.to_string(),
            new_message: "Better message".to_string(),
        }
    }

    #[test]
    fn analyze_reword_commits_over_everything_else() {
        // A pending reword request wins even with CI failures and review
        // comments also pending — it's quick and mechanical, so clear it first.
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Fail)],
        };
        let threads = vec![make_thread(
            "T1",
            false,
            vec![make_comment("reviewer", "Please fix this")],
        )];
        let reword_requests = vec![make_reword_request("abc123")];

        match analyze_pr(&checks, threads, reword_requests) {
            NextAction::RewordCommits { requests } => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].sha, "abc123");
            }
            other => panic!("Expected RewordCommits, got {:?}", other),
        }
    }

    #[test]
    fn analyze_no_reword_requests_falls_through() {
        let checks = ChecksSummary {
            checks: vec![make_check("build", CheckStatus::Pass)],
        };
        match analyze_pr(&checks, vec![], vec![]) {
            NextAction::PrReady => {}
            other => panic!("Expected PrReady, got {:?}", other),
        }
    }
}
