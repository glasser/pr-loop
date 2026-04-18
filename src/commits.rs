// Fetch commits on a PR via GitHub GraphQL.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// A commit on a PR.
#[derive(Debug, Clone)]
pub struct PrCommit {
    pub sha: String,
    pub abbreviated_sha: String,
    pub message_headline: String,
    pub committed_date: String,
    pub author_name: Option<String>,
    pub author_login: Option<String>,
    /// PR-anchored URL (github.com/owner/repo/pull/N/commits/SHA)
    pub url: String,
}

/// PR identity fields that come back from the commits query, since we're
/// already making that call.
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub title: String,
    pub url: String,
    pub commits: Vec<PrCommit>,
}

/// Trait for fetching PR commits, allowing test implementations.
pub trait CommitsClient {
    fn fetch_pr_info(&self, owner: &str, repo: &str, pr_number: u64) -> Result<PrInfo>;
}

pub struct RealCommitsClient;

impl CommitsClient for RealCommitsClient {
    fn fetch_pr_info(&self, owner: &str, repo: &str, pr_number: u64) -> Result<PrInfo> {
        fetch_pr_info_from_graphql(owner, repo, pr_number)
    }
}

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
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    commits: CommitsConnection,
}

#[derive(Deserialize)]
struct CommitsConnection {
    nodes: Vec<PrCommitNode>,
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
struct PrCommitNode {
    url: String,
    commit: CommitNode,
}

#[derive(Deserialize)]
struct CommitNode {
    oid: String,
    #[serde(rename = "abbreviatedOid")]
    abbreviated_oid: String,
    #[serde(rename = "messageHeadline")]
    message_headline: String,
    #[serde(rename = "committedDate")]
    committed_date: String,
    author: Option<AuthorNode>,
}

#[derive(Deserialize)]
struct AuthorNode {
    name: Option<String>,
    user: Option<UserNode>,
}

#[derive(Deserialize)]
struct UserNode {
    login: String,
}

const FETCH_COMMITS_QUERY: &str = include_str!("../graphql/operation/fetch_commits.graphql");

fn fetch_pr_info_from_graphql(owner: &str, repo: &str, pr_number: u64) -> Result<PrInfo> {
    let mut all_commits: Vec<PrCommit> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut title: Option<String> = None;
    let mut url: Option<String> = None;

    loop {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={}", FETCH_COMMITS_QUERY),
            "-f".to_string(),
            format!("owner={}", owner),
            "-f".to_string(),
            format!("repo={}", repo),
            "-F".to_string(),
            format!("pr={}", pr_number),
        ];
        if let Some(c) = &cursor {
            args.push("-f".to_string());
            args.push(format!("cursor={}", c));
        }

        let output = Command::new("gh")
            .args(&args)
            .output()
            .context("Failed to run 'gh api graphql' for commits")?;

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

        let pr = response
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_request)
            .ok_or_else(|| anyhow::anyhow!("PR not found or no access"))?;

        if title.is_none() {
            title = pr.title;
        }
        if url.is_none() {
            url = pr.url;
        }
        let connection = pr.commits;

        for n in connection.nodes {
            all_commits.push(PrCommit {
                sha: n.commit.oid,
                abbreviated_sha: n.commit.abbreviated_oid,
                message_headline: n.commit.message_headline,
                committed_date: n.commit.committed_date,
                author_name: n.commit.author.as_ref().and_then(|a| a.name.clone()),
                author_login: n
                    .commit
                    .author
                    .as_ref()
                    .and_then(|a| a.user.as_ref().map(|u| u.login.clone())),
                url: n.url,
            });
        }

        if !connection.page_info.has_next_page {
            break;
        }
        cursor = connection.page_info.end_cursor;
    }

    Ok(PrInfo {
        title: title.unwrap_or_default(),
        url: url.unwrap_or_default(),
        commits: all_commits,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub struct TestCommitsClient {
        pub info: PrInfo,
    }

    impl CommitsClient for TestCommitsClient {
        fn fetch_pr_info(&self, _owner: &str, _repo: &str, _pr: u64) -> Result<PrInfo> {
            Ok(self.info.clone())
        }
    }
}
