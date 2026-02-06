// PR review thread handling via GitHub GraphQL API.
// Fetches review threads including resolution status and comments.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// A comment in a review thread.
#[derive(Debug, Clone)]
pub struct ThreadComment {
    pub id: String,
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

    /// Returns human (non-Claude) comments that appear after the specified comment ID.
    /// Returns None if the comment ID is not found in this thread.
    pub fn human_comments_after(&self, comment_id: &str) -> Option<Vec<ThreadComment>> {
        let index = self.comments.iter().position(|c| c.id == comment_id)?;
        let comments_after: Vec<_> = self.comments[index + 1..]
            .iter()
            .filter(|c| !c.body.starts_with(CLAUDE_MARKER))
            .cloned()
            .collect();
        Some(comments_after)
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

    /// Returns true if this thread is "pure Claude" - meaning every comment is either:
    /// - A Claude-marked comment, OR
    /// - From an author who has also posted a Claude-marked comment in this thread
    /// Empty threads are not considered "pure Claude".
    pub fn is_pure_claude(&self) -> bool {
        if self.comments.is_empty() {
            return false;
        }

        // Find all authors who have posted Claude-marked comments
        let claude_authors: std::collections::HashSet<&str> = self
            .comments
            .iter()
            .filter(|c| c.body.starts_with(CLAUDE_MARKER))
            .map(|c| c.author.as_str())
            .collect();

        // Thread is pure-Claude if every comment is either Claude-marked OR from a Claude author
        self.comments.iter().all(|c| {
            c.body.starts_with(CLAUDE_MARKER) || claude_authors.contains(c.author.as_str())
        })
    }

    /// Returns the IDs of all comments in this thread.
    pub fn comment_ids(&self) -> Vec<&str> {
        self.comments.iter().map(|c| c.id.as_str()).collect()
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

    /// Fetch the thread containing a specific comment, returning both the thread and confirming
    /// the comment exists.
    fn fetch_thread_by_comment_id(&self, comment_id: &str) -> Result<ReviewThread>;
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

    fn fetch_thread_by_comment_id(&self, comment_id: &str) -> Result<ReviewThread> {
        fetch_thread_by_comment_id_graphql(comment_id)
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
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
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
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct CommentNode {
    id: String,
    author: Option<AuthorNode>,
    body: String,
}

#[derive(Deserialize)]
struct AuthorNode {
    login: String,
}

/// Fetch threads using GitHub GraphQL API with pagination support.
fn fetch_threads_from_graphql(
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<ReviewThread>> {
    let mut all_threads: Vec<ReviewThread> = Vec::new();
    let mut threads_cursor: Option<String> = None;

    // Paginate through all review threads
    loop {
        let (thread_nodes, page_info) =
            fetch_threads_page(owner, repo, pr_number, threads_cursor.as_deref())?;

        for t in thread_nodes {
            let thread_id = t.id.clone();
            let mut comments: Vec<ThreadComment> = t
                .comments
                .nodes
                .into_iter()
                .map(|c| ThreadComment {
                    id: c.id,
                    author: c.author.map(|a| a.login).unwrap_or_else(|| "ghost".to_string()),
                    body: c.body,
                })
                .collect();

            // If this thread has more comments, fetch them
            if t.comments.page_info.has_next_page {
                let additional_comments =
                    fetch_remaining_comments(&thread_id, t.comments.page_info.end_cursor)?;
                comments.extend(additional_comments);
            }

            all_threads.push(ReviewThread {
                id: t.id,
                is_resolved: t.is_resolved,
                path: t.path,
                line: t.line,
                comments,
            });
        }

        if !page_info.has_next_page {
            break;
        }
        threads_cursor = page_info.end_cursor;
    }

    Ok(all_threads)
}

/// Fetch a single page of review threads.
/// GraphQL query for fetching review threads (loaded from graphql/operation/).
const FETCH_THREADS_QUERY: &str = include_str!("../graphql/operation/fetch_threads.graphql");

fn fetch_threads_page(
    owner: &str,
    repo: &str,
    pr_number: u64,
    cursor: Option<&str>,
) -> Result<(Vec<ReviewThreadNode>, PageInfo)> {
    let query = FETCH_THREADS_QUERY;

    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={}", query),
        "-f".to_string(),
        format!("owner={}", owner),
        "-f".to_string(),
        format!("repo={}", repo),
        "-F".to_string(),
        format!("pr={}", pr_number),
    ];

    if let Some(c) = cursor {
        args.push("-f".to_string());
        args.push(format!("cursor={}", c));
    }

    let output = Command::new("gh")
        .args(&args)
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

    let review_threads = response
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .map(|pr| pr.review_threads)
        .ok_or_else(|| anyhow::anyhow!("No review threads data in response"))?;

    Ok((review_threads.nodes, review_threads.page_info))
}

/// GraphQL query for fetching remaining comments (loaded from graphql/operation/).
const FETCH_REMAINING_COMMENTS_QUERY: &str =
    include_str!("../graphql/operation/fetch_remaining_comments.graphql");

/// Fetch remaining comments for a thread that has more than 100 comments.
fn fetch_remaining_comments(
    thread_id: &str,
    start_cursor: Option<String>,
) -> Result<Vec<ThreadComment>> {
    let mut all_comments: Vec<ThreadComment> = Vec::new();
    let mut cursor = start_cursor;

    loop {
        let query = FETCH_REMAINING_COMMENTS_QUERY;

        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={}", query),
            "-f".to_string(),
            format!("id={}", thread_id),
        ];

        if let Some(c) = &cursor {
            args.push("-f".to_string());
            args.push(format!("cursor={}", c));
        }

        let output = Command::new("gh")
            .args(&args)
            .output()
            .context("Failed to run 'gh api graphql'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("GraphQL query failed: {}", stderr.trim());
        }

        let response: SingleThreadGraphQLResponse = serde_json::from_slice(&output.stdout)
            .context("Failed to parse GraphQL response")?;

        if let Some(errors) = response.errors {
            let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
            anyhow::bail!("GraphQL errors: {}", messages.join(", "));
        }

        let thread_node = response
            .data
            .and_then(|d| d.node)
            .ok_or_else(|| anyhow::anyhow!("Thread not found: {}", thread_id))?;

        let comments: Vec<ThreadComment> = thread_node
            .comments
            .nodes
            .into_iter()
            .map(|c| ThreadComment {
                id: c.id,
                author: c.author.map(|a| a.login).unwrap_or_else(|| "ghost".to_string()),
                body: c.body,
            })
            .collect();

        all_comments.extend(comments);

        if !thread_node.comments.page_info.has_next_page {
            break;
        }
        cursor = thread_node.comments.page_info.end_cursor;
    }

    Ok(all_comments)
}

// GraphQL response structures for single thread query
#[derive(Deserialize)]
struct SingleThreadGraphQLResponse {
    data: Option<SingleThreadData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize)]
struct SingleThreadData {
    node: Option<ReviewThreadNode>,
}

/// GraphQL query for fetching PR info from a comment (loaded from graphql/operation/).
const FETCH_COMMENT_PR_INFO_QUERY: &str =
    include_str!("../graphql/operation/fetch_comment_pr_info.graphql");

/// Fetch the thread containing a specific comment by the comment's ID.
fn fetch_thread_by_comment_id_graphql(comment_id: &str) -> Result<ReviewThread> {
    // First, get the PR info from the comment (GitHub doesn't expose a direct thread field)
    let query = FETCH_COMMENT_PR_INFO_QUERY;

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={}", query),
            "-f",
            &format!("id={}", comment_id),
        ])
        .output()
        .context("Failed to run 'gh api graphql'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL query failed: {}", stderr.trim());
    }

    #[derive(Deserialize)]
    struct CommentQueryResponse {
        data: Option<CommentQueryData>,
        errors: Option<Vec<GraphQLError>>,
    }

    #[derive(Deserialize)]
    struct CommentQueryData {
        node: Option<CommentQueryNode>,
    }

    #[derive(Deserialize)]
    struct CommentQueryNode {
        #[serde(rename = "pullRequest")]
        pull_request: Option<PullRequestInfo>,
    }

    #[derive(Deserialize)]
    struct PullRequestInfo {
        number: u64,
        repository: RepositoryInfo,
    }

    #[derive(Deserialize)]
    struct RepositoryInfo {
        owner: OwnerInfo,
        name: String,
    }

    #[derive(Deserialize)]
    struct OwnerInfo {
        login: String,
    }

    let response: CommentQueryResponse = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    let pr_info = response
        .data
        .and_then(|d| d.node)
        .and_then(|n| n.pull_request)
        .ok_or_else(|| anyhow::anyhow!("Comment not found or not a PR review comment: {}", comment_id))?;

    // Now fetch all threads from the PR and find the one containing this comment
    let threads = fetch_threads_from_graphql(
        &pr_info.repository.owner.login,
        &pr_info.repository.name,
        pr_info.number,
    )?;

    threads
        .into_iter()
        .find(|t| t.comments.iter().any(|c| c.id == comment_id))
        .ok_or_else(|| anyhow::anyhow!("Comment {} not found in any thread", comment_id))
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

        fn fetch_thread_by_comment_id(&self, comment_id: &str) -> Result<ReviewThread> {
            self.threads
                .iter()
                .find(|t| t.comments.iter().any(|c| c.id == comment_id))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Comment not found: {}", comment_id))
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

    #[test]
    fn is_pure_claude_all_claude_comments() {
        let thread = make_thread(
            "T1",
            true,
            vec![
                make_comment("claude-bot", "🤖 From Claude: First"),
                make_comment("claude-bot", "🤖 From Claude: Second"),
            ],
        );
        assert!(thread.is_pure_claude());
    }

    #[test]
    fn is_pure_claude_same_author_claude_and_non_claude() {
        // Same author posts both a regular comment and a Claude-marked comment
        // This IS pure-Claude because the author has posted Claude comments
        let thread = make_thread(
            "T1",
            true,
            vec![
                make_comment("glasser", "Please fix this"),
                make_comment("glasser", "🤖 From Claude: Fixed!"),
            ],
        );
        assert!(thread.is_pure_claude());
    }

    #[test]
    fn is_pure_claude_different_authors_one_without_claude() {
        // Different authors: reviewer has no Claude comments, claude-bot does
        // NOT pure-Claude because reviewer never posted Claude comments
        let thread = make_thread(
            "T1",
            true,
            vec![
                make_comment("reviewer", "Please fix this"),
                make_comment("claude-bot", "🤖 From Claude: Fixed!"),
            ],
        );
        assert!(!thread.is_pure_claude());
    }

    #[test]
    fn is_pure_claude_no_claude_comments() {
        let thread = make_thread("T1", true, vec![make_comment("reviewer", "Looks good")]);
        assert!(!thread.is_pure_claude());
    }

    #[test]
    fn is_pure_claude_empty_thread() {
        let thread = ReviewThread {
            id: "T1".to_string(),
            is_resolved: true,
            path: None,
            line: None,
            comments: vec![],
        };
        assert!(!thread.is_pure_claude());
    }

    #[test]
    fn comment_ids_returns_all_ids() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment("a", "first"),
                make_comment("b", "second"),
            ],
        );
        let ids = thread.comment_ids();
        assert_eq!(ids.len(), 2);
    }

    fn make_comment_with_id(id: &str, author: &str, body: &str) -> ThreadComment {
        ThreadComment {
            id: id.to_string(),
            author: author.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn human_comments_after_returns_human_comments() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment_with_id("C1", "reviewer", "Please fix this"),
                make_comment_with_id("C2", "claude-bot", "🤖 From Claude: Fixed!"),
                make_comment_with_id("C3", "reviewer", "Actually, one more thing"),
                make_comment_with_id("C4", "reviewer", "And another thing"),
            ],
        );

        // Comments after C1 (which includes C2 Claude, C3 human, C4 human)
        // Should return only the human comments (C3, C4)
        let comments = thread.human_comments_after("C1").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "C3");
        assert_eq!(comments[1].id, "C4");
    }

    #[test]
    fn human_comments_after_claude_comment() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment_with_id("C1", "reviewer", "Please fix this"),
                make_comment_with_id("C2", "claude-bot", "🤖 From Claude: Fixed!"),
                make_comment_with_id("C3", "reviewer", "Actually, one more thing"),
            ],
        );

        // Comments after C2 should just be C3
        let comments = thread.human_comments_after("C2").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, "C3");
    }

    #[test]
    fn human_comments_after_last_comment() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment_with_id("C1", "reviewer", "Please fix this"),
                make_comment_with_id("C2", "claude-bot", "🤖 From Claude: Fixed!"),
            ],
        );

        // Comments after C2 (the last one) should be empty
        let comments = thread.human_comments_after("C2").unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn human_comments_after_unknown_comment() {
        let thread = make_thread(
            "T1",
            false,
            vec![make_comment_with_id("C1", "reviewer", "Please fix this")],
        );

        // Unknown comment should return None
        assert!(thread.human_comments_after("unknown").is_none());
    }

    #[test]
    fn human_comments_after_filters_claude() {
        let thread = make_thread(
            "T1",
            false,
            vec![
                make_comment_with_id("C1", "reviewer", "First"),
                make_comment_with_id("C2", "claude-bot", "🤖 From Claude: Response"),
                make_comment_with_id("C3", "other-claude", "🤖 From Claude: Another response"),
            ],
        );

        // Comments after C1 should filter out both Claude comments
        let comments = thread.human_comments_after("C1").unwrap();
        assert!(comments.is_empty());
    }
}
