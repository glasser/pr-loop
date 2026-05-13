// CI status check handling.
// Fetches and filters PR status checks using the GitHub API.

use anyhow::{Context, Result};
use glob::Pattern;
use serde::Deserialize;
use std::process::Command;

/// Status of a CI check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Pending,
    Skipping,
    Cancelled,
}

impl CheckStatus {
    fn from_bucket(bucket: &str) -> Self {
        match bucket {
            "pass" => CheckStatus::Pass,
            "fail" => CheckStatus::Fail,
            "pending" => CheckStatus::Pending,
            "skipping" => CheckStatus::Skipping,
            "cancel" => CheckStatus::Cancelled,
            _ => CheckStatus::Pending, // Default unknown states to pending
        }
    }
}

/// CircleCI's commit-status description when it auto-cancels a redundant
/// build. The job posts `failure` to the status API, but the newer pipeline
/// is still in flight and will overwrite the status — so we treat this as
/// pending rather than a real failure.
const CIRCLECI_CANCELED_DESCRIPTION: &str = "Your CircleCI tests were canceled";

/// Classify a check given its `gh pr checks` bucket and description.
/// Reclassifies CircleCI auto-cancel failures as pending; otherwise defers
/// to [`CheckStatus::from_bucket`].
fn classify_check(bucket: &str, description: Option<&str>) -> CheckStatus {
    let base = CheckStatus::from_bucket(bucket);
    if base == CheckStatus::Fail && description == Some(CIRCLECI_CANCELED_DESCRIPTION) {
        return CheckStatus::Pending;
    }
    base
}

/// A single CI check result.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
}

/// Summary of all checks for a PR.
#[derive(Debug, Clone)]
pub struct ChecksSummary {
    pub checks: Vec<Check>,
}

impl ChecksSummary {
    /// Returns checks that have failed.
    pub fn failed(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .collect()
    }

    /// Returns checks that are still pending.
    pub fn pending(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Pending)
            .collect()
    }

}

/// Trait for fetching checks, allowing test implementations.
pub trait ChecksClient {
    fn fetch_checks(&self, owner: &str, repo: &str, pr_number: u64) -> Result<Vec<Check>>;
}

/// Real client that uses `gh pr checks`.
pub struct RealChecksClient;

impl ChecksClient for RealChecksClient {
    fn fetch_checks(&self, owner: &str, repo: &str, pr_number: u64) -> Result<Vec<Check>> {
        fetch_checks_from_gh(owner, repo, pr_number)
    }
}

#[derive(Deserialize)]
struct GhCheck {
    name: String,
    bucket: String,
    link: Option<String>,
    description: Option<String>,
}

/// Fetch checks using `gh pr checks --json`.
fn fetch_checks_from_gh(owner: &str, repo: &str, pr_number: u64) -> Result<Vec<Check>> {
    let output = Command::new("gh")
        .args([
            "pr",
            "checks",
            &pr_number.to_string(),
            "--repo",
            &format!("{}/{}", owner, repo),
            "--json",
            "name,bucket,link,description",
        ])
        .output()
        .context("Failed to run 'gh pr checks'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch checks: {}", stderr.trim());
    }

    let gh_checks: Vec<GhCheck> =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh pr checks output")?;

    Ok(gh_checks
        .into_iter()
        .map(|c| Check {
            status: classify_check(&c.bucket, c.description.as_deref()),
            name: c.name,
            url: c.link,
        })
        .collect())
}

/// Filter checks based on include/exclude glob patterns.
pub fn filter_checks(
    checks: Vec<Check>,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<Vec<Check>> {
    // Compile patterns
    let includes: Vec<Pattern> = include_patterns
        .iter()
        .map(|p| Pattern::new(p).context(format!("Invalid include pattern: {}", p)))
        .collect::<Result<Vec<_>>>()?;

    let excludes: Vec<Pattern> = exclude_patterns
        .iter()
        .map(|p| Pattern::new(p).context(format!("Invalid exclude pattern: {}", p)))
        .collect::<Result<Vec<_>>>()?;

    Ok(checks
        .into_iter()
        .filter(|check| {
            // If include patterns specified, check must match at least one
            let included = if includes.is_empty() {
                true
            } else {
                includes.iter().any(|p| p.matches(&check.name))
            };

            // Check must not match any exclude pattern
            let excluded = excludes.iter().any(|p| p.matches(&check.name));

            included && !excluded
        })
        .collect())
}

/// Fetch and filter checks for a PR.
pub fn get_checks_summary(
    client: &dyn ChecksClient,
    owner: &str,
    repo: &str,
    pr_number: u64,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<ChecksSummary> {
    let checks = client.fetch_checks(owner, repo, pr_number)?;
    let filtered = filter_checks(checks, include_patterns, exclude_patterns)?;
    Ok(ChecksSummary { checks: filtered })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test client that returns predefined checks.
    pub struct TestChecksClient {
        pub checks: Vec<Check>,
    }

    impl ChecksClient for TestChecksClient {
        fn fetch_checks(&self, _owner: &str, _repo: &str, _pr_number: u64) -> Result<Vec<Check>> {
            Ok(self.checks.clone())
        }
    }

    fn make_check(name: &str, status: CheckStatus) -> Check {
        Check {
            name: name.to_string(),
            status,
            url: Some(format!("https://example.com/{}", name)),
        }
    }

    #[test]
    fn check_status_from_bucket() {
        assert_eq!(CheckStatus::from_bucket("pass"), CheckStatus::Pass);
        assert_eq!(CheckStatus::from_bucket("fail"), CheckStatus::Fail);
        assert_eq!(CheckStatus::from_bucket("pending"), CheckStatus::Pending);
        assert_eq!(CheckStatus::from_bucket("skipping"), CheckStatus::Skipping);
        assert_eq!(CheckStatus::from_bucket("cancel"), CheckStatus::Cancelled);
        assert_eq!(CheckStatus::from_bucket("unknown"), CheckStatus::Pending);
    }

    #[test]
    fn classify_check_passes_through_normal_buckets() {
        assert_eq!(classify_check("pass", None), CheckStatus::Pass);
        assert_eq!(classify_check("fail", None), CheckStatus::Fail);
        assert_eq!(
            classify_check("fail", Some("Tests failed: 3 errors")),
            CheckStatus::Fail
        );
        assert_eq!(classify_check("pending", None), CheckStatus::Pending);
    }

    #[test]
    fn classify_check_reclassifies_circleci_cancel_as_pending() {
        // CircleCI auto-cancel-redundant-builds: the older pipeline's jobs
        // post `failure` with this description while a newer pipeline is
        // still running. Treat as pending so wait-until-actionable keeps
        // waiting for the newer pipeline's results to overwrite the status.
        assert_eq!(
            classify_check("fail", Some("Your CircleCI tests were canceled")),
            CheckStatus::Pending
        );
    }

    #[test]
    fn classify_check_strict_match_only() {
        // Don't match similar-but-not-identical descriptions — we want this
        // to be conservative.
        assert_eq!(
            classify_check("fail", Some("Your CircleCI tests were cancelled")),
            CheckStatus::Fail
        );
        assert_eq!(
            classify_check("fail", Some("CircleCI tests were canceled")),
            CheckStatus::Fail
        );
        // Don't promote a non-fail bucket.
        assert_eq!(
            classify_check("pass", Some("Your CircleCI tests were canceled")),
            CheckStatus::Pass
        );
    }

    #[test]
    fn summary_all_passed() {
        let summary = ChecksSummary {
            checks: vec![
                make_check("ci/build", CheckStatus::Pass),
                make_check("ci/test", CheckStatus::Pass),
            ],
        };
        assert!(summary.failed().is_empty());
        assert!(summary.pending().is_empty());
    }

    #[test]
    fn summary_with_failure() {
        let summary = ChecksSummary {
            checks: vec![
                make_check("ci/build", CheckStatus::Pass),
                make_check("ci/test", CheckStatus::Fail),
            ],
        };
        assert_eq!(summary.failed().len(), 1);
        assert_eq!(summary.failed()[0].name, "ci/test");
    }

    #[test]
    fn summary_with_pending() {
        let summary = ChecksSummary {
            checks: vec![
                make_check("ci/build", CheckStatus::Pass),
                make_check("ci/test", CheckStatus::Pending),
            ],
        };
        assert_eq!(summary.pending().len(), 1);
    }

    #[test]
    fn filter_with_include_pattern() {
        let checks = vec![
            make_check("ci/build", CheckStatus::Pass),
            make_check("ci/test", CheckStatus::Pass),
            make_check("lint", CheckStatus::Pass),
        ];

        let filtered = filter_checks(checks, &["ci/*".to_string()], &[]).unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name.starts_with("ci/")));
    }

    #[test]
    fn filter_with_exclude_pattern() {
        let checks = vec![
            make_check("ci/build", CheckStatus::Pass),
            make_check("ci/test", CheckStatus::Pass),
            make_check("lint", CheckStatus::Pass),
        ];

        let filtered = filter_checks(checks, &[], &["lint".to_string()]).unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "lint"));
    }

    #[test]
    fn filter_with_both_patterns() {
        let checks = vec![
            make_check("ci/build", CheckStatus::Pass),
            make_check("ci/test", CheckStatus::Pass),
            make_check("ci/lint", CheckStatus::Pass),
            make_check("other", CheckStatus::Pass),
        ];

        let filtered = filter_checks(
            checks,
            &["ci/*".to_string()],
            &["ci/lint".to_string()],
        )
        .unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|c| c.name == "ci/build"));
        assert!(filtered.iter().any(|c| c.name == "ci/test"));
    }

    #[test]
    fn get_checks_summary_integration() {
        let client = TestChecksClient {
            checks: vec![
                make_check("ci/build", CheckStatus::Pass),
                make_check("ci/test", CheckStatus::Fail),
                make_check("lint", CheckStatus::Pending),
            ],
        };

        let summary =
            get_checks_summary(&client, "owner", "repo", 1, &[], &[]).unwrap();
        assert_eq!(summary.checks.len(), 3);
        assert_eq!(summary.failed().len(), 1);
        assert_eq!(summary.pending().len(), 1);
    }
}
