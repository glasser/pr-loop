// Reply to PR review threads via GitHub GraphQL API.
// Posts comments with the Claude marker prefix.

use crate::threads::{CLAUDE_IN_PROGRESS_MARKER, CLAUDE_MARKER};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// Result of posting a reply.
#[derive(Debug)]
pub struct ReplyResult {
    pub comment_id: String,
}

/// Trait for posting replies, allowing test implementations.
pub trait ReplyClient {
    fn post_reply(&self, thread_id: &str, body: &str) -> Result<ReplyResult>;
    fn delete_comment(&self, comment_id: &str) -> Result<()>;
    fn update_comment(&self, comment_id: &str, body: &str) -> Result<()>;
    fn resolve_thread(&self, thread_id: &str) -> Result<()>;
}

/// Real client that uses `gh api graphql`.
pub struct RealReplyClient;

impl ReplyClient for RealReplyClient {
    fn post_reply(&self, thread_id: &str, body: &str) -> Result<ReplyResult> {
        post_reply_graphql(thread_id, body)
    }

    fn delete_comment(&self, comment_id: &str) -> Result<()> {
        delete_comment_graphql(comment_id)
    }

    fn update_comment(&self, comment_id: &str, body: &str) -> Result<()> {
        update_comment_graphql(comment_id, body)
    }

    fn resolve_thread(&self, thread_id: &str) -> Result<()> {
        resolve_thread_graphql(thread_id)
    }
}

// GraphQL response structures
#[derive(Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Deserialize)]
struct ReplyData {
    #[serde(rename = "addPullRequestReviewThreadReply")]
    add_reply: Option<AddReplyPayload>,
}

#[derive(Deserialize)]
struct AddReplyPayload {
    comment: Option<CommentNode>,
}

#[derive(Deserialize)]
struct CommentNode {
    id: String,
}

/// GraphQL mutation for adding a reply (loaded from graphql/operation/).
const ADD_REPLY_MUTATION: &str = include_str!("../graphql/operation/add_reply.graphql");

/// Post a reply to a thread using GraphQL.
fn post_reply_graphql(thread_id: &str, body: &str) -> Result<ReplyResult> {
    let mutation = ADD_REPLY_MUTATION;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", mutation),
            "-f",
            &format!("threadId={}", thread_id),
            "-f",
            &format!("body={}", body),
        ])
        .output()
        .context("Failed to run 'gh api graphql' for reply")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    let response: GraphQLResponse<ReplyData> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    let comment_id = response
        .data
        .and_then(|d| d.add_reply)
        .and_then(|r| r.comment)
        .map(|c| c.id)
        .ok_or_else(|| anyhow::anyhow!("No comment ID returned from mutation"))?;

    Ok(ReplyResult { comment_id })
}

/// GraphQL mutation for deleting a comment (loaded from graphql/operation/).
const DELETE_COMMENT_MUTATION: &str = include_str!("../graphql/operation/delete_comment.graphql");

/// Delete a PR review comment using GraphQL.
fn delete_comment_graphql(comment_id: &str) -> Result<()> {
    let mutation = DELETE_COMMENT_MUTATION;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", mutation),
            "-f",
            &format!("commentId={}", comment_id),
        ])
        .output()
        .context("Failed to run 'gh api graphql' for delete comment")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    // We don't need to parse the response - success is enough
    Ok(())
}

/// GraphQL mutation for updating a comment (loaded from graphql/operation/).
const UPDATE_COMMENT_MUTATION: &str = include_str!("../graphql/operation/update_comment.graphql");

/// Update a PR review comment's body using GraphQL.
fn update_comment_graphql(comment_id: &str, body: &str) -> Result<()> {
    let mutation = UPDATE_COMMENT_MUTATION;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", mutation),
            "-f",
            &format!("commentId={}", comment_id),
            "-f",
            &format!("body={}", body),
        ])
        .output()
        .context("Failed to run 'gh api graphql' for update comment")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    Ok(())
}

/// GraphQL mutation for resolving a review thread (loaded from graphql/operation/).
const RESOLVE_THREAD_MUTATION: &str = include_str!("../graphql/operation/resolve_thread.graphql");

/// Resolve a review thread using GraphQL.
fn resolve_thread_graphql(thread_id: &str) -> Result<()> {
    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", RESOLVE_THREAD_MUTATION),
            "-f",
            &format!("threadId={}", thread_id),
        ])
        .output()
        .context("Failed to run 'gh api graphql' for resolve thread")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    Ok(())
}

/// Format the message with the Claude marker prefix. When `in_progress` is
/// true, uses the interim-acknowledgment marker instead of the final one.
pub fn format_claude_message(message: &str, in_progress: bool) -> String {
    let marker = if in_progress {
        CLAUDE_IN_PROGRESS_MARKER
    } else {
        CLAUDE_MARKER
    };
    format!("{} {}", marker, message)
}

/// Whether a reply must be posted as an in-progress ack rather than a final
/// reply, given whether the caller asked for `--in-progress` and whether
/// newer human comments arrived after the one being replied to.
///
/// A reply that doesn't address newer comments must never use the final
/// marker: `ReviewThread::needs_response` only looks at the *last* comment,
/// and a final reply is always chronologically last, so it would make the
/// thread look fully resolved even though those newer comments were never
/// addressed. Relying on the agent noticing the newer-comments notice printed
/// to stdout isn't reliable enough to guard against silently dropping them.
pub fn should_post_as_in_progress(requested_in_progress: bool, has_newer_comments: bool) -> bool {
    requested_in_progress || has_newer_comments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test client that tracks calls.
    pub struct TestReplyClient {
        pub should_fail: bool,
    }

    impl ReplyClient for TestReplyClient {
        fn post_reply(&self, _thread_id: &str, _body: &str) -> Result<ReplyResult> {
            if self.should_fail {
                anyhow::bail!("Test failure")
            } else {
                Ok(ReplyResult {
                    comment_id: "test_comment_id".to_string(),
                })
            }
        }

        fn delete_comment(&self, _comment_id: &str) -> Result<()> {
            if self.should_fail {
                anyhow::bail!("Test failure")
            } else {
                Ok(())
            }
        }

        fn update_comment(&self, _comment_id: &str, _body: &str) -> Result<()> {
            if self.should_fail {
                anyhow::bail!("Test failure")
            } else {
                Ok(())
            }
        }

        fn resolve_thread(&self, _thread_id: &str) -> Result<()> {
            if self.should_fail {
                anyhow::bail!("Test failure")
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn format_message_adds_marker() {
        let formatted = format_claude_message("Hello world", false);
        assert_eq!(formatted, "🤖 From Claude: Hello world");
    }

    #[test]
    fn format_message_multiline() {
        let formatted = format_claude_message("Line 1\nLine 2", false);
        assert!(formatted.starts_with(CLAUDE_MARKER));
        assert!(formatted.contains("Line 1\nLine 2"));
    }

    #[test]
    fn format_message_in_progress_adds_in_progress_marker() {
        let formatted = format_claude_message("Looking into it", true);
        assert!(formatted.starts_with(CLAUDE_IN_PROGRESS_MARKER));
        assert!(!formatted.starts_with(CLAUDE_MARKER));
        assert!(formatted.contains("Looking into it"));
    }

    #[test]
    fn should_post_as_in_progress_when_explicitly_requested() {
        assert!(should_post_as_in_progress(true, false));
    }

    #[test]
    fn should_post_as_in_progress_when_newer_comments_arrived() {
        // Even if the caller intended a final reply, newer comments that
        // arrived after the one being replied to mean this reply can't
        // honestly be final.
        assert!(should_post_as_in_progress(false, true));
    }

    #[test]
    fn should_post_as_in_progress_when_neither() {
        assert!(!should_post_as_in_progress(false, false));
    }

    #[test]
    fn should_post_as_in_progress_when_both() {
        assert!(should_post_as_in_progress(true, true));
    }

    /// Reproduces the bug this function fixes: if a final reply were posted
    /// (in_progress=false) despite newer comments having arrived, the thread
    /// would look fully resolved to `needs_response()` even though the newer
    /// comments were never addressed — silently dropping them, not just
    /// delaying them. Posting with the marker this function chooses instead
    /// keeps the thread actionable.
    #[test]
    fn posting_without_in_progress_marker_would_hide_unaddressed_newer_comments() {
        use crate::threads::{ReviewThread, ThreadComment};

        let make_comment = |id: &str, body: &str| ThreadComment {
            id: id.to_string(),
            author: "reviewer".to_string(),
            body: body.to_string(),
            diff_hunk: None,
            url: None,
            created_at: None,
        };

        // C1: original human comment being replied to.
        // C2: a newer human comment that arrived while Claude was working.
        // Reply: posted after both, addressing only C1.
        let has_newer_comments = true;
        let final_marker_would_be_used = !should_post_as_in_progress(false, has_newer_comments);
        assert!(
            !final_marker_would_be_used,
            "must not use the final marker when newer comments weren't addressed"
        );

        let reply_body = format_claude_message(
            "Fixed it",
            should_post_as_in_progress(false, has_newer_comments),
        );
        let thread = ReviewThread {
            id: "THREAD1".to_string(),
            is_resolved: false,
            is_outdated: false,
            path: None,
            line: None,
            comments: vec![
                make_comment("C1", "please fix this"),
                make_comment("C2", "also consider this"),
                make_comment("R1", &reply_body),
            ],
        };

        assert!(
            thread.needs_response(),
            "thread must still be flagged actionable so C2 isn't dropped"
        );
    }

    #[test]
    fn test_client_success() {
        let client = TestReplyClient { should_fail: false };
        let result = client.post_reply("T1", "test").unwrap();
        assert_eq!(result.comment_id, "test_comment_id");
    }

    #[test]
    fn test_client_failure() {
        let client = TestReplyClient { should_fail: true };
        assert!(client.post_reply("T1", "test").is_err());
    }
}
