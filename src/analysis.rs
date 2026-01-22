// PR analysis and decision engine.
// Determines the recommended next action based on PR state.

use crate::checks::ChecksSummary;
use crate::threads::{find_actionable_threads, ActionableThread, ReviewThread};

/// The recommended next action for the PR.
#[derive(Debug, Clone)]
pub enum NextAction {
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
pub fn analyze_pr(checks: &ChecksSummary, threads: Vec<ReviewThread>) -> NextAction {
    let actionable_threads = find_actionable_threads(threads);
    let failed_checks = checks.failed();
    let pending_checks = checks.pending();

    // Priority 1: Respond to review comments
    if !actionable_threads.is_empty() {
        return NextAction::RespondToComments {
            threads: actionable_threads,
            also_has_ci_failures: !failed_checks.is_empty(),
            ci_pending: !pending_checks.is_empty(),
        };
    }

    // Priority 2: Fix CI failures
    if !failed_checks.is_empty() {
        return NextAction::FixCiFailures {
            failed_check_names: failed_checks.iter().map(|c| c.name.clone()).collect(),
        };
    }

    // Priority 3: Wait for CI
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
        }
    }

    fn make_thread(id: &str, resolved: bool, comments: Vec<ThreadComment>) -> ReviewThread {
        ReviewThread {
            id: id.to_string(),
            is_resolved: resolved,
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
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

        match analyze_pr(&checks, threads) {
            NextAction::FixCiFailures { .. } => {}
            other => panic!("Expected FixCiFailures, got {:?}", other),
        }
    }
}
