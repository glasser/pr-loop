// pr-loop: CLI tool to help Claude Code manage PR workflows.
// Analyzes PR state (CI checks, review threads) and recommends next actions.

mod analysis;
mod cc_status;
mod checks;
mod circleci;
mod cli;
mod commits;
mod credentials;
mod gh_actions;
mod git;
mod github;
mod hub;
#[cfg(test)]
mod graphql_validation;
mod pr;
mod reply;
mod threads;
mod wait;
mod web;

use analysis::{analyze_pr, NextAction};
use checks::{get_checks_summary, CheckStatus, ChecksSummary, RealChecksClient};
use circleci::{
    get_job_failures, is_circleci_url, parse_circleci_url, CircleCiFailureInfo, FailedStepLog,
    RealCircleCiClient,
};
use clap::Parser;
use cli::{Cli, Command};
use credentials::{CredentialProvider, Credentials, RealCredentialProvider};
use git::RealGitClient;
use github::{
    resolve_pr_context, MergeableClient, MergeableStatus, PrContext, RealGitHubClient,
    RealMergeableClient,
};
use pr::{has_status_block, remove_status_block, update_body_with_status, PrClient, RealPrClient};
use reply::{format_claude_message, RealReplyClient, ReplyClient};
use threads::{
    RealThreadsClient, ReviewThread, ThreadsClient, CLAUDE_MARKER, PAPERCLIP_EMOJI,
    PAPERCLIP_SHORTCODE,
};
use wait::{capture_snapshot, wait_until_actionable, wait_until_actionable_or_happy, WaitResult};

fn main() {
    let cli = Cli::parse();

    // Hub subcommand doesn't need PR context, credentials, or anything else;
    // handle it (and --install/--uninstall) before the rest of setup.
    if let Some(Command::Hub { port, install, uninstall }) = &cli.command {
        let result = if *install {
            hub::install()
        } else if *uninstall {
            hub::uninstall()
        } else {
            hub::run(*port)
        };
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

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
        Some(Command::Reply { in_reply_to, message }) => {
            let reply_client = RealReplyClient;
            let threads_client = RealThreadsClient;

            // Fetch the thread containing this comment
            let thread_data = match threads_client.fetch_thread_by_comment_id(&in_reply_to) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: Could not fetch thread for comment {}: {}", in_reply_to, e);
                    std::process::exit(1);
                }
            };

            let thread_id = thread_data.id.clone();

            // Check for newer human comments after the one we're replying to
            let newer_comments = match thread_data.human_comments_after(&in_reply_to) {
                Some(comments) => comments,
                None => {
                    eprintln!(
                        "Error: Comment {} not found in thread {}",
                        in_reply_to, thread_id
                    );
                    std::process::exit(1);
                }
            };

            // Modify message if there are newer human comments
            let final_message = if !newer_comments.is_empty() {
                format!(
                    "{}\n\n(Looks like you had something else to say here while I was working. I'll look at that now.)",
                    message
                )
            } else {
                message.clone()
            };

            let formatted_message = format_claude_message(&final_message);

            println!(
                "Replying to thread {} on {}/{}#{}",
                thread_id, pr_context.owner, pr_context.repo, pr_context.pr_number
            );

            match reply_client.post_reply(&thread_id, &formatted_message) {
                Ok(result) => {
                    println!("✓ Reply posted (comment ID: {})", result.comment_id);

                    // If there were newer comments, print them for the invoker
                    if !newer_comments.is_empty() {
                        print_newer_comments(&newer_comments, &thread_id);
                    }

                    // Poke a running `pr-loop web` instance so its UI
                    // refreshes immediately. Best-effort, ignore failures.
                    web::poke_running_server(&pr_context);
                }
                Err(e) => {
                    eprintln!("Error: Failed to post reply: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some(Command::Ready { preserve_claude_threads, reviewer }) => {
            run_ready_command(
                &pr_client,
                &pr_context,
                &cli.include_checks,
                &cli.exclude_checks,
                preserve_claude_threads,
                &reviewer,
            );
        }

        Some(Command::CleanThreads) => {
            run_clean_threads_command(&pr_context);
        }

        Some(Command::Checks) => {
            run_checks_command(
                &creds,
                &pr_context,
                &cli.include_checks,
                &cli.exclude_checks,
            );
        }

        Some(Command::Web { port, no_open }) => {
            if let Err(e) = web::run(&pr_context, port, no_open) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }

        Some(Command::Hub { .. }) => {
            // Handled above before setup; unreachable.
            unreachable!();
        }

        None => {
            let checks_client = RealChecksClient;
            let threads_client = RealThreadsClient;
            let git_client = RealGitClient;
            let mergeable_client = RealMergeableClient;

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

            // If there are CI failures, fetch logs. fetch_ci_failure_info
            // handles the no-CircleCI-token case internally; GitHub Actions
            // logs don't need extra credentials.
            let circleci_info = fetch_ci_failure_info(&creds, &checks_summary);

            let mergeable_status = match mergeable_client.fetch_mergeable_status(
                &pr_context.owner,
                &pr_context.repo,
                pr_context.pr_number,
            ) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Warning: Failed to fetch merge conflict status: {}", e);
                    MergeableStatus::Unknown
                }
            };

            print_recommendation(
                &pr_context,
                &checks_summary,
                &action,
                &circleci_info,
                &mergeable_status,
            );
        }
    }
}

/// Fetch CI failure info (logs + test failures) for failed checks. Handles
/// both CircleCI (via their API; requires CIRCLECI_TOKEN) and GitHub Actions
/// (via `gh api`, no extra credentials needed).
fn fetch_ci_failure_info(creds: &Credentials, checks: &ChecksSummary) -> CircleCiFailureInfo {
    let circleci_client = creds
        .circleci_token
        .as_ref()
        .map(|t| RealCircleCiClient::new(t.clone()));
    let gh_actions_client = gh_actions::RealGhActionsClient;
    let mut combined = CircleCiFailureInfo::default();

    for check in checks.failed() {
        let Some(url) = &check.url else { continue };
        if is_circleci_url(url) {
            let Some(c) = &circleci_client else { continue };
            if let Some(job_info) = parse_circleci_url(url) {
                match get_job_failures(c, &job_info) {
                    Ok(info) => {
                        combined.step_logs.extend(info.step_logs);
                        combined.test_failures.extend(info.test_failures);
                    }
                    Err(e) => eprintln!(
                        "Warning: Failed to fetch CircleCI logs for {}: {}",
                        check.name, e
                    ),
                }
            }
        } else if gh_actions::is_gh_actions_url(url) {
            if let Some(job_info) = gh_actions::parse_gh_actions_url(url) {
                match gh_actions::get_failed_step_logs(&gh_actions_client, &job_info) {
                    Ok(logs) => combined.step_logs.extend(logs),
                    Err(e) => eprintln!(
                        "Warning: Failed to fetch GitHub Actions logs for {}: {}",
                        check.name, e
                    ),
                }
            }
        }
    }

    combined
}

fn print_recommendation(
    pr_context: &github::PrContext,
    checks: &ChecksSummary,
    action: &NextAction,
    circleci_info: &CircleCiFailureInfo,
    mergeable_status: &MergeableStatus,
) {
    println!(
        "# PR Analysis: {}/{}#{}",
        pr_context.owner, pr_context.repo, pr_context.pr_number
    );
    println!();

    if *mergeable_status == MergeableStatus::Conflicting {
        println!("⚠ **MERGE CONFLICTS**: This PR has merge conflicts that must be resolved.");
        println!();
    }

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
                println!();

                for comment in &actionable.thread.comments {
                    println!("**@{}** (comment `{}`):", comment.author, comment.id);
                    for line in comment.body.lines() {
                        println!("> {}", line);
                    }
                    println!();
                }

                if i < threads.len() - 1 {
                    println!("---");
                    println!();
                }
            }

            println!("To reply, use:");
            println!(
                "  pr-loop reply --in-reply-to <COMMENT_ID> --message \"Your response\""
            );
            println!();
            println!("The --in-reply-to should be the ID of the last comment shown above.");
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

            if *mergeable_status == MergeableStatus::Conflicting {
                println!();
                println!("⚠ This PR has merge conflicts. Consider rebasing to resolve conflicts");
                println!("  before investigating CI failures — some failures may be caused by the");
                println!("  conflicts, and CI will re-run after rebasing anyway.");
            }

            // Show CircleCI test failures if available (structured, most useful)
            if !circleci_info.test_failures.is_empty() {
                println!();
                println!("## CI Test Failures");
                print_test_failures(&circleci_info.test_failures);
            }

            // Show CircleCI step logs if available
            if !circleci_info.step_logs.is_empty() {
                println!();
                println!("## CI Failure Logs");
                print_step_logs(&circleci_info.step_logs);
                println!();
                println!("Analyze the errors above and push fixes to resolve them.");
            } else if circleci_info.test_failures.is_empty() {
                println!();
                println!("Use the CircleCI MCP server to investigate the failures:");
                println!("  - List recent pipelines for this project");
                println!("  - Get job details and logs for the failed workflow");
                println!();
                println!("Then push fixes to resolve the issues.");
            } else {
                println!();
                println!("Analyze the test failures above and push fixes to resolve them.");
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

/// Print newer comments that were posted while the LLM was working.
fn print_newer_comments(comments: &[threads::ThreadComment], thread_id: &str) {
    println!();
    println!("## NEWER COMMENTS DETECTED");
    println!();
    println!(
        "The following {} comment{} {} posted to this thread while you were working.",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" },
        if comments.len() == 1 { "was" } else { "were" }
    );
    println!("Please address {} as well:", if comments.len() == 1 { "it" } else { "them" });
    println!();

    for (i, comment) in comments.iter().enumerate() {
        println!("### Comment {} (in thread {})", i + 1, thread_id);
        println!("**@{}:**", comment.author);
        for line in comment.body.lines() {
            println!("> {}", line);
        }
        println!();
    }
}

/// Print structured test failures, grouped by job name.
fn print_test_failures(failures: &[circleci::TestFailure]) {
    println!();
    println!(
        "{} test failure{}:",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    );

    // Group by job name, preserving order of first appearance
    let mut job_order: Vec<&str> = Vec::new();
    let mut by_job: std::collections::HashMap<&str, Vec<&circleci::TestFailure>> =
        std::collections::HashMap::new();
    for failure in failures {
        let entry = by_job.entry(failure.job_name.as_str()).or_default();
        if entry.is_empty() {
            job_order.push(&failure.job_name);
        }
        entry.push(failure);
    }

    for job_name in &job_order {
        println!();
        println!("### Job: {}", job_name);
        for failure in &by_job[job_name] {
            println!();
            println!("- **{}** / {}", failure.classname, failure.test_name);
            if !failure.message.is_empty() {
                println!("  ```");
                // Truncate long messages (stack traces can be very long)
                let msg = truncate_log(&failure.message, 500);
                for line in msg.lines() {
                    println!("  {}", line);
                }
                println!("  ```");
            }
        }
    }
}

/// Print step log details.
fn print_step_logs(logs: &[FailedStepLog]) {
    for log in logs {
        println!();
        println!("### Job: {} / Step: {}", log.job_name, log.step_name);
        if !log.error.is_empty() {
            println!();
            println!("**Stderr:**");
            println!("```");
            let error_truncated = truncate_log(&log.error, 2000);
            println!("{}", error_truncated);
            println!("```");
        }
        if !log.output.is_empty() {
            println!();
            println!("**Stdout (last lines):**");
            println!("```");
            // Gradle/Java failures often end with ~15 lines of "Try: Run
            // with --stacktrace / BUILD FAILED / Publishing Build Scan"
            // boilerplate, so keep a decently sized tail so the actual
            // error is visible above it.
            let output_truncated = truncate_log_tail(&log.output, 4000);
            println!("{}", output_truncated);
            println!("```");
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

/// Delete a batch of comments in parallel with bounded concurrency.
/// Returns (success_count, failure_count).
fn delete_comments_parallel(comment_ids: &[&str], max_concurrent: usize) -> (usize, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let success_count = Arc::new(AtomicUsize::new(0));
    let failure_count = Arc::new(AtomicUsize::new(0));

    // Process in chunks of max_concurrent
    for chunk in comment_ids.chunks(max_concurrent) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|&id| {
                let id = id.to_string();
                let success = Arc::clone(&success_count);
                let failure = Arc::clone(&failure_count);
                std::thread::spawn(move || {
                    let client = RealReplyClient;
                    match client.delete_comment(&id) {
                        Ok(()) => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("Warning: Failed to delete comment {}: {}", id, e);
                            failure.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("thread panicked during comment deletion");
        }
    }

    (
        success_count.load(Ordering::Relaxed),
        failure_count.load(Ordering::Relaxed),
    )
}

/// Strip the paperclip marker from comments in paperclip threads.
/// These threads are preserved for human review; the marker is removed so the
/// human reviewer sees the comments without the marker noise.
fn strip_paperclips(threads: &[ReviewThread]) {
    let paperclip_threads: Vec<_> = threads.iter().filter(|t| t.has_paperclip()).collect();

    if paperclip_threads.is_empty() {
        return;
    }

    let client = RealReplyClient;
    let mut updated = 0;
    let mut failed = 0;

    for thread in &paperclip_threads {
        for comment in &thread.comments {
            if comment.body.contains(PAPERCLIP_SHORTCODE)
                || comment.body.contains(PAPERCLIP_EMOJI)
            {
                let new_body = comment
                    .body
                    .replace(PAPERCLIP_SHORTCODE, "")
                    .replace(PAPERCLIP_EMOJI, "");
                match client.update_comment(&comment.id, &new_body) {
                    Ok(()) => updated += 1,
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to strip paperclip from comment {}: {}",
                            comment.id, e
                        );
                        failed += 1;
                    }
                }
            }
        }
    }

    if updated > 0 {
        println!(
            "✓ Stripped paperclip marker from {} comment(s) in {} thread(s)",
            updated,
            paperclip_threads.len()
        );
    }
    if failed > 0 {
        eprintln!("  ({} update(s) failed)", failed);
    }
}

/// Run the `clean-threads` subcommand: delete resolved pure-Claude threads.
fn run_clean_threads_command(pr_context: &PrContext) {
    let threads_client = RealThreadsClient;

    println!("Deleting resolved pure-Claude threads...");
    match threads_client.fetch_threads(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(threads) => {
            // Delete pure-Claude threads first, before stripping paperclips.
            // This ordering matters: if we stripped paperclips first and then
            // deletion failed midway, a retry would no longer detect the
            // paperclip threads and might incorrectly delete them.
            let pure_claude_threads: Vec<_> = threads
                .iter()
                .filter(|t| !t.has_paperclip() && t.is_resolved && t.is_pure_claude())
                .collect();

            if pure_claude_threads.is_empty() {
                println!("  (no resolved pure-Claude threads found)");
            } else {
                let comment_ids: Vec<&str> = pure_claude_threads
                    .iter()
                    .flat_map(|t| t.comment_ids())
                    .collect();

                let (deleted, failed) = delete_comments_parallel(&comment_ids, 10);
                println!(
                    "✓ Deleted {} comment(s) from {} pure-Claude thread(s)",
                    deleted,
                    pure_claude_threads.len()
                );
                if failed > 0 {
                    eprintln!("  ({} deletion(s) failed)", failed);
                }
            }

            // Strip paperclip markers (these threads are preserved for human review)
            strip_paperclips(&threads);
        }
        Err(e) => {
            eprintln!("Error: Failed to fetch threads: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run the `checks` subcommand: show CI check status and failure logs.
fn run_checks_command(
    creds: &Credentials,
    pr_context: &PrContext,
    include_checks: &[String],
    exclude_checks: &[String],
) {
    let checks_client = RealChecksClient;
    let mergeable_client = RealMergeableClient;

    let checks_summary = match get_checks_summary(
        &checks_client,
        &pr_context.owner,
        &pr_context.repo,
        pr_context.pr_number,
        include_checks,
        exclude_checks,
    ) {
        Ok(summary) => summary,
        Err(e) => {
            eprintln!("Error: Failed to fetch checks: {}", e);
            std::process::exit(1);
        }
    };

    let mergeable_status = match mergeable_client.fetch_mergeable_status(
        &pr_context.owner,
        &pr_context.repo,
        pr_context.pr_number,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: Failed to fetch merge conflict status: {}", e);
            MergeableStatus::Unknown
        }
    };

    println!(
        "# CI Checks: {}/{}#{}",
        pr_context.owner, pr_context.repo, pr_context.pr_number
    );
    println!();

    if mergeable_status == MergeableStatus::Conflicting {
        println!("⚠ **MERGE CONFLICTS**: This PR has merge conflicts that must be resolved.");
        println!();
    }

    if checks_summary.checks.is_empty() {
        println!("No checks found.");
        return;
    }

    // Group checks by status for display
    let passed: Vec<_> = checks_summary
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Pass)
        .collect();
    let failed = checks_summary.failed();
    let pending = checks_summary.pending();
    let skipped: Vec<_> = checks_summary
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Skipping)
        .collect();
    let cancelled: Vec<_> = checks_summary
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Cancelled)
        .collect();

    if !failed.is_empty() {
        println!(
            "## Failed ({})",
            failed.len()
        );
        for check in &failed {
            println!("  ✗ {}", check.name);
        }
        println!();
        if mergeable_status == MergeableStatus::Conflicting {
            println!("⚠ This PR has merge conflicts. Consider rebasing to resolve conflicts");
            println!("  before investigating CI failures — some failures may be caused by the");
            println!("  conflicts, and CI will re-run after rebasing anyway.");
            println!();
        }
    }

    if !pending.is_empty() {
        println!(
            "## Pending ({})",
            pending.len()
        );
        for check in &pending {
            println!("  ○ {}", check.name);
        }
        println!();
    }

    if !passed.is_empty() {
        println!(
            "## Passed ({})",
            passed.len()
        );
        for check in &passed {
            println!("  ✓ {}", check.name);
        }
        println!();
    }

    if !skipped.is_empty() {
        println!(
            "## Skipped ({})",
            skipped.len()
        );
        for check in &skipped {
            println!("  ⊘ {}", check.name);
        }
        println!();
    }

    if !cancelled.is_empty() {
        println!(
            "## Cancelled ({})",
            cancelled.len()
        );
        for check in &cancelled {
            println!("  ⊘ {}", check.name);
        }
        println!();
    }

    // Fetch and display CI failure info (CircleCI + GH Actions)
    if !failed.is_empty() {
        let circleci_info = fetch_ci_failure_info(creds, &checks_summary);
        if !circleci_info.test_failures.is_empty() {
            println!("## CI Test Failures");
            print_test_failures(&circleci_info.test_failures);
        }
        if !circleci_info.step_logs.is_empty() {
            println!("## CI Failure Logs");
            print_step_logs(&circleci_info.step_logs);
        }
    }
}

/// Run the `ready` subcommand.
fn run_ready_command(
    pr_client: &dyn PrClient,
    pr_context: &PrContext,
    include_checks: &[String],
    exclude_checks: &[String],
    preserve_claude_threads: bool,
    reviewers: &[String],
) {
    let checks_client = RealChecksClient;
    let threads_client = RealThreadsClient;

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
            eprintln!();
            eprintln!("After squashing and force-pushing, wait for CI to pass by running:");
            eprintln!("  pr-loop --wait-until-actionable-or-happy --maintain-status");
            eprintln!();
            eprintln!("NOTE: You MUST use --wait-until-actionable-or-happy (not --wait-until-actionable)");
            eprintln!("so that the command exits successfully when CI passes. Then run `pr-loop ready` again.");
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

    // Step 4: Clean up threads (delete pure-Claude threads, then strip paperclips)
    // Deletion before stripping: if we stripped first and deletion failed midway,
    // a retry would no longer detect paperclip threads and might delete them.
    match threads_client.fetch_threads(&pr_context.owner, &pr_context.repo, pr_context.pr_number) {
        Ok(threads) => {
            if !preserve_claude_threads {
                println!("Deleting pure-Claude threads...");
                let pure_claude_threads: Vec<_> = threads
                    .iter()
                    .filter(|t| !t.has_paperclip() && t.is_resolved && t.is_pure_claude())
                    .collect();

                if pure_claude_threads.is_empty() {
                    println!("  (no pure-Claude threads found)");
                } else {
                    let comment_ids: Vec<&str> = pure_claude_threads
                        .iter()
                        .flat_map(|t| t.comment_ids())
                        .collect();

                    let (deleted, _) = delete_comments_parallel(&comment_ids, 10);
                    println!("✓ Deleted {} comment(s) from pure-Claude threads", deleted);
                }
            }

            // Strip paperclip markers (these threads are preserved for human review)
            strip_paperclips(&threads);
        }
        Err(e) => {
            eprintln!("Warning: Failed to fetch threads for cleanup: {}", e);
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
        }
        Err(e) => {
            eprintln!("Error: Failed to mark PR as ready: {}", e);
            std::process::exit(1);
        }
    }

    // Step 7 (optional): Request review from specified reviewers
    for username in reviewers {
        println!("Requesting review from @{}...", username);
        match pr_client.add_reviewer(&pr_context.owner, &pr_context.repo, pr_context.pr_number, username) {
            Ok(()) => {
                println!("✓ Review requested from @{}", username);
            }
            Err(e) => {
                eprintln!("Error: Failed to request review from @{}: {}", username, e);
                std::process::exit(1);
            }
        }
    }

    println!();
    println!("🎉 PR is now ready for human review!");
}
