// PR review thread handling via GitHub GraphQL API.
// Fetches review threads including resolution status and comments.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// A comment in a review thread.
#[derive(Debug, Clone)]
pub struct ThreadComment {
    pub author: String,
    pub body: String,
}

/// A review thread on a PR.
#[derive(Debug, Clone)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub comments: Vec<ThreadComment>,
}

/// The marker prefix that Claude uses when replying to threads.
pub const CLAUDE_MARKER: &str = "🤖 From Claude:";

impl ReviewThread {
    /// Returns the last comment in the thread.
    pub fn last_comment(&self) -> Option<&ThreadComment> {
        self.comments.last()
    }

    /// Returns true if this thread needs a response from Claude.
    /// A thread needs response if: it's unresolved AND the last comment
    /// doesn't start with the Claude marker.
    pub fn needs_response(&self) -> bool {
        if self.is_resolved {
            return false;
        }

        match self.last_comment() {
            Some(comment) => !comment.body.starts_with(CLAUDE_MARKER),
            None => false, // Empty thread, nothing to respond to
        }
    }
}

/// A thread that needs a response, with additional context for display.
#[derive(Debug, Clone)]
pub struct ActionableThread {
    pub thread: ReviewThread,
}

impl ActionableThread {
    /// Format the thread location for display.
    pub fn location(&self) -> String {
        match (&self.thread.path, self.thread.line) {
            (Some(path), Some(line)) => format!("{}:{}", path, line),
            (Some(path), None) => path.clone(),
            _ => "unknown location".to_string(),
        }
    }
}

/// Find all threads that need a response from Claude.
pub fn find_actionable_threads(threads: Vec<ReviewThread>) -> Vec<ActionableThread> {
    threads
        .into_iter()
        .filter(|t| t.needs_response())
        .map(|thread| ActionableThread { thread })
        .collect()
}

/// Trait for fetching review threads, allowing test implementations.
pub trait ThreadsClient {
    fn fetch_threads(&self, owner: &str, repo: &str, pr_number: u64)
        -> Result<Vec<ReviewThread>>;
}

/// Real client that uses `gh api graphql`.
pub struct RealThreadsClient;

impl ThreadsClient for RealThreadsClient {
    fn fetch_threads(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<ReviewThread>> {
        fetch_threads_from_graphql(owner, repo, pr_number)
    }
}

// GraphQL response structures
#[derive(Deserialize)]
struct GraphQLResponse {
    data: Option<GraphQLData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Deserialize)]
struct GraphQLData {
    repository: Option<RepositoryData>,
}

#[derive(Deserialize)]
struct RepositoryData {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestData>,
}

#[derive(Deserialize)]
struct PullRequestData {
    #[serde(rename = "reviewThreads")]
    review_threads: ReviewThreadsConnection,
}

#[derive(Deserialize)]
struct ReviewThreadsConnection {
    nodes: Vec<ReviewThreadNode>,
}

#[derive(Deserialize)]
struct ReviewThreadNode {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    path: Option<String>,
    line: Option<u64>,
    comments: CommentsConnection,
}

#[derive(Deserialize)]
struct CommentsConnection {
    nodes: Vec<CommentNode>,
}

#[derive(Deserialize)]
struct CommentNode {
    author: Option<AuthorNode>,
    body: String,
}

#[derive(Deserialize)]
struct AuthorNode {
    login: String,
}

/// Fetch threads using GitHub GraphQL API.
fn fetch_threads_from_graphql(
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<ReviewThread>> {
    let query = r#"
        query($owner: String!, $repo: String!, $pr: Int!) {
            repository(owner: $owner, name: $repo) {
                pullRequest(number: $pr) {
                    reviewThreads(first: 100) {
                        nodes {
                            id
                            isResolved
                            path
                            line
                            comments(first: 100) {
                                nodes {
                                    author {
                                        login
                                    }
                                    body
                                }
                            }
                        }
                    }
                }
            }
        }
    "#;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", query),
            "-f",
            &format!("owner={}", owner),
            "-f",
            &format!("repo={}", repo),
            "-F",
            &format!("pr={}", pr_number),
        ])
        .output()
        .context("Failed to run 'gh api graphql'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL query failed: {}", stderr.trim());
    }

    let response: GraphQLResponse = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    let threads = response
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .map(|pr| pr.review_threads.nodes)
        .unwrap_or_default();

    Ok(threads
        .into_iter()
        .map(|t| ReviewThread {
            id: t.id,
            is_resolved: t.is_resolved,
            path: t.path,
            line: t.line,
            comments: t
                .comments
                .nodes
                .into_iter()
                .map(|c| ThreadComment {
                    author: c.author.map(|a| a.login).unwrap_or_else(|| "ghost".to_string()),
                    body: c.body,
                })
                .collect(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test client that returns predefined threads.
    pub struct TestThreadsClient {
        pub threads: Vec<ReviewThread>,
    }

    impl ThreadsClient for TestThreadsClient {
        fn fetch_threads(
            &self,
            _owner: &str,
            _repo: &str,
            _pr_number: u64,
        ) -> Result<Vec<ReviewThread>> {
            Ok(self.threads.clone())
        }
    }

    fn make_comment(author: &str, body: &str) -> ThreadComment {
        ThreadComment {
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
    fn thread_last_comment() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment("alice", "First comment"),
                make_comment("bob", "Second comment"),
            ],
        );

        let last = thread.last_comment().unwrap();
        assert_eq!(last.author, "bob");
        assert_eq!(last.body, "Second comment");
    }

    #[test]
    fn thread_last_comment_empty() {
        let thread = ReviewThread {
            id: "T1".to_string(),
            is_resolved: false,
            path: None,
            line: None,
            comments: vec![],
        };

        assert!(thread.last_comment().is_none());
    }

    #[test]
    fn test_client_returns_threads() {
        let client = TestThreadsClient {
            threads: vec![
                make_thread("T1", false, vec![make_comment("alice", "Question")]),
                make_thread("T2", true, vec![make_comment("bob", "Answer")]),
            ],
        };

        let threads = client.fetch_threads("owner", "repo", 1).unwrap();
        assert_eq!(threads.len(), 2);
        assert!(!threads[0].is_resolved);
        assert!(threads[1].is_resolved);
    }

    #[test]
    fn thread_needs_response_unresolved_from_other() {
        let thread = make_thread("T1", false, vec![make_comment("reviewer", "Please fix this")]);
        assert!(thread.needs_response());
    }

    #[test]
    fn thread_needs_response_resolved() {
        let thread = make_thread("T1", true, vec![make_comment("reviewer", "Please fix this")]);
        assert!(!thread.needs_response());
    }

    #[test]
    fn thread_needs_response_last_from_claude() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment("reviewer", "Please fix this"),
                make_comment("claude-bot", "🤖 From Claude: Fixed!"),
            ],
        );
        assert!(!thread.needs_response());
    }

    #[test]
    fn thread_needs_response_claude_then_reviewer() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment("claude-bot", "🤖 From Claude: Fixed!"),
                make_comment("reviewer", "Actually, that's not quite right"),
            ],
        );
        assert!(thread.needs_response());
    }

    #[test]
    fn thread_needs_response_empty() {
        let thread = ReviewThread {
            id: "T1".to_string(),
            is_resolved: false,
            path: None,
            line: None,
            comments: vec![],
        };
        assert!(!thread.needs_response());
    }

    #[test]
    fn find_actionable_threads_filters() {
        let threads = vec![
            make_thread("T1", false, vec![make_comment("reviewer", "Question")]),
            make_thread("T2", true, vec![make_comment("reviewer", "Resolved")]),
            make_thread(
                "T3",
                false,
                vec![make_comment("bot", "🤖 From Claude: Done")],
            ),
            make_thread("T4", false, vec![make_comment("reviewer", "Another question")]),
        ];

        let actionable = find_actionable_threads(threads);
        assert_eq!(actionable.len(), 2);
        assert_eq!(actionable[0].thread.id, "T1");
        assert_eq!(actionable[1].thread.id, "T4");
    }

    #[test]
    fn actionable_thread_location() {
        let thread = make_thread("T1", false, vec![make_comment("a", "b")]);
        let actionable = ActionableThread { thread };
        assert_eq!(actionable.location(), "src/main.rs:42");
    }

    #[test]
    fn actionable_thread_location_no_line() {
        let mut thread = make_thread("T1", false, vec![make_comment("a", "b")]);
        thread.line = None;
        let actionable = ActionableThread { thread };
        assert_eq!(actionable.location(), "src/main.rs");
    }
}
