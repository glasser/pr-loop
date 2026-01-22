// CircleCI API integration.
// Fetches job details and step logs for failed CI checks.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Parsed CircleCI job info from a status check URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircleCiJobInfo {
    /// VCS type (e.g., "gh" for GitHub)
    pub vcs: String,
    /// Repository owner
    pub owner: String,
    /// Repository name
    pub repo: String,
    /// Job number
    pub job_number: u64,
}

impl CircleCiJobInfo {
    /// Returns the project slug in "vcs/owner/repo" format.
    pub fn project_slug(&self) -> String {
        format!("{}/{}/{}", self.vcs, self.owner, self.repo)
    }
}

/// Parse a CircleCI job URL to extract job info.
/// Handles URLs like:
/// - https://circleci.com/gh/owner/repo/123
/// - https://circleci.com/gh/owner/repo/123?some=param
/// - https://app.circleci.com/pipelines/github/owner/repo/456/workflows/abc/jobs/789
pub fn parse_circleci_url(url: &str) -> Option<CircleCiJobInfo> {
    // Try the modern app.circleci.com format first
    if let Some(info) = parse_app_circleci_url(url) {
        return Some(info);
    }
    // Try the classic circleci.com format
    parse_classic_circleci_url(url)
}

/// Parse classic CircleCI URL format: https://circleci.com/gh/owner/repo/123
fn parse_classic_circleci_url(url: &str) -> Option<CircleCiJobInfo> {
    // Strip query params
    let url = url.split('?').next()?;

    // Remove trailing slash if present
    let url = url.trim_end_matches('/');

    // Expected format: https://circleci.com/{vcs}/{owner}/{repo}/{job_number}
    let parts: Vec<&str> = url.split('/').collect();

    // Find the circleci.com part
    let cci_idx = parts.iter().position(|&p| p == "circleci.com")?;

    // Need at least 4 more parts after circleci.com: vcs, owner, repo, job_number
    if parts.len() < cci_idx + 5 {
        return None;
    }

    let vcs = parts[cci_idx + 1];
    let owner = parts[cci_idx + 2];
    let repo = parts[cci_idx + 3];
    let job_number_str = parts[cci_idx + 4];

    let job_number = job_number_str.parse().ok()?;

    Some(CircleCiJobInfo {
        vcs: vcs.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        job_number,
    })
}

/// Parse modern app.circleci.com URL format.
/// Example: https://app.circleci.com/pipelines/github/owner/repo/456/workflows/abc/jobs/789
fn parse_app_circleci_url(url: &str) -> Option<CircleCiJobInfo> {
    // Strip query params
    let url = url.split('?').next()?;

    // Must contain app.circleci.com
    if !url.contains("app.circleci.com") {
        return None;
    }

    // Must have /jobs/ to get the job number
    let jobs_idx = url.find("/jobs/")?;
    let after_jobs = &url[jobs_idx + 6..];
    let job_number_str = after_jobs.split('/').next()?;
    let job_number: u64 = job_number_str.parse().ok()?;

    // Parse the pipelines part: /pipelines/{vcs_type}/{owner}/{repo}/
    let pipelines_idx = url.find("/pipelines/")?;
    let after_pipelines = &url[pipelines_idx + 11..];
    let parts: Vec<&str> = after_pipelines.split('/').collect();

    if parts.len() < 3 {
        return None;
    }

    // Map vcs type: "github" -> "gh", "bitbucket" -> "bb"
    let vcs = match parts[0] {
        "github" => "gh",
        "bitbucket" => "bb",
        other => other,
    };

    Some(CircleCiJobInfo {
        vcs: vcs.to_string(),
        owner: parts[1].to_string(),
        repo: parts[2].to_string(),
        job_number,
    })
}

/// A step within a CircleCI job.
#[derive(Debug, Clone)]
pub struct JobStep {
    pub name: String,
    pub actions: Vec<StepAction>,
}

/// An action within a step.
#[derive(Debug, Clone)]
pub struct StepAction {
    pub index: u32,
    pub step: u32,
    pub failed: bool,
}

/// Details of a CircleCI job.
#[derive(Debug, Clone)]
pub struct JobDetails {
    pub job_name: String,
    pub steps: Vec<JobStep>,
}

/// Output from a step (stdout and stderr).
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub output: String,
    pub error: String,
}

/// Log output for a failed step.
#[derive(Debug, Clone)]
pub struct FailedStepLog {
    pub job_name: String,
    pub step_name: String,
    pub output: String,
    pub error: String,
}

/// Trait for CircleCI API operations.
pub trait CircleCiClient {
    /// Fetch job details from the v1.1 API.
    fn fetch_job_details(&self, job_info: &CircleCiJobInfo) -> Result<JobDetails>;

    /// Fetch step output from the private API.
    fn fetch_step_output(
        &self,
        job_info: &CircleCiJobInfo,
        task_index: u32,
        step_id: u32,
    ) -> Result<StepOutput>;
}

/// Real CircleCI client using reqwest.
pub struct RealCircleCiClient {
    token: String,
}

impl RealCircleCiClient {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

// Response types for JSON deserialization
#[derive(Deserialize)]
struct JobDetailsResponse {
    steps: Vec<StepResponse>,
    workflows: WorkflowsResponse,
}

#[derive(Deserialize)]
struct StepResponse {
    name: String,
    actions: Vec<ActionResponse>,
}

#[derive(Deserialize)]
struct ActionResponse {
    index: u32,
    step: u32,
    failed: Option<bool>,
}

#[derive(Deserialize)]
struct WorkflowsResponse {
    job_name: String,
}

impl CircleCiClient for RealCircleCiClient {
    fn fetch_job_details(&self, job_info: &CircleCiJobInfo) -> Result<JobDetails> {
        // Use blocking reqwest since we're in sync code
        let client = reqwest::blocking::Client::new();

        let url = format!(
            "https://circleci.com/api/v1.1/project/{}/{}",
            job_info.project_slug(),
            job_info.job_number
        );

        let response = client
            .get(&url)
            .header("Circle-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .context("Failed to send request to CircleCI API")?;

        if response.status() == 404 {
            anyhow::bail!("Job not found: {}", job_info.job_number);
        }
        if response.status() == 429 {
            anyhow::bail!("CircleCI API rate limited");
        }
        if !response.status().is_success() {
            anyhow::bail!("CircleCI API error: {}", response.status());
        }

        let details: JobDetailsResponse = response
            .json()
            .context("Failed to parse CircleCI job details")?;

        Ok(JobDetails {
            job_name: details.workflows.job_name,
            steps: details
                .steps
                .into_iter()
                .map(|s| JobStep {
                    name: s.name,
                    actions: s
                        .actions
                        .into_iter()
                        .map(|a| StepAction {
                            index: a.index,
                            step: a.step,
                            failed: a.failed.unwrap_or(false),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    fn fetch_step_output(
        &self,
        job_info: &CircleCiJobInfo,
        task_index: u32,
        step_id: u32,
    ) -> Result<StepOutput> {
        let client = reqwest::blocking::Client::new();
        let base = format!(
            "https://circleci.com/api/private/output/raw/{}/{}",
            job_info.project_slug(),
            job_info.job_number
        );

        // Fetch stdout
        let output_url = format!("{}/output/{}/{}", base, task_index, step_id);
        let output = client
            .get(&output_url)
            .header("Circle-Token", &self.token)
            .send()
            .and_then(|r| r.text())
            .unwrap_or_default();

        // Fetch stderr
        let error_url = format!("{}/error/{}/{}", base, task_index, step_id);
        let error = client
            .get(&error_url)
            .header("Circle-Token", &self.token)
            .send()
            .and_then(|r| r.text())
            .unwrap_or_default();

        Ok(StepOutput { output, error })
    }
}

/// Fetch logs for failed steps in a job.
pub fn get_failed_step_logs(
    client: &dyn CircleCiClient,
    job_info: &CircleCiJobInfo,
) -> Result<Vec<FailedStepLog>> {
    let details = client.fetch_job_details(job_info)?;

    let mut logs = Vec::new();

    for step in &details.steps {
        for action in &step.actions {
            if action.failed {
                let output = client.fetch_step_output(job_info, action.index, action.step)?;
                logs.push(FailedStepLog {
                    job_name: details.job_name.clone(),
                    step_name: step.name.clone(),
                    output: output.output,
                    error: output.error,
                });
            }
        }
    }

    Ok(logs)
}

/// Check if a URL is a CircleCI URL.
pub fn is_circleci_url(url: &str) -> bool {
    url.contains("circleci.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classic_url() {
        let url = "https://circleci.com/gh/owner/repo/12345";
        let info = parse_circleci_url(url).unwrap();
        assert_eq!(info.vcs, "gh");
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
        assert_eq!(info.job_number, 12345);
    }

    #[test]
    fn parse_classic_url_with_query() {
        let url = "https://circleci.com/gh/owner/repo/12345?utm_source=github";
        let info = parse_circleci_url(url).unwrap();
        assert_eq!(info.job_number, 12345);
    }

    #[test]
    fn parse_classic_url_with_trailing_slash() {
        let url = "https://circleci.com/gh/owner/repo/12345/";
        let info = parse_circleci_url(url).unwrap();
        assert_eq!(info.job_number, 12345);
    }

    #[test]
    fn parse_app_url() {
        let url = "https://app.circleci.com/pipelines/github/owner/repo/456/workflows/abc-123/jobs/789";
        let info = parse_circleci_url(url).unwrap();
        assert_eq!(info.vcs, "gh");
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
        assert_eq!(info.job_number, 789);
    }

    #[test]
    fn parse_app_url_bitbucket() {
        let url =
            "https://app.circleci.com/pipelines/bitbucket/owner/repo/456/workflows/abc/jobs/999";
        let info = parse_circleci_url(url).unwrap();
        assert_eq!(info.vcs, "bb");
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
        assert_eq!(info.job_number, 999);
    }

    #[test]
    fn parse_invalid_url() {
        assert!(parse_circleci_url("https://github.com/owner/repo").is_none());
        assert!(parse_circleci_url("https://circleci.com/gh/owner").is_none());
        assert!(parse_circleci_url("not a url").is_none());
    }

    #[test]
    fn project_slug() {
        let info = CircleCiJobInfo {
            vcs: "gh".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            job_number: 123,
        };
        assert_eq!(info.project_slug(), "gh/owner/repo");
    }

    #[test]
    fn is_circleci_url_true() {
        assert!(is_circleci_url("https://circleci.com/gh/owner/repo/123"));
        assert!(is_circleci_url("https://app.circleci.com/pipelines/..."));
    }

    #[test]
    fn is_circleci_url_false() {
        assert!(!is_circleci_url("https://github.com/owner/repo"));
        assert!(!is_circleci_url("https://example.com"));
    }

    // Test implementation for unit testing without real API calls
    pub struct TestCircleCiClient {
        pub job_details: Option<JobDetails>,
        pub step_outputs: Vec<StepOutput>,
    }

    impl CircleCiClient for TestCircleCiClient {
        fn fetch_job_details(&self, _job_info: &CircleCiJobInfo) -> Result<JobDetails> {
            self.job_details
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No job details configured"))
        }

        fn fetch_step_output(
            &self,
            _job_info: &CircleCiJobInfo,
            task_index: u32,
            _step_id: u32,
        ) -> Result<StepOutput> {
            self.step_outputs
                .get(task_index as usize)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("No step output configured"))
        }
    }

    #[test]
    fn get_failed_step_logs_filters_failed() {
        let client = TestCircleCiClient {
            job_details: Some(JobDetails {
                job_name: "test-job".to_string(),
                steps: vec![
                    JobStep {
                        name: "Checkout".to_string(),
                        actions: vec![StepAction {
                            index: 0,
                            step: 0,
                            failed: false,
                        }],
                    },
                    JobStep {
                        name: "Run tests".to_string(),
                        actions: vec![StepAction {
                            index: 1,
                            step: 0,
                            failed: true,
                        }],
                    },
                ],
            }),
            step_outputs: vec![
                StepOutput {
                    output: "checkout ok".to_string(),
                    error: "".to_string(),
                },
                StepOutput {
                    output: "test output".to_string(),
                    error: "test failed: assertion error".to_string(),
                },
            ],
        };

        let job_info = CircleCiJobInfo {
            vcs: "gh".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            job_number: 123,
        };

        let logs = get_failed_step_logs(&client, &job_info).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].step_name, "Run tests");
        assert_eq!(logs[0].error, "test failed: assertion error");
    }

    #[test]
    fn get_failed_step_logs_empty_when_all_pass() {
        let client = TestCircleCiClient {
            job_details: Some(JobDetails {
                job_name: "test-job".to_string(),
                steps: vec![JobStep {
                    name: "Checkout".to_string(),
                    actions: vec![StepAction {
                        index: 0,
                        step: 0,
                        failed: false,
                    }],
                }],
            }),
            step_outputs: vec![],
        };

        let job_info = CircleCiJobInfo {
            vcs: "gh".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            job_number: 123,
        };

        let logs = get_failed_step_logs(&client, &job_info).unwrap();
        assert!(logs.is_empty());
    }
}
