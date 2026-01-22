// PR operations: draft mode checking and description status block management.
// Uses `gh` CLI for PR interactions.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// Marker comments for the status block in PR description.
const STATUS_BLOCK_START: &str = "<!-- pr-loop-status-start -->";
const STATUS_BLOCK_END: &str = "<!-- pr-loop-status-end -->";

/// Trait for PR operations, allowing test implementations.
pub trait PrClient {
    /// Check if the PR is in draft mode.
    fn is_draft(&self, owner: &str, repo: &str, pr_number: u64) -> Result<bool>;

    /// Get the current PR description body.
    fn get_body(&self, owner: &str, repo: &str, pr_number: u64) -> Result<String>;

    /// Update the PR description body.
    fn set_body(&self, owner: &str, repo: &str, pr_number: u64, body: &str) -> Result<()>;

    /// Mark the PR as ready for review (non-draft).
    fn mark_ready(&self, owner: &str, repo: &str, pr_number: u64) -> Result<()>;
}

/// Real PR client that uses the `gh` CLI.
pub struct RealPrClient;

impl PrClient for RealPrClient {
    fn is_draft(&self, owner: &str, repo: &str, pr_number: u64) -> Result<bool> {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", owner, repo),
                "--json",
                "isDraft",
            ])
            .output()
            .context("Failed to run 'gh pr view'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to check PR draft status: {}", stderr.trim());
        }

        #[derive(Deserialize)]
        struct DraftOnly {
            #[serde(rename = "isDraft")]
            is_draft: bool,
        }

        let view: DraftOnly =
            serde_json::from_slice(&output.stdout).context("Failed to parse PR view output")?;

        Ok(view.is_draft)
    }

    fn get_body(&self, owner: &str, repo: &str, pr_number: u64) -> Result<String> {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", owner, repo),
                "--json",
                "body",
            ])
            .output()
            .context("Failed to run 'gh pr view'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to get PR body: {}", stderr.trim());
        }

        #[derive(Deserialize)]
        struct BodyOnly {
            body: String,
        }

        let view: BodyOnly =
            serde_json::from_slice(&output.stdout).context("Failed to parse PR view output")?;

        Ok(view.body)
    }

    fn set_body(&self, owner: &str, repo: &str, pr_number: u64, body: &str) -> Result<()> {
        let output = Command::new("gh")
            .args([
                "pr",
                "edit",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", owner, repo),
                "--body",
                body,
            ])
            .output()
            .context("Failed to run 'gh pr edit'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to update PR body: {}", stderr.trim());
        }

        Ok(())
    }

    fn mark_ready(&self, owner: &str, repo: &str, pr_number: u64) -> Result<()> {
        let output = Command::new("gh")
            .args([
                "pr",
                "ready",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", owner, repo),
            ])
            .output()
            .context("Failed to run 'gh pr ready'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to mark PR as ready: {}", stderr.trim());
        }

        Ok(())
    }
}

/// Build the status block content for the PR description.
pub fn build_status_block(status_message: Option<&str>) -> String {
    let mut block = String::new();
    block.push_str(STATUS_BLOCK_START);
    block.push('\n');
    block.push_str("> **🤖 LLM Iteration In Progress**\n");
    block.push_str("> \n");
    block.push_str("> This PR is being iterated on with help from an LLM assistant.\n");
    block.push_str("> It is not ready for human review yet.\n");
    if let Some(msg) = status_message {
        block.push_str("> \n");
        block.push_str(&format!("> **Status:** {}\n", msg));
    }
    block.push_str(STATUS_BLOCK_END);
    block
}

/// Update the PR description to include or update the status block.
/// Returns the new body with the status block at the top.
pub fn update_body_with_status(current_body: &str, status_message: Option<&str>) -> String {
    let body_without_status = remove_status_block(current_body);
    let status_block = build_status_block(status_message);

    if body_without_status.is_empty() {
        status_block
    } else {
        format!("{}\n\n{}", status_block, body_without_status)
    }
}

/// Remove the status block from the PR description.
/// Returns the body without the status block.
pub fn remove_status_block(body: &str) -> String {
    if let Some(start_idx) = body.find(STATUS_BLOCK_START) {
        if let Some(end_idx) = body.find(STATUS_BLOCK_END) {
            let end_idx = end_idx + STATUS_BLOCK_END.len();
            let before = &body[..start_idx];
            let after = &body[end_idx..];

            // Clean up extra newlines
            let before = before.trim_end();
            let after = after.trim_start();

            if before.is_empty() {
                after.to_string()
            } else if after.is_empty() {
                before.to_string()
            } else {
                format!("{}\n\n{}", before, after)
            }
        } else {
            // Malformed: start without end, just return original
            body.to_string()
        }
    } else {
        body.to_string()
    }
}

/// Check if the body contains a status block.
pub fn has_status_block(body: &str) -> bool {
    body.contains(STATUS_BLOCK_START) && body.contains(STATUS_BLOCK_END)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test PR client that returns fixed values.
    pub struct TestPrClient {
        pub is_draft: bool,
        pub body: String,
        pub set_body_called: std::cell::RefCell<Option<String>>,
        pub mark_ready_called: std::cell::RefCell<bool>,
    }

    impl TestPrClient {
        pub fn new(is_draft: bool, body: &str) -> Self {
            Self {
                is_draft,
                body: body.to_string(),
                set_body_called: std::cell::RefCell::new(None),
                mark_ready_called: std::cell::RefCell::new(false),
            }
        }
    }

    impl PrClient for TestPrClient {
        fn is_draft(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<bool> {
            Ok(self.is_draft)
        }

        fn get_body(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<String> {
            Ok(self.body.clone())
        }

        fn set_body(&self, _owner: &str, _repo: &str, _pr_number: u64, body: &str) -> Result<()> {
            *self.set_body_called.borrow_mut() = Some(body.to_string());
            Ok(())
        }

        fn mark_ready(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<()> {
            *self.mark_ready_called.borrow_mut() = true;
            Ok(())
        }
    }

    #[test]
    fn build_status_block_without_message() {
        let block = build_status_block(None);
        assert!(block.contains(STATUS_BLOCK_START));
        assert!(block.contains(STATUS_BLOCK_END));
        assert!(block.contains("🤖 LLM Iteration In Progress"));
        assert!(block.contains("not ready for human review"));
        assert!(!block.contains("**Status:**"));
    }

    #[test]
    fn build_status_block_with_message() {
        let block = build_status_block(Some("Working on CI failures"));
        assert!(block.contains(STATUS_BLOCK_START));
        assert!(block.contains(STATUS_BLOCK_END));
        assert!(block.contains("**Status:** Working on CI failures"));
    }

    #[test]
    fn update_body_empty() {
        let result = update_body_with_status("", None);
        assert!(result.starts_with(STATUS_BLOCK_START));
        assert!(result.ends_with(STATUS_BLOCK_END));
    }

    #[test]
    fn update_body_with_existing_content() {
        let result = update_body_with_status("## Summary\n\nThis PR does something.", None);
        assert!(result.starts_with(STATUS_BLOCK_START));
        assert!(result.contains("## Summary"));
        assert!(result.contains("This PR does something."));
    }

    #[test]
    fn update_body_replaces_existing_status() {
        let existing = format!(
            "{}\n> Old status\n{}\n\n## Summary\n\nContent.",
            STATUS_BLOCK_START, STATUS_BLOCK_END
        );
        let result = update_body_with_status(&existing, Some("New status"));
        assert!(result.contains("**Status:** New status"));
        assert!(!result.contains("Old status"));
        assert!(result.contains("## Summary"));
        // Should only have one status block
        assert_eq!(
            result.matches(STATUS_BLOCK_START).count(),
            1,
            "Should have exactly one status block"
        );
    }

    #[test]
    fn remove_status_block_at_start() {
        let body = format!(
            "{}\n> Status content\n{}\n\n## Summary\n\nContent.",
            STATUS_BLOCK_START, STATUS_BLOCK_END
        );
        let result = remove_status_block(&body);
        assert!(!result.contains(STATUS_BLOCK_START));
        assert!(!result.contains(STATUS_BLOCK_END));
        assert!(result.starts_with("## Summary"));
    }

    #[test]
    fn remove_status_block_only_content() {
        let body = format!(
            "{}\n> Status content\n{}",
            STATUS_BLOCK_START, STATUS_BLOCK_END
        );
        let result = remove_status_block(&body);
        assert!(result.is_empty());
    }

    #[test]
    fn remove_status_block_none_present() {
        let body = "## Summary\n\nContent.";
        let result = remove_status_block(body);
        assert_eq!(result, body);
    }

    #[test]
    fn has_status_block_true() {
        let body = format!(
            "{}\n> Status\n{}\n\nContent.",
            STATUS_BLOCK_START, STATUS_BLOCK_END
        );
        assert!(has_status_block(&body));
    }

    #[test]
    fn has_status_block_false() {
        assert!(!has_status_block("## Summary\n\nContent."));
    }

    #[test]
    fn test_client_is_draft() {
        let client = TestPrClient::new(true, "body");
        assert!(client.is_draft("owner", "repo", 1).unwrap());

        let client = TestPrClient::new(false, "body");
        assert!(!client.is_draft("owner", "repo", 1).unwrap());
    }

    #[test]
    fn test_client_set_body() {
        let client = TestPrClient::new(true, "old body");
        client.set_body("owner", "repo", 1, "new body").unwrap();
        assert_eq!(
            *client.set_body_called.borrow(),
            Some("new body".to_string())
        );
    }

    #[test]
    fn test_client_mark_ready() {
        let client = TestPrClient::new(true, "body");
        client.mark_ready("owner", "repo", 1).unwrap();
        assert!(*client.mark_ready_called.borrow());
    }
}
