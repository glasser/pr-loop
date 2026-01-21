// Reply to PR review threads via GitHub GraphQL API.
// Posts comments with the Claude marker prefix.

use crate::threads::CLAUDE_MARKER;
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
    fn resolve_thread(&self, thread_id: &str) -> Result<()>;
}

/// Real client that uses `gh api graphql`.
pub struct RealReplyClient;

impl ReplyClient for RealReplyClient {
    fn post_reply(&self, thread_id: &str, body: &str) -> Result<ReplyResult> {
        post_reply_graphql(thread_id, body)
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

#[derive(Deserialize)]
struct ResolveData {
    #[serde(rename = "resolveReviewThread")]
    resolve_thread: Option<ResolvePayload>,
}

#[derive(Deserialize)]
struct ResolvePayload {
    thread: Option<ThreadNode>,
}

#[derive(Deserialize)]
struct ThreadNode {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

/// Post a reply to a thread using GraphQL.
fn post_reply_graphql(thread_id: &str, body: &str) -> Result<ReplyResult> {
    let mutation = r#"
        mutation($threadId: ID!, $body: String!) {
            addPullRequestReviewThreadReply(input: {
                pullRequestReviewThreadId: $threadId,
                body: $body
            }) {
                comment {
                    id
                }
            }
        }
    "#;

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

/// Resolve a thread using GraphQL.
fn resolve_thread_graphql(thread_id: &str) -> Result<()> {
    let mutation = r#"
        mutation($threadId: ID!) {
            resolveReviewThread(input: {
                threadId: $threadId
            }) {
                thread {
                    isResolved
                }
            }
        }
    "#;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", mutation),
            "-f",
            &format!("threadId={}", thread_id),
        ])
        .output()
        .context("Failed to run 'gh api graphql' for resolve")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    let response: GraphQLResponse<ResolveData> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    let is_resolved = response
        .data
        .and_then(|d| d.resolve_thread)
        .and_then(|r| r.thread)
        .map(|t| t.is_resolved)
        .unwrap_or(false);

    if !is_resolved {
        anyhow::bail!("Thread was not resolved");
    }

    Ok(())
}

/// Format the message with the Claude marker prefix.
pub fn format_claude_message(message: &str) -> String {
    format!("{} {}", CLAUDE_MARKER, message)
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
        let formatted = format_claude_message("Hello world");
        assert_eq!(formatted, "🤖 From Claude: Hello world");
    }

    #[test]
    fn format_message_multiline() {
        let formatted = format_claude_message("Line 1\nLine 2");
        assert!(formatted.starts_with(CLAUDE_MARKER));
        assert!(formatted.contains("Line 1\nLine 2"));
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
