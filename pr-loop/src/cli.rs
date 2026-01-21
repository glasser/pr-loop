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
    #[arg(long)]
    pub wait_until_actionable: bool,

    /// Timeout in seconds for --wait-until-actionable (default: 1800 = 30 minutes)
    #[arg(long, default_value = "1800")]
    pub timeout: u64,

    /// Polling interval in seconds for --wait-until-actionable (default: 30)
    #[arg(long, default_value = "30")]
    pub poll_interval: u64,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Reply to a review thread comment
    Reply {
        /// The review thread ID to reply to
        #[arg(long)]
        thread: String,

        /// The message to post (will be prefixed with "🤖 From Claude:")
        #[arg(long)]
        message: String,

        /// Also resolve the thread after replying
        #[arg(long)]
        resolve: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        // Verifies that the CLI is well-formed
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_no_args() {
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
            "--thread",
            "PRRT_123",
            "--message",
            "Fixed the issue",
        ]);
        match cli.command {
            Some(Command::Reply {
                thread,
                message,
                resolve,
            }) => {
                assert_eq!(thread, "PRRT_123");
                assert_eq!(message, "Fixed the issue");
                assert!(!resolve);
            }
            _ => panic!("Expected Reply command"),
        }
    }

    #[test]
    fn parse_reply_with_resolve() {
        let cli = Cli::parse_from([
            "pr-loop",
            "reply",
            "--thread",
            "PRRT_123",
            "--message",
            "Done",
            "--resolve",
        ]);
        match cli.command {
            Some(Command::Reply { resolve, .. }) => {
                assert!(resolve);
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
            "--thread",
            "T1",
            "--message",
            "msg",
        ]);
        assert_eq!(cli.repo, Some("foo/bar".to_string()));
        assert_eq!(cli.pr, Some(42));
        assert!(matches!(cli.command, Some(Command::Reply { .. })));
    }

    #[test]
    fn parse_check_filters_from_env() {
        // Test that env vars work with comma-separated values
        // SAFETY: Test runs single-threaded; no other code accesses these env vars concurrently
        unsafe {
            std::env::set_var("PR_LOOP_INCLUDE_CHECKS", "ci/*,build");
            std::env::set_var("PR_LOOP_EXCLUDE_CHECKS", "lint,codecov/*");
        }

        let cli = Cli::parse_from(["pr-loop"]);
        assert_eq!(cli.include_checks, vec!["ci/*", "build"]);
        assert_eq!(cli.exclude_checks, vec!["lint", "codecov/*"]);

        // Clean up
        // SAFETY: Test runs single-threaded
        unsafe {
            std::env::remove_var("PR_LOOP_INCLUDE_CHECKS");
            std::env::remove_var("PR_LOOP_EXCLUDE_CHECKS");
        }
    }

    #[test]
    fn cli_args_override_env() {
        // Test that CLI args take precedence over env vars
        // SAFETY: Test runs single-threaded; no other code accesses these env vars concurrently
        unsafe {
            std::env::set_var("PR_LOOP_INCLUDE_CHECKS", "from-env");
        }

        let cli = Cli::parse_from(["pr-loop", "--include-checks", "from-cli"]);
        assert_eq!(cli.include_checks, vec!["from-cli"]);

        // Clean up
        // SAFETY: Test runs single-threaded
        unsafe {
            std::env::remove_var("PR_LOOP_INCLUDE_CHECKS");
        }
    }

    #[test]
    fn parse_wait_until_actionable() {
        let cli = Cli::parse_from(["pr-loop", "--wait-until-actionable"]);
        assert!(cli.wait_until_actionable);
        assert_eq!(cli.timeout, 1800); // default 30 minutes
        assert_eq!(cli.poll_interval, 30); // default 30 seconds
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
}
