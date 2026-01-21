// pr-loop: CLI tool to help Claude Code manage PR workflows.
// Analyzes PR state (CI checks, review threads) and recommends next actions.

mod analysis;
mod checks;
mod cli;
mod credentials;
mod github;
mod reply;
mod threads;
mod wait;

use analysis::{analyze_pr, NextAction};
use checks::{get_checks_summary, ChecksSummary, RealChecksClient};
use clap::Parser;
use cli::{Cli, Command};
use credentials::{CredentialProvider, RealCredentialProvider};
use github::{resolve_pr_context, RealGitHubClient};
use reply::{format_claude_message, RealReplyClient, ReplyClient};
use threads::{RealThreadsClient, ThreadsClient, CLAUDE_MARKER};
use wait::{wait_until_actionable, WaitResult};

fn main() {
    let cli = Cli::parse();

    // Get credentials
    let provider = RealCredentialProvider;
    let creds = match provider.get_credentials() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Warn if CircleCI token is missing (needed for detailed CI logs, deferred)
    if creds.circleci_token.is_none() {
        eprintln!("Note: CIRCLECI_TOKEN not set. CircleCI log details will be unavailable.");
    }

    // Resolve PR context (from args or auto-detect)
    let gh_client = RealGitHubClient;
    let pr_context = match resolve_pr_context(&gh_client, cli.repo.as_deref(), cli.pr) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    match cli.command {
        Some(Command::Reply {
            thread,
            message,
            resolve,
        }) => {
            let reply_client = RealReplyClient;
            let formatted_message = format_claude_message(&message);

            println!(
                "Replying to thread {} on {}/{}#{}",
                thread, pr_context.owner, pr_context.repo, pr_context.pr_number
            );

            match reply_client.post_reply(&thread, &formatted_message) {
                Ok(result) => {
                    println!("✓ Reply posted (comment ID: {})", result.comment_id);

                    if resolve {
                        match reply_client.resolve_thread(&thread) {
                            Ok(()) => println!("✓ Thread resolved"),
                            Err(e) => eprintln!("Warning: Failed to resolve thread: {}", e),
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: Failed to post reply: {}", e);
                    std::process::exit(1);
                }
            }
        }
        None => {
            let checks_client = RealChecksClient;
            let threads_client = RealThreadsClient;

            // If --wait-until-actionable, poll until something needs attention
            if cli.wait_until_actionable {
                match wait_until_actionable(
                    &checks_client,
                    &threads_client,
                    &pr_context.owner,
                    &pr_context.repo,
                    pr_context.pr_number,
                    &cli.include_checks,
                    &cli.exclude_checks,
                    cli.timeout,
                    cli.poll_interval,
                ) {
                    Ok(WaitResult::Actionable) => {
                        eprintln!("PR is now actionable.");
                    }
                    Ok(WaitResult::Timeout) => {
                        eprintln!("Timeout reached without PR becoming actionable.");
                        std::process::exit(2);
                    }
                    Err(e) => {
                        eprintln!("Error while waiting: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            // Fetch checks
            let checks_summary = match get_checks_summary(
                &checks_client,
                &pr_context.owner,
                &pr_context.repo,
                pr_context.pr_number,
                &cli.include_checks,
                &cli.exclude_checks,
            ) {
                Ok(summary) => summary,
                Err(e) => {
                    eprintln!("Error: Failed to fetch checks: {}", e);
                    // Continue with empty checks
                    ChecksSummary { checks: vec![] }
                }
            };

            // Fetch review threads
            let threads = match threads_client.fetch_threads(
                &pr_context.owner,
                &pr_context.repo,
                pr_context.pr_number,
            ) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: Failed to fetch review threads: {}", e);
                    vec![]
                }
            };

            // Analyze and output recommendation
            let action = analyze_pr(&checks_summary, threads);
            print_recommendation(&pr_context, &checks_summary, &action);
        }
    }
}

fn print_recommendation(
    pr_context: &github::PrContext,
    checks: &ChecksSummary,
    action: &NextAction,
) {
    println!(
        "# PR Analysis: {}/{}#{}",
        pr_context.owner, pr_context.repo, pr_context.pr_number
    );
    println!();

    match action {
        NextAction::RespondToComments {
            threads,
            also_has_ci_failures,
            ci_pending,
        } => {
            println!("## ACTION REQUIRED: Respond to review comments");
            println!();
            println!(
                "There {} {} unaddressed review thread{}:",
                if threads.len() == 1 { "is" } else { "are" },
                threads.len(),
                if threads.len() == 1 { "" } else { "s" }
            );
            println!();

            for (i, actionable) in threads.iter().enumerate() {
                println!("### Thread {} - {}", i + 1, actionable.location());
                println!("Thread ID: `{}`", actionable.thread.id);
                if let Some(last) = actionable.thread.last_comment() {
                    println!("Last comment by @{}:", last.author);
                    println!();
                    // Indent the comment body
                    for line in last.body.lines() {
                        println!("> {}", line);
                    }
                    println!();
                }
            }

            println!("To reply, use:");
            println!(
                "  pr-loop reply --thread <THREAD_ID> --message \"Your response\""
            );
            println!();
            println!(
                "Your message will be prefixed with \"{}\"",
                CLAUDE_MARKER
            );

            if *also_has_ci_failures {
                println!();
                println!(
                    "⚠ Note: {} CI check(s) have also failed.",
                    checks.failed().len()
                );
            }
            if *ci_pending {
                println!();
                println!("○ Note: {} CI check(s) are still pending.", checks.pending().len());
            }
        }

        NextAction::FixCiFailures { failed_check_names } => {
            println!("## ACTION REQUIRED: Fix CI failures");
            println!();
            println!(
                "The following {} check{} failed:",
                failed_check_names.len(),
                if failed_check_names.len() == 1 { "" } else { "s" }
            );
            for name in failed_check_names {
                println!("  ✗ {}", name);
            }
            println!();
            println!("Use the CircleCI MCP server to investigate the failures:");
            println!("  - List recent pipelines for this project");
            println!("  - Get job details and logs for the failed workflow");
            println!();
            println!("Then push fixes to resolve the issues.");
        }

        NextAction::WaitForCi { pending_check_names } => {
            println!("## WAITING: CI checks in progress");
            println!();
            println!(
                "The following {} check{} still running:",
                pending_check_names.len(),
                if pending_check_names.len() == 1 { " is" } else { "s are" }
            );
            for name in pending_check_names {
                println!("  ○ {}", name);
            }
            println!();
            println!("No action needed. Wait for CI to complete.");
        }

        NextAction::PrReady => {
            println!("## PR READY");
            println!();
            println!("✓ All CI checks passed");
            println!("✓ No unaddressed review comments");
            println!();
            println!("The PR is ready for merge or further review.");
        }
    }
}
