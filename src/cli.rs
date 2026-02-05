// CLI argument parsing using clap.
// Defines the command-line interface for pr-loop.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "pr-loop")]
#[command(about = "CLI tool to help Claude Code manage PR workflows")]
#[command(version)]
pub struct Cli {
    /// GitHub repository in OWNER/REPO format (auto-detected if not specified)
    #[arg(long, global = true)]
    pub repo: Option<String>,

    /// Pull request number (auto-detected if not specified)
    #[arg(long, global = true)]
    pub pr: Option<u64>,

    /// Glob pattern for CI checks to include (can be repeated)
    #[arg(long = "include-checks", global = true, env = "PR_LOOP_INCLUDE_CHECKS", value_delimiter = ',')]
    pub include_checks: Vec<String>,

    /// Glob pattern for CI checks to exclude (can be repeated)
    #[arg(long = "exclude-checks", global = true, env = "PR_LOOP_EXCLUDE_CHECKS", value_delimiter = ',')]
    pub exclude_checks: Vec<String>,

    /// Wait until the PR becomes actionable (has comments needing response or CI failures)
    #[arg(long, conflicts_with = "wait_until_actionable_or_happy")]
    pub wait_until_actionable: bool,

    /// Wait until PR is "happy" (CI passing, no comments) or actionable. Exits successfully
    /// when CI passes with no unaddressed comments (after waiting for CI to trigger).
    #[arg(long, conflicts_with = "wait_until_actionable")]
    pub wait_until_actionable_or_happy: bool,

    /// Timeout in seconds for wait modes (default: 1800 = 30 minutes)
    #[arg(long, default_value = "1800")]
    pub timeout: u64,

    /// Polling interval in seconds for wait modes (default: 5)
    #[arg(long, default_value = "5")]
    pub poll_interval: u64,

    /// Minimum seconds to wait after last push before considering PR "happy" (default: 30)
    #[arg(long, default_value = "30")]
    pub min_wait_after_push: u64,

    /// Maintain a status block in the PR description indicating LLM iteration is in progress.
    /// Requires the PR to be in draft mode.
    #[arg(long)]
    pub maintain_status: bool,

    /// Custom status message to include in the PR description status block.
    /// Only used when --maintain-status is set.
    #[arg(long)]
    pub status_message: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Reply to a review thread comment
    Reply {
        /// The comment ID this reply is responding to. The thread is derived from
        /// the comment. If there are newer human comments after this one, an
        /// acknowledgment will be added and those comments will be printed for
        /// the invoker to address.
        #[arg(long)]
        in_reply_to: String,

        /// The message to post (will be prefixed with "🤖 From Claude:")
        #[arg(long)]
        message: String,
    },

    /// Mark the PR as ready for review.
    /// Validates the PR is happy (CI passing, no unresolved threads), removes the status block,
    /// and marks the PR as non-draft.
    Ready {
        /// Preserve review threads where all comments are from Claude (resolved threads only).
        /// By default, these are deleted as they are typically noise from the LLM iteration process.
        #[arg(long)]
        preserve_claude_threads: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serial_test::serial;

    #[test]
    fn verify_cli() {
        // Verifies that the CLI is well-formed
        Cli::command().debug_assert();
    }

    #[test]
    #[serial]
    fn parse_no_args() {
        // Clear env vars to ensure clean state
        // SAFETY: Test is serialized via #[serial]
        unsafe {
            std::env::remove_var("PR_LOOP_INCLUDE_CHECKS");
            std::env::remove_var("PR_LOOP_EXCLUDE_CHECKS");
        }

        let cli = Cli::parse_from(["pr-loop"]);
        assert!(cli.repo.is_none());
        assert!(cli.pr.is_none());
        assert!(cli.include_checks.is_empty());
        assert!(cli.exclude_checks.is_empty());
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_with_repo_and_pr() {
        let cli = Cli::parse_from(["pr-loop", "--repo", "owner/repo", "--pr", "123"]);
        assert_eq!(cli.repo, Some("owner/repo".to_string()));
        assert_eq!(cli.pr, Some(123));
    }

    #[test]
    fn parse_with_check_filters() {
        let cli = Cli::parse_from([
            "pr-loop",
            "--include-checks",
            "ci/*",
            "--include-checks",
            "build",
            "--exclude-checks",
            "lint",
        ]);
        assert_eq!(cli.include_checks, vec!["ci/*", "build"]);
        assert_eq!(cli.exclude_checks, vec!["lint"]);
    }

    #[test]
    fn parse_reply_command() {
        let cli = Cli::parse_from([
            "pr-loop",
            "reply",
            "--in-reply-to",
            "PRRC_456",
            "--message",
            "Fixed the issue",
        ]);
        match cli.command {
            Some(Command::Reply { in_reply_to, message }) => {
                assert_eq!(in_reply_to, "PRRC_456");
                assert_eq!(message, "Fixed the issue");
            }
            _ => panic!("Expected Reply command"),
        }
    }

    #[test]
    fn global_args_work_with_subcommand() {
        let cli = Cli::parse_from([
            "pr-loop",
            "--repo",
            "foo/bar",
            "--pr",
            "42",
            "reply",
            "--in-reply-to",
            "C1",
            "--message",
            "msg",
        ]);
        assert_eq!(cli.repo, Some("foo/bar".to_string()));
        assert_eq!(cli.pr, Some(42));
        assert!(matches!(cli.command, Some(Command::Reply { .. })));
    }

    #[test]
    #[serial]
    fn parse_check_filters_from_env() {
        // SAFETY: Test is serialized via #[serial]
        unsafe {
            std::env::set_var("PR_LOOP_INCLUDE_CHECKS", "ci/*,build");
            std::env::set_var("PR_LOOP_EXCLUDE_CHECKS", "lint,codecov/*");
        }

        let cli = Cli::parse_from(["pr-loop"]);
        assert_eq!(cli.include_checks, vec!["ci/*", "build"]);
        assert_eq!(cli.exclude_checks, vec!["lint", "codecov/*"]);

        // Clean up
        unsafe {
            std::env::remove_var("PR_LOOP_INCLUDE_CHECKS");
            std::env::remove_var("PR_LOOP_EXCLUDE_CHECKS");
        }
    }

    #[test]
    #[serial]
    fn cli_args_override_env() {
        // SAFETY: Test is serialized via #[serial]
        unsafe {
            std::env::set_var("PR_LOOP_INCLUDE_CHECKS", "from-env");
        }

        let cli = Cli::parse_from(["pr-loop", "--include-checks", "from-cli"]);
        assert_eq!(cli.include_checks, vec!["from-cli"]);

        // Clean up
        unsafe {
            std::env::remove_var("PR_LOOP_INCLUDE_CHECKS");
        }
    }

    #[test]
    fn parse_wait_until_actionable() {
        let cli = Cli::parse_from(["pr-loop", "--wait-until-actionable"]);
        assert!(cli.wait_until_actionable);
        assert_eq!(cli.timeout, 1800); // default 30 minutes
        assert_eq!(cli.poll_interval, 5); // default 5 seconds
    }

    #[test]
    fn parse_wait_with_custom_timeout() {
        let cli = Cli::parse_from([
            "pr-loop",
            "--wait-until-actionable",
            "--timeout",
            "600",
            "--poll-interval",
            "10",
        ]);
        assert!(cli.wait_until_actionable);
        assert_eq!(cli.timeout, 600);
        assert_eq!(cli.poll_interval, 10);
    }

    #[test]
    fn parse_wait_until_actionable_or_happy() {
        let cli = Cli::parse_from(["pr-loop", "--wait-until-actionable-or-happy"]);
        assert!(cli.wait_until_actionable_or_happy);
        assert!(!cli.wait_until_actionable);
        assert_eq!(cli.min_wait_after_push, 30); // default 30 seconds
    }

    #[test]
    fn parse_wait_until_actionable_or_happy_with_min_wait() {
        let cli = Cli::parse_from([
            "pr-loop",
            "--wait-until-actionable-or-happy",
            "--min-wait-after-push",
            "60",
        ]);
        assert!(cli.wait_until_actionable_or_happy);
        assert_eq!(cli.min_wait_after_push, 60);
    }

    #[test]
    fn parse_maintain_status() {
        let cli = Cli::parse_from(["pr-loop", "--maintain-status"]);
        assert!(cli.maintain_status);
        assert!(cli.status_message.is_none());
    }

    #[test]
    fn parse_maintain_status_with_message() {
        let cli = Cli::parse_from([
            "pr-loop",
            "--maintain-status",
            "--status-message",
            "Struggling with CI failures",
        ]);
        assert!(cli.maintain_status);
        assert_eq!(
            cli.status_message,
            Some("Struggling with CI failures".to_string())
        );
    }

    #[test]
    fn parse_ready_command() {
        let cli = Cli::parse_from(["pr-loop", "ready"]);
        match cli.command {
            Some(Command::Ready { preserve_claude_threads }) => {
                assert!(!preserve_claude_threads);
            }
            _ => panic!("Expected Ready command"),
        }
    }

    #[test]
    fn parse_ready_command_with_global_args() {
        let cli = Cli::parse_from(["pr-loop", "--repo", "owner/repo", "--pr", "123", "ready"]);
        assert_eq!(cli.repo, Some("owner/repo".to_string()));
        assert_eq!(cli.pr, Some(123));
        match cli.command {
            Some(Command::Ready { preserve_claude_threads }) => {
                assert!(!preserve_claude_threads);
            }
            _ => panic!("Expected Ready command"),
        }
    }

    #[test]
    fn parse_ready_command_with_preserve_claude_threads() {
        let cli = Cli::parse_from(["pr-loop", "ready", "--preserve-claude-threads"]);
        match cli.command {
            Some(Command::Ready { preserve_claude_threads }) => {
                assert!(preserve_claude_threads);
            }
            _ => panic!("Expected Ready command"),
        }
    }
}
