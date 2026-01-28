// pr-loop: CLI tool to help Claude Code manage PR workflows.
// Analyzes PR state (CI checks, review threads) and recommends next actions.

mod analysis;
mod checks;
mod circleci;
mod cli;
mod credentials;
mod git;
mod github;
mod pr;
mod reply;
mod threads;
mod wait;

use analysis::{analyze_pr, NextAction};
use checks::{get_checks_summary, ChecksSummary, RealChecksClient};
use circleci::{
    get_failed_step_logs, is_circleci_url, parse_circleci_url, FailedStepLog, RealCircleCiClient,
};
use clap::Parser;
use cli::{Cli, Command};
use credentials::{CredentialProvider, Credentials, RealCredentialProvider};
use git::RealGitClient;
use github::{resolve_pr_context, PrContext, RealGitHubClient};
use pr::{has_status_block, remove_status_block, update_body_with_status, PrClient, RealPrClient};
use reply::{format_claude_message, RealReplyClient, ReplyClient};
use threads::{RealThreadsClient, ThreadsClient, CLAUDE_MARKER};
use wait::{capture_snapshot, wait_until_actionable, wait_until_actionable_or_happy, WaitResult};

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

    // Initialize PR client for status operations
    let pr_client = RealPrClient;

    // If --maintain-status is set, check draft mode first
    if cli.maintain_status {
        match pr_client.is_draft(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
            Ok(true) => {
                // Good, PR is in draft mode
            }
            Ok(false) => {
                eprintln!("Error: --maintain-status requires the PR to be in draft mode.");
                eprintln!("It's not polite to iterate with an AI on a non-draft PR!");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error: Failed to check PR draft status: {}", e);
                std::process::exit(1);
            }
        }

        // Update the status block
        if let Err(e) = update_pr_status(
            &pr_client,
            &pr_context,
            cli.status_message.as_deref(),
        ) {
            eprintln!("Warning: Failed to update PR status: {}", e);
        }
    }

    match cli.command {
        Some(Command::Reply { thread, message }) => {
            let reply_client = RealReplyClient;
            let formatted_message = format_claude_message(&message);

            println!(
                "Replying to thread {} on {}/{}#{}",
                thread, pr_context.owner, pr_context.repo, pr_context.pr_number
            );

            match reply_client.post_reply(&thread, &formatted_message) {
                Ok(result) => {
                    println!("✓ Reply posted (comment ID: {})", result.comment_id);
                }
                Err(e) => {
                    eprintln!("Error: Failed to post reply: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some(Command::Ready { preserve_claude_threads }) => {
            run_ready_command(
                &pr_client,
                &pr_context,
                &cli.include_checks,
                &cli.exclude_checks,
                preserve_claude_threads,
            );
        }

        None => {
            let checks_client = RealChecksClient;
            let threads_client = RealThreadsClient;
            let git_client = RealGitClient;

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
                    Ok(WaitResult::Happy) => {
                        // Should not happen with wait_until_actionable
                        eprintln!("PR is happy.");
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

            // If --wait-until-actionable-or-happy, poll until actionable or happy
            if cli.wait_until_actionable_or_happy {
                match wait_until_actionable_or_happy(
                    &checks_client,
                    &threads_client,
                    &git_client,
                    &pr_context.owner,
                    &pr_context.repo,
                    pr_context.pr_number,
                    &cli.include_checks,
                    &cli.exclude_checks,
                    cli.timeout,
                    cli.poll_interval,
                    cli.min_wait_after_push,
                ) {
                    Ok(WaitResult::Actionable) => {
                        eprintln!("PR is now actionable.");
                    }
                    Ok(WaitResult::Happy) => {
                        eprintln!("PR is happy (CI passing, no comments).");
                        std::process::exit(0);
                    }
                    Ok(WaitResult::Timeout) => {
                        eprintln!("Timeout reached.");
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

            // If there are CI failures and we have a CircleCI token, fetch logs
            let circleci_logs = if creds.circleci_token.is_some() {
                fetch_circleci_logs(&creds, &checks_summary)
            } else {
                vec![]
            };

            print_recommendation(&pr_context, &checks_summary, &action, &circleci_logs);
        }
    }
}

/// Fetch CircleCI logs for failed checks that have CircleCI URLs.
fn fetch_circleci_logs(creds: &Credentials, checks: &ChecksSummary) -> Vec<FailedStepLog> {
    let token = match &creds.circleci_token {
        Some(t) => t,
        None => return vec![],
    };

    let client = RealCircleCiClient::new(token.clone());
    let mut all_logs = Vec::new();

    for check in checks.failed() {
        if let Some(url) = &check.url {
            if is_circleci_url(url) {
                if let Some(job_info) = parse_circleci_url(url) {
                    match get_failed_step_logs(&client, &job_info) {
                        Ok(logs) => all_logs.extend(logs),
                        Err(e) => {
                            eprintln!(
                                "Warning: Failed to fetch CircleCI logs for {}: {}",
                                check.name, e
                            );
                        }
                    }
                }
            }
        }
    }

    all_logs
}

fn print_recommendation(
    pr_context: &github::PrContext,
    checks: &ChecksSummary,
    action: &NextAction,
    circleci_logs: &[FailedStepLog],
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

            // Show CircleCI logs if available
            if !circleci_logs.is_empty() {
                println!();
                println!("## CI Failure Details");
                for log in circleci_logs {
                    println!();
                    println!("### Job: {} / Step: {}", log.job_name, log.step_name);
                    if !log.error.is_empty() {
                        println!();
                        println!("**Stderr:**");
                        println!("```");
                        // Truncate long output
                        let error_truncated = truncate_log(&log.error, 2000);
                        println!("{}", error_truncated);
                        println!("```");
                    }
                    if !log.output.is_empty() {
                        println!();
                        println!("**Stdout (last lines):**");
                        println!("```");
                        // Show last part of stdout (often contains the actual error)
                        let output_truncated = truncate_log_tail(&log.output, 2000);
                        println!("{}", output_truncated);
                        println!("```");
                    }
                }
                println!();
                println!("Analyze the errors above and push fixes to resolve them.");
            } else {
                println!();
                println!("Use the CircleCI MCP server to investigate the failures:");
                println!("  - List recent pipelines for this project");
                println!("  - Get job details and logs for the failed workflow");
                println!();
                println!("Then push fixes to resolve the issues.");
            }
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

/// Truncate a log string to a maximum length, from the beginning.
fn truncate_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...\n[truncated, {} more bytes]", &s[..max_len], s.len() - max_len)
    }
}

/// Truncate a log string to show only the tail (last lines).
fn truncate_log_tail(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let start = s.len() - max_len;
        // Find the next newline to start on a line boundary
        let start = s[start..].find('\n').map(|i| start + i + 1).unwrap_or(start);
        format!("[... {} bytes truncated]\n{}", start, &s[start..])
    }
}

/// Update the PR description with a status block.
fn update_pr_status(
    pr_client: &dyn PrClient,
    pr_context: &PrContext,
    status_message: Option<&str>,
) -> anyhow::Result<()> {
    let current_body = pr_client.get_body(&pr_context.owner, &pr_context.repo, pr_context.pr_number)?;
    let new_body = update_body_with_status(&current_body, status_message);
    pr_client.set_body(&pr_context.owner, &pr_context.repo, pr_context.pr_number, &new_body)?;
    eprintln!("✓ Updated PR status block");
    Ok(())
}

/// Run the `ready` subcommand.
fn run_ready_command(
    pr_client: &dyn PrClient,
    pr_context: &PrContext,
    include_checks: &[String],
    exclude_checks: &[String],
    preserve_claude_threads: bool,
) {
    let checks_client = RealChecksClient;
    let threads_client = RealThreadsClient;
    let reply_client = RealReplyClient;

    // Step 1: Check that PR is in draft mode
    println!("Checking PR draft status...");
    match pr_client.is_draft(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(true) => {
            println!("✓ PR is in draft mode");
        }
        Ok(false) => {
            eprintln!("Error: PR is not in draft mode. The 'ready' command is for marking draft PRs as ready.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: Failed to check PR draft status: {}", e);
            std::process::exit(1);
        }
    }

    // Step 2: Check that PR has exactly one commit
    println!("Checking PR commit count...");
    match pr_client.get_commit_count(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(1) => {
            println!("✓ PR has a single commit");
        }
        Ok(count) => {
            eprintln!("Error: PR has {} commits. Please squash to a single commit before marking ready.", count);
            eprintln!();
            eprintln!("First, fetch the latest from origin:");
            eprintln!("  git fetch origin");
            eprintln!();
            eprintln!("To squash commits interactively:");
            eprintln!("  git rebase -i origin/main");
            eprintln!();
            eprintln!("Or to squash all commits on this branch:");
            eprintln!("  git reset --soft $(git merge-base HEAD origin/main) && git commit");
            eprintln!();
            eprintln!("When writing the squashed commit message:");
            eprintln!("  - Describe the full change as a single cohesive commit");
            eprintln!("  - Summarize what the PR accomplishes, not the individual commits");
            eprintln!("  - After squashing, update the PR description to match (keep any status blocks");
            eprintln!("    and follow any PR template in the repo)");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: Failed to check PR commit count: {}", e);
            std::process::exit(1);
        }
    }

    // Step 3: Validate PR is "happy" (no unresolved threads, CI passing)
    println!("Validating PR state...");
    let snapshot = match capture_snapshot(
        &checks_client,
        &threads_client,
        &pr_context.owner,
        &pr_context.repo,
        pr_context.pr_number,
        include_checks,
        exclude_checks,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: Failed to check PR state: {}", e);
            std::process::exit(1);
        }
    };

    // Check for unresolved threads (ALL threads must be resolved, not just non-actionable)
    if !snapshot.unresolved_thread_ids.is_empty() {
        eprintln!(
            "Error: PR has {} unresolved review thread(s). All threads must be resolved before marking ready.",
            snapshot.unresolved_thread_ids.len()
        );
        std::process::exit(1);
    }

    if !snapshot.failed_check_names.is_empty() {
        eprintln!(
            "Error: PR has {} failing CI check(s): {}",
            snapshot.failed_check_names.len(),
            snapshot.failed_check_names.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        std::process::exit(1);
    }

    if !snapshot.pending_check_names.is_empty() {
        eprintln!(
            "Error: PR has {} pending CI check(s): {}",
            snapshot.pending_check_names.len(),
            snapshot.pending_check_names.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        eprintln!("Wait for CI to complete before marking ready.");
        std::process::exit(1);
    }

    println!("✓ All threads resolved");
    println!("✓ All CI checks passed");

    // Step 4: Delete pure-Claude threads unless preservation is requested
    if !preserve_claude_threads {
        println!("Deleting pure-Claude threads...");
        match threads_client.fetch_threads(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
            Ok(threads) => {
                let pure_claude_threads: Vec<_> = threads
                    .iter()
                    .filter(|t| t.is_resolved && t.is_pure_claude())
                    .collect();

                if pure_claude_threads.is_empty() {
                    println!("  (no pure-Claude threads found)");
                } else {
                    let mut deleted_count = 0;
                    for thread in pure_claude_threads {
                        for comment_id in thread.comment_ids() {
                            match reply_client.delete_comment(comment_id) {
                                Ok(()) => deleted_count += 1,
                                Err(e) => {
                                    eprintln!("Warning: Failed to delete comment {}: {}", comment_id, e);
                                }
                            }
                        }
                    }
                    println!("✓ Deleted {} comment(s) from pure-Claude threads", deleted_count);
                }
            }
            Err(e) => {
                eprintln!("Warning: Failed to fetch threads for deletion: {}", e);
            }
        }
    }

    // Step 5: Remove status block from PR description
    println!("Removing status block from PR description...");
    match pr_client.get_body(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(body) => {
            if has_status_block(&body) {
                let new_body = remove_status_block(&body);
                if let Err(e) = pr_client.set_body(
                    &pr_context.owner,
                    &pr_context.repo,
                    pr_context.pr_number,
                    &new_body,
                ) {
                    eprintln!("Warning: Failed to remove status block: {}", e);
                } else {
                    println!("✓ Status block removed");
                }
            } else {
                println!("  (no status block present)");
            }
        }
        Err(e) => {
            eprintln!("Warning: Failed to get PR body: {}", e);
        }
    }

    // Step 6: Mark PR as ready (non-draft)
    println!("Marking PR as ready for review...");
    match pr_client.mark_ready(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(()) => {
            println!("✓ PR marked as ready for review");
            println!();
            println!("🎉 PR is now ready for human review!");
        }
        Err(e) => {
            eprintln!("Error: Failed to mark PR as ready: {}", e);
            std::process::exit(1);
        }
    }
}
