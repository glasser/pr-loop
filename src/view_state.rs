// PR file "viewed" state management via GitHub GraphQL API.
// Manages the per-viewer "Viewed" checkboxes on a PR's Files Changed tab:
// exporting/restoring the full set of per-file states, and bulk mark-all
// operations.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::process::Command;

/// The viewed state of a single file, matching GitHub's `FileViewedState` enum.
///
/// `Dismissed` is a state GitHub applies automatically when a previously-viewed
/// file changes again; there's no mutation to set it directly. The closest
/// equivalent is `unmarkFileAsViewed`, which is what we use when restoring a
/// `Dismissed` entry (see `plan_set`) — this loses the distinction between
/// "never viewed" and "viewed, then changed again", which is a known
/// limitation of `set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileViewedState {
    #[serde(rename = "UNVIEWED")]
    Unviewed,
    #[serde(rename = "VIEWED")]
    Viewed,
    #[serde(rename = "DISMISSED")]
    Dismissed,
}

/// A single file's path and viewed state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrFileState {
    pub path: String,
    #[serde(rename = "viewerViewedState")]
    pub viewed_state: FileViewedState,
}

/// The on-disk format written by `export` and read by `set`.
/// Carries the repo/PR it was exported from so `set` can refuse to apply it
/// to the wrong PR by accident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewStateFile {
    pub owner: String,
    pub repo: String,
    pub pr: u64,
    pub files: Vec<PrFileState>,
}

/// Trait for reading/writing PR file viewed state, allowing test implementations.
pub trait ViewStateClient {
    /// Fetch the PR's GraphQL node ID and the viewed state of every file in the PR.
    fn fetch_pr_files(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(String, Vec<PrFileState>)>;

    /// Mark the given paths as viewed. `pr_id` is the PR's GraphQL node ID.
    fn mark_viewed(&self, pr_id: &str, paths: &[String]) -> Result<()>;

    /// Mark the given paths as unviewed. `pr_id` is the PR's GraphQL node ID.
    fn mark_unviewed(&self, pr_id: &str, paths: &[String]) -> Result<()>;
}

/// Real client that uses `gh api graphql`.
pub struct RealViewStateClient;

impl ViewStateClient for RealViewStateClient {
    fn fetch_pr_files(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(String, Vec<PrFileState>)> {
        fetch_pr_files_graphql(owner, repo, pr_number)
    }

    fn mark_viewed(&self, pr_id: &str, paths: &[String]) -> Result<()> {
        run_batched_mutation("markFileAsViewed", pr_id, paths)
    }

    fn mark_unviewed(&self, pr_id: &str, paths: &[String]) -> Result<()> {
        run_batched_mutation("unmarkFileAsViewed", pr_id, paths)
    }
}

/// Given the saved state and the set of paths that currently exist in the PR,
/// compute which paths to mark viewed, which to mark unviewed, and which
/// saved paths are gone from the PR (added/removed by later commits).
///
/// `Unviewed` and `Dismissed` are both restored via `unmarkFileAsViewed` —
/// see the `Dismissed` caveat on [`FileViewedState`].
pub struct SetPlan {
    pub to_mark_viewed: Vec<String>,
    pub to_mark_unviewed: Vec<String>,
    pub missing_paths: Vec<String>,
}

pub fn plan_set(saved_files: &[PrFileState], current_paths: &HashSet<String>) -> SetPlan {
    let mut to_mark_viewed = Vec::new();
    let mut to_mark_unviewed = Vec::new();
    let mut missing_paths = Vec::new();

    for f in saved_files {
        if !current_paths.contains(&f.path) {
            missing_paths.push(f.path.clone());
            continue;
        }
        match f.viewed_state {
            FileViewedState::Viewed => to_mark_viewed.push(f.path.clone()),
            FileViewedState::Unviewed | FileViewedState::Dismissed => {
                to_mark_unviewed.push(f.path.clone())
            }
        }
    }

    SetPlan {
        to_mark_viewed,
        to_mark_unviewed,
        missing_paths,
    }
}

/// Paths that need a `markFileAsViewed` call to become viewed (i.e. every
/// file not already `Viewed`).
pub fn paths_needing_mark_viewed(files: &[PrFileState]) -> Vec<String> {
    files
        .iter()
        .filter(|f| f.viewed_state != FileViewedState::Viewed)
        .map(|f| f.path.clone())
        .collect()
}

/// Paths that need an `unmarkFileAsViewed` call to become unviewed (i.e.
/// every file not already `Unviewed` — `Dismissed` files are included so
/// their underlying state actually converges to `Unviewed`, even though
/// they already render without a checkmark).
pub fn paths_needing_mark_unviewed(files: &[PrFileState]) -> Vec<String> {
    files
        .iter()
        .filter(|f| f.viewed_state != FileViewedState::Unviewed)
        .map(|f| f.path.clone())
        .collect()
}

/// The file extension GitHub's own "Files changed" extension filter would use
/// for `path` (the substring after the last `.` in the file name), or `None`
/// for an extensionless file like `Dockerfile`.
fn file_extension(path: &str) -> Option<&str> {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
}

/// Restrict `files` to those whose extension is in `extensions` (matched
/// case-insensitively; a leading `.` on either side is ignored). An empty
/// `extensions` list means "no filter" — every file is kept, matching
/// `mark-all-*`'s default behavior when `--extension` isn't passed.
pub fn filter_by_extensions(files: &[PrFileState], extensions: &[String]) -> Vec<PrFileState> {
    if extensions.is_empty() {
        return files.to_vec();
    }

    let wanted: HashSet<String> = extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_lowercase())
        .collect();

    files
        .iter()
        .filter(|f| {
            file_extension(&f.path)
                .is_some_and(|ext| wanted.contains(&ext.to_lowercase()))
        })
        .cloned()
        .collect()
}

// GraphQL response structures for fetching PR files.
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
struct FetchFilesData {
    repository: Option<RepositoryData>,
}

#[derive(Deserialize)]
struct RepositoryData {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestData>,
}

#[derive(Deserialize)]
struct PullRequestData {
    id: String,
    files: FilesConnection,
}

#[derive(Deserialize)]
struct FilesConnection {
    nodes: Vec<PrFileState>,
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

/// GraphQL query for fetching a PR's files (loaded from graphql/operation/).
const FETCH_PR_FILES_QUERY: &str = include_str!("../graphql/operation/fetch_pr_files.graphql");

/// Fetch a PR's node ID and every file's viewed state, paginating as needed.
fn fetch_pr_files_graphql(owner: &str, repo: &str, pr_number: u64) -> Result<(String, Vec<PrFileState>)> {
    let mut all_files: Vec<PrFileState> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pr_id: Option<String> = None;

    loop {
        let (id, nodes, page_info) = fetch_pr_files_page(owner, repo, pr_number, cursor.as_deref())?;
        pr_id.get_or_insert(id);
        all_files.extend(nodes);

        if !page_info.has_next_page {
            break;
        }
        cursor = page_info.end_cursor;
    }

    let pr_id = pr_id.ok_or_else(|| {
        anyhow::anyhow!("PR not found: {}/{}#{}", owner, repo, pr_number)
    })?;

    Ok((pr_id, all_files))
}

fn fetch_pr_files_page(
    owner: &str,
    repo: &str,
    pr_number: u64,
    cursor: Option<&str>,
) -> Result<(String, Vec<PrFileState>, PageInfo)> {
    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={}", FETCH_PR_FILES_QUERY),
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
        .context("Failed to run 'gh api graphql' for fetch PR files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL query failed: {}", stderr.trim());
    }

    let response: GraphQLResponse<FetchFilesData> = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    let pr = response
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .ok_or_else(|| anyhow::anyhow!("No pull request data in response"))?;

    Ok((pr.id, pr.files.nodes, pr.files.page_info))
}

/// Max files to include in a single batched mutation request. GitHub enforces
/// a query-cost limit, so we don't jam hundreds of aliased mutations into one
/// call — see NOTES_view_state.md for the reasoning.
const BATCH_SIZE: usize = 30;

/// Run `mutation_field` (either `markFileAsViewed` or `unmarkFileAsViewed`)
/// against every path, batching many files into few `gh api graphql` calls by
/// aliasing multiple mutation fields in a single request.
fn run_batched_mutation(mutation_field: &str, pr_id: &str, paths: &[String]) -> Result<()> {
    for chunk in paths.chunks(BATCH_SIZE) {
        run_one_batch(mutation_field, pr_id, chunk)?;
    }
    Ok(())
}

/// Build and run one aliased-mutation batch, e.g. for `markFileAsViewed` with
/// paths `["a", "b"]`:
///
/// ```graphql
/// mutation($pid: ID!, $p0: String!, $p1: String!) {
///   m0: markFileAsViewed(input: { pullRequestId: $pid, path: $p0 }) { clientMutationId }
///   m1: markFileAsViewed(input: { pullRequestId: $pid, path: $p1 }) { clientMutationId }
/// }
/// ```
fn run_one_batch(mutation_field: &str, pr_id: &str, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let query = build_batch_mutation(mutation_field, paths.len());

    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={}", query),
        "-f".to_string(),
        format!("pid={}", pr_id),
    ];
    for (i, path) in paths.iter().enumerate() {
        args.push("-f".to_string());
        args.push(format!("p{}={}", i, path));
    }

    let output = Command::new("gh")
        .args(&args)
        .output()
        .context("Failed to run 'gh api graphql' for batched file-viewed mutation")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GraphQL mutation failed: {}", stderr.trim());
    }

    #[derive(Deserialize)]
    struct BatchResponse {
        errors: Option<Vec<GraphQLError>>,
    }

    let response: BatchResponse = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = response.errors {
        let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
        anyhow::bail!("GraphQL errors: {}", messages.join(", "));
    }

    Ok(())
}

/// Build the aliased batch mutation query string for `count` files.
fn build_batch_mutation(mutation_field: &str, count: usize) -> String {
    let mut var_decls = String::from("$pid: ID!");
    let mut body = String::new();

    for i in 0..count {
        var_decls.push_str(&format!(", $p{}: String!", i));
        body.push_str(&format!(
            "  m{i}: {field}(input: {{ pullRequestId: $pid, path: $p{i} }}) {{ clientMutationId }}\n",
            i = i,
            field = mutation_field
        ));
    }

    format!("mutation({}) {{\n{}}}", var_decls, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, state: FileViewedState) -> PrFileState {
        PrFileState {
            path: path.to_string(),
            viewed_state: state,
        }
    }

    // --- serde format for the export/set file ---

    #[test]
    fn viewed_state_serializes_to_github_names() {
        assert_eq!(
            serde_json::to_string(&FileViewedState::Viewed).unwrap(),
            "\"VIEWED\""
        );
        assert_eq!(
            serde_json::to_string(&FileViewedState::Unviewed).unwrap(),
            "\"UNVIEWED\""
        );
        assert_eq!(
            serde_json::to_string(&FileViewedState::Dismissed).unwrap(),
            "\"DISMISSED\""
        );
    }

    #[test]
    fn view_state_file_round_trips_through_json() {
        let original = ViewStateFile {
            owner: "glasser".to_string(),
            repo: "pr-loop-test-repo".to_string(),
            pr: 42,
            files: vec![
                file("src/main.rs", FileViewedState::Viewed),
                file("README.md", FileViewedState::Unviewed),
                file("Cargo.lock", FileViewedState::Dismissed),
            ],
        };

        let json = serde_json::to_string_pretty(&original).unwrap();
        let parsed: ViewStateFile = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.owner, original.owner);
        assert_eq!(parsed.repo, original.repo);
        assert_eq!(parsed.pr, original.pr);
        assert_eq!(parsed.files, original.files);
    }

    #[test]
    fn view_state_file_parses_example_from_notes() {
        let json = r#"{
            "owner": "mdg-private",
            "repo": "monorepo",
            "pr": 21729,
            "files": [
                { "path": "apps/billing/src/main/kotlin/Startup.kt", "viewerViewedState": "VIEWED" },
                { "path": "apps/billing/build.gradle.kts", "viewerViewedState": "DISMISSED" }
            ]
        }"#;

        let parsed: ViewStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.owner, "mdg-private");
        assert_eq!(parsed.pr, 21729);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].viewed_state, FileViewedState::Viewed);
        assert_eq!(parsed.files[1].viewed_state, FileViewedState::Dismissed);
    }

    // --- plan_set ---

    #[test]
    fn plan_set_viewed_goes_to_mark_viewed() {
        let saved = vec![file("a.rs", FileViewedState::Viewed)];
        let current: HashSet<String> = ["a.rs".to_string()].into_iter().collect();

        let plan = plan_set(&saved, &current);
        assert_eq!(plan.to_mark_viewed, vec!["a.rs".to_string()]);
        assert!(plan.to_mark_unviewed.is_empty());
        assert!(plan.missing_paths.is_empty());
    }

    #[test]
    fn plan_set_unviewed_and_dismissed_go_to_mark_unviewed() {
        let saved = vec![
            file("a.rs", FileViewedState::Unviewed),
            file("b.rs", FileViewedState::Dismissed),
        ];
        let current: HashSet<String> = ["a.rs".to_string(), "b.rs".to_string()].into_iter().collect();

        let plan = plan_set(&saved, &current);
        assert!(plan.to_mark_viewed.is_empty());
        assert_eq!(
            plan.to_mark_unviewed,
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
        assert!(plan.missing_paths.is_empty());
    }

    #[test]
    fn plan_set_reports_missing_paths_without_mutating() {
        let saved = vec![
            file("gone.rs", FileViewedState::Viewed),
            file("still-here.rs", FileViewedState::Unviewed),
        ];
        let current: HashSet<String> = ["still-here.rs".to_string()].into_iter().collect();

        let plan = plan_set(&saved, &current);
        assert_eq!(plan.missing_paths, vec!["gone.rs".to_string()]);
        assert!(plan.to_mark_viewed.is_empty());
        assert_eq!(plan.to_mark_unviewed, vec!["still-here.rs".to_string()]);
    }

    #[test]
    fn plan_set_empty_saved_files() {
        let current: HashSet<String> = ["a.rs".to_string()].into_iter().collect();
        let plan = plan_set(&[], &current);
        assert!(plan.to_mark_viewed.is_empty());
        assert!(plan.to_mark_unviewed.is_empty());
        assert!(plan.missing_paths.is_empty());
    }

    // --- mark-all filters ---

    #[test]
    fn paths_needing_mark_viewed_skips_already_viewed() {
        let files = vec![
            file("a.rs", FileViewedState::Viewed),
            file("b.rs", FileViewedState::Unviewed),
            file("c.rs", FileViewedState::Dismissed),
        ];
        let needing = paths_needing_mark_viewed(&files);
        assert_eq!(needing, vec!["b.rs".to_string(), "c.rs".to_string()]);
    }

    #[test]
    fn paths_needing_mark_viewed_all_already_viewed() {
        let files = vec![
            file("a.rs", FileViewedState::Viewed),
            file("b.rs", FileViewedState::Viewed),
        ];
        assert!(paths_needing_mark_viewed(&files).is_empty());
    }

    #[test]
    fn paths_needing_mark_unviewed_skips_already_unviewed_but_includes_dismissed() {
        let files = vec![
            file("a.rs", FileViewedState::Viewed),
            file("b.rs", FileViewedState::Unviewed),
            file("c.rs", FileViewedState::Dismissed),
        ];
        let needing = paths_needing_mark_unviewed(&files);
        assert_eq!(needing, vec!["a.rs".to_string(), "c.rs".to_string()]);
    }

    #[test]
    fn paths_needing_mark_unviewed_all_already_unviewed() {
        let files = vec![
            file("a.rs", FileViewedState::Unviewed),
            file("b.rs", FileViewedState::Unviewed),
        ];
        assert!(paths_needing_mark_unviewed(&files).is_empty());
    }

    // --- filter_by_extensions ---

    #[test]
    fn filter_by_extensions_empty_filter_keeps_everything() {
        let files = vec![
            file("a.rs", FileViewedState::Viewed),
            file("b.yaml", FileViewedState::Unviewed),
        ];
        let filtered = filter_by_extensions(&files, &[]);
        assert_eq!(filtered, files);
    }

    #[test]
    fn filter_by_extensions_single_extension() {
        let files = vec![
            file("a.rs", FileViewedState::Viewed),
            file("b.yaml", FileViewedState::Unviewed),
            file("dir/c.yaml", FileViewedState::Dismissed),
        ];
        let filtered = filter_by_extensions(&files, &["yaml".to_string()]);
        assert_eq!(
            filtered.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["b.yaml", "dir/c.yaml"]
        );
    }

    #[test]
    fn filter_by_extensions_multiple_extensions_are_or() {
        let files = vec![
            file("a.yaml", FileViewedState::Viewed),
            file("b.yml", FileViewedState::Viewed),
            file("c.rs", FileViewedState::Viewed),
        ];
        let filtered = filter_by_extensions(
            &files,
            &["yaml".to_string(), "yml".to_string()],
        );
        assert_eq!(
            filtered.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["a.yaml", "b.yml"]
        );
    }

    #[test]
    fn filter_by_extensions_ignores_leading_dot_and_case() {
        let files = vec![file("a.YAML", FileViewedState::Viewed)];
        let filtered = filter_by_extensions(&files, &[".yaml".to_string()]);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_by_extensions_excludes_extensionless_files() {
        let files = vec![
            file("Dockerfile", FileViewedState::Viewed),
            file("a.yaml", FileViewedState::Viewed),
        ];
        let filtered = filter_by_extensions(&files, &["yaml".to_string()]);
        assert_eq!(filtered.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.yaml"]);
    }

    #[test]
    fn filter_by_extensions_no_matches() {
        let files = vec![file("a.rs", FileViewedState::Viewed)];
        let filtered = filter_by_extensions(&files, &["yaml".to_string()]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_by_extensions_uses_last_dot_segment() {
        let files = vec![file("foo.test.yaml", FileViewedState::Viewed)];
        assert_eq!(filter_by_extensions(&files, &["yaml".to_string()]).len(), 1);
        assert!(filter_by_extensions(&files, &["test".to_string()]).is_empty());
    }

    // --- batch mutation query building ---

    #[test]
    fn build_batch_mutation_single_file() {
        let query = build_batch_mutation("markFileAsViewed", 1);
        assert!(query.contains("mutation($pid: ID!, $p0: String!)"));
        assert!(query.contains("m0: markFileAsViewed(input: { pullRequestId: $pid, path: $p0 }) { clientMutationId }"));
    }

    #[test]
    fn build_batch_mutation_multiple_files() {
        let query = build_batch_mutation("unmarkFileAsViewed", 3);
        assert!(query.contains("$p0: String!, $p1: String!, $p2: String!"));
        assert!(query.contains("m0: unmarkFileAsViewed"));
        assert!(query.contains("m1: unmarkFileAsViewed"));
        assert!(query.contains("m2: unmarkFileAsViewed"));
    }

    #[test]
    fn build_batch_mutation_validates_against_schema() {
        use apollo_compiler::validation::Valid;
        use apollo_compiler::{ExecutableDocument, Schema};
        use std::path::Path;

        let schema_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("graphql/schema/github.graphql");
        let schema_str = std::fs::read_to_string(&schema_path).unwrap();
        let schema: Valid<Schema> =
            Schema::parse_and_validate(&schema_str, "github.graphql").unwrap();

        for field in ["markFileAsViewed", "unmarkFileAsViewed"] {
            for count in [1, 3] {
                let query = build_batch_mutation(field, count);
                ExecutableDocument::parse_and_validate(&schema, &query, "batch.graphql")
                    .unwrap_or_else(|e| {
                        panic!("batch mutation for {} (n={}) failed validation: {:?}", field, count, e)
                    });
            }
        }
    }

    // --- test double ---

    /// Test client that tracks calls and returns predefined files.
    pub struct TestViewStateClient {
        pub pr_id: String,
        pub files: Vec<PrFileState>,
        pub marked_viewed: std::sync::Mutex<Vec<String>>,
        pub marked_unviewed: std::sync::Mutex<Vec<String>>,
        pub should_fail: bool,
    }

    impl ViewStateClient for TestViewStateClient {
        fn fetch_pr_files(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<(String, Vec<PrFileState>)> {
            if self.should_fail {
                anyhow::bail!("Test failure");
            }
            Ok((self.pr_id.clone(), self.files.clone()))
        }

        fn mark_viewed(&self, _pr_id: &str, paths: &[String]) -> Result<()> {
            if self.should_fail {
                anyhow::bail!("Test failure");
            }
            self.marked_viewed.lock().unwrap().extend(paths.iter().cloned());
            Ok(())
        }

        fn mark_unviewed(&self, _pr_id: &str, paths: &[String]) -> Result<()> {
            if self.should_fail {
                anyhow::bail!("Test failure");
            }
            self.marked_unviewed.lock().unwrap().extend(paths.iter().cloned());
            Ok(())
        }
    }

    #[test]
    fn test_client_fetch_and_mark() {
        let client = TestViewStateClient {
            pr_id: "PR_1".to_string(),
            files: vec![file("a.rs", FileViewedState::Unviewed)],
            marked_viewed: std::sync::Mutex::new(vec![]),
            marked_unviewed: std::sync::Mutex::new(vec![]),
            should_fail: false,
        };

        let (pr_id, files) = client.fetch_pr_files("o", "r", 1).unwrap();
        assert_eq!(pr_id, "PR_1");
        assert_eq!(files.len(), 1);

        client.mark_viewed(&pr_id, &["a.rs".to_string()]).unwrap();
        assert_eq!(*client.marked_viewed.lock().unwrap(), vec!["a.rs".to_string()]);
    }

    #[test]
    fn test_client_failure() {
        let client = TestViewStateClient {
            pr_id: "PR_1".to_string(),
            files: vec![],
            marked_viewed: std::sync::Mutex::new(vec![]),
            marked_unviewed: std::sync::Mutex::new(vec![]),
            should_fail: true,
        };
        assert!(client.fetch_pr_files("o", "r", 1).is_err());
        assert!(client.mark_viewed("PR_1", &[]).is_err());
        assert!(client.mark_unviewed("PR_1", &[]).is_err());
    }
}
