// GitHub Actions API integration. Parallel to circleci.rs — fetches structured
// annotations and the failed-step portion of job logs for a failing GH Actions
// check, so we can surface them alongside CircleCI failure details.

use crate::circleci::FailedStepLog;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;

/// A failing GH Actions job we know how to look up by URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhActionsJobInfo {
    pub owner: String,
    pub repo: String,
    pub job_id: u64,
}

/// A single check annotation (GitHub's "highlighted error" in the UI).
#[derive(Debug, Clone)]
pub struct Annotation {
    pub message: String,
    pub path: String,
    pub start_line: Option<u32>,
    pub level: String,
}

/// Trait for GH Actions API operations, allowing test implementations.
pub trait GhActionsClient {
    fn fetch_job_details(&self, job_info: &GhActionsJobInfo) -> Result<JobDetails>;
    /// Return the logs from each failed step, keyed by step name. Uses
    /// `gh run view --log-failed` under the hood because the gh CLI handles
    /// per-step attribution cleanly — the raw /jobs/{id}/logs API response is
    /// one concatenated text blob where step boundaries are ambiguous.
    fn fetch_failed_step_logs(
        &self,
        job_info: &GhActionsJobInfo,
    ) -> Result<std::collections::HashMap<String, String>>;
    fn fetch_annotations(&self, job_info: &GhActionsJobInfo) -> Result<Vec<Annotation>>;
}

#[derive(Debug, Clone)]
pub struct JobDetails {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub conclusion: Option<String>,
    // Timestamps were used by an earlier timestamp-based log extraction
    // path; kept on the struct for completeness/tests but currently unused.
    #[allow(dead_code)]
    pub started_at: Option<String>,
    #[allow(dead_code)]
    pub completed_at: Option<String>,
}

pub struct RealGhActionsClient;

#[derive(Deserialize)]
struct JobDetailsResponse {
    name: String,
    steps: Vec<StepResponse>,
}

#[derive(Deserialize)]
struct StepResponse {
    name: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Deserialize)]
struct AnnotationResponse {
    message: String,
    path: String,
    start_line: Option<u32>,
    annotation_level: String,
}

impl GhActionsClient for RealGhActionsClient {
    fn fetch_job_details(&self, job_info: &GhActionsJobInfo) -> Result<JobDetails> {
        let path = format!(
            "/repos/{}/{}/actions/jobs/{}",
            job_info.owner, job_info.repo, job_info.job_id
        );
        let output = Command::new("gh")
            .args(["api", &path])
            .output()
            .context("Failed to run 'gh api' for job details")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api job details failed: {}", stderr.trim());
        }
        let parsed: JobDetailsResponse =
            serde_json::from_slice(&output.stdout).context("parse job details")?;
        Ok(JobDetails {
            name: parsed.name,
            steps: parsed
                .steps
                .into_iter()
                .map(|s| Step {
                    name: s.name,
                    conclusion: s.conclusion,
                    started_at: s.started_at,
                    completed_at: s.completed_at,
                })
                .collect(),
        })
    }

    fn fetch_failed_step_logs(
        &self,
        job_info: &GhActionsJobInfo,
    ) -> Result<std::collections::HashMap<String, String>> {
        let repo = format!("{}/{}", job_info.owner, job_info.repo);
        let output = Command::new("gh")
            .args([
                "run",
                "view",
                "-R",
                &repo,
                "--job",
                &job_info.job_id.to_string(),
                "--log-failed",
            ])
            .output()
            .context("Failed to run 'gh run view --log-failed'")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh run view failed: {}", stderr.trim());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_log_failed_output(&text))
    }

    fn fetch_annotations(&self, job_info: &GhActionsJobInfo) -> Result<Vec<Annotation>> {
        // For GH Actions checks, the check_run_id equals the job_id.
        let path = format!(
            "/repos/{}/{}/check-runs/{}/annotations",
            job_info.owner, job_info.repo, job_info.job_id
        );
        let output = Command::new("gh")
            .args(["api", &path])
            .output()
            .context("Failed to run 'gh api' for annotations")?;
        if !output.status.success() {
            // Annotations may 404 in some cases; treat as "no annotations".
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("HTTP 404") {
                return Ok(vec![]);
            }
            anyhow::bail!("gh api annotations failed: {}", stderr.trim());
        }
        let parsed: Vec<AnnotationResponse> =
            serde_json::from_slice(&output.stdout).context("parse annotations")?;
        Ok(parsed
            .into_iter()
            .map(|a| Annotation {
                message: a.message,
                path: a.path,
                start_line: a.start_line,
                level: a.annotation_level,
            })
            .collect())
    }
}

/// Check if a URL is a GitHub Actions job URL.
pub fn is_gh_actions_url(url: &str) -> bool {
    url.contains("github.com/")
        && url.contains("/actions/runs/")
        && url.contains("/job/")
}

/// Parse a GH Actions job URL.
/// Example: https://github.com/owner/repo/actions/runs/123/job/456?pr=789
pub fn parse_gh_actions_url(url: &str) -> Option<GhActionsJobInfo> {
    let stripped = url.split('?').next()?;
    // Find "github.com/"
    let gh_idx = stripped.find("github.com/")?;
    let after_gh = &stripped[gh_idx + "github.com/".len()..];
    let parts: Vec<&str> = after_gh.split('/').collect();
    // Expected: owner, repo, "actions", "runs", run_id, "job", job_id
    if parts.len() < 7 || parts[2] != "actions" || parts[3] != "runs" || parts[5] != "job" {
        return None;
    }
    Some(GhActionsJobInfo {
        owner: parts[0].to_string(),
        repo: parts[1].to_string(),
        job_id: parts[6].parse().ok()?,
    })
}

/// Fetch failed-step logs plus annotations for a GH Actions job, returned as
/// `FailedStepLog` entries so they can be merged with CircleCI-style output.
/// The `output` field gets the log for the failed step and the `error` field
/// gets the formatted annotations (since that's the structured signal).
pub fn get_failed_step_logs(
    client: &dyn GhActionsClient,
    job_info: &GhActionsJobInfo,
) -> Result<Vec<FailedStepLog>> {
    let details = client.fetch_job_details(job_info)?;
    let failed: Vec<&Step> = details
        .steps
        .iter()
        .filter(|s| s.conclusion.as_deref() == Some("failure"))
        .collect();
    if failed.is_empty() {
        return Ok(vec![]);
    }

    let logs_by_step = client.fetch_failed_step_logs(job_info).unwrap_or_default();
    let annotations = client.fetch_annotations(job_info).unwrap_or_default();
    let annotations_text = format_annotations(&annotations);

    let logs: Vec<FailedStepLog> = failed
        .into_iter()
        .map(|step| FailedStepLog {
            job_name: details.name.clone(),
            step_name: step.name.clone(),
            output: logs_by_step.get(&step.name).cloned().unwrap_or_default(),
            error: annotations_text.clone(),
        })
        .collect();

    Ok(logs)
}

/// Parse the output of `gh run view --log-failed`. Each line is
/// `<jobname>\t<stepname>\t<timestamp> <content>`. We group by step name,
/// stripping the timestamp so the rendered output isn't swamped by repeated
/// `2026-04-22T18:41:59.5490832Z` prefixes.
fn parse_log_failed_output(text: &str) -> std::collections::HashMap<String, String> {
    let mut by_step: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for line in text.lines() {
        // strip BOM if present on first line
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        let mut parts = line.splitn(3, '\t');
        let Some(_job) = parts.next() else { continue };
        let Some(step) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        // rest = "<timestamp>Z <content>" — drop up to and including the
        // first 'Z' that's at least 20 chars in (RFC-3339 shape).
        let content = if let Some(z) = rest.find('Z') {
            if z >= 19 {
                rest[z + 1..].trim_start()
            } else {
                rest
            }
        } else {
            rest
        };
        let entry = by_step.entry(step.to_string()).or_default();
        entry.push_str(content);
        entry.push('\n');
    }
    by_step
}

fn format_annotations(annotations: &[Annotation]) -> String {
    if annotations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for a in annotations {
        let location = match (&a.path, a.start_line) {
            (p, Some(l)) if !p.is_empty() => format!("{}:{}", p, l),
            (p, _) if !p.is_empty() => p.clone(),
            _ => String::new(),
        };
        if location.is_empty() {
            out.push_str(&format!("[{}] {}\n", a.level, a.message));
        } else {
            out.push_str(&format!("[{}] {} — {}\n", a.level, location, a.message));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_actions_url() {
        let url = "https://github.com/owner/repo/actions/runs/123/job/456";
        let info = parse_gh_actions_url(url).unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
        assert_eq!(info.job_id, 456);
    }

    #[test]
    fn parses_gh_actions_url_with_query() {
        let url = "https://github.com/owner/repo/actions/runs/123/job/456?pr=789";
        let info = parse_gh_actions_url(url).unwrap();
        assert_eq!(info.job_id, 456);
    }

    #[test]
    fn is_gh_actions_url_true() {
        assert!(is_gh_actions_url(
            "https://github.com/owner/repo/actions/runs/1/job/2"
        ));
    }

    #[test]
    fn is_gh_actions_url_false() {
        assert!(!is_gh_actions_url("https://circleci.com/gh/owner/repo/123"));
        assert!(!is_gh_actions_url("https://github.com/owner/repo/pull/1"));
    }

    #[test]
    fn parse_log_failed_groups_by_step() {
        let input = "\
identity-e2e\tStart localserver\t2026-04-22T18:41:59.5490832Z first line
identity-e2e\tStart localserver\t2026-04-22T18:42:00.1234567Z second line
identity-e2e\tDifferent step\t2026-04-22T18:42:05.0000000Z other step
";
        let by = parse_log_failed_output(input);
        let start = by.get("Start localserver").unwrap();
        assert!(start.contains("first line"));
        assert!(start.contains("second line"));
        // timestamp + tabs are stripped
        assert!(!start.contains("2026-"));
        assert!(!start.contains("identity-e2e"));
        let other = by.get("Different step").unwrap();
        assert!(other.contains("other step"));
    }

    #[test]
    fn format_annotations_basic() {
        let anns = vec![
            Annotation {
                message: "Process failed".into(),
                path: ".github".into(),
                start_line: Some(1592),
                level: "failure".into(),
            },
            Annotation {
                message: "Localserver died".into(),
                path: "".into(),
                start_line: None,
                level: "failure".into(),
            },
        ];
        let s = format_annotations(&anns);
        assert!(s.contains(".github:1592"));
        assert!(s.contains("Process failed"));
        assert!(s.contains("Localserver died"));
    }

    pub struct TestGhActionsClient {
        pub job_details: Option<JobDetails>,
        pub logs_by_step: std::collections::HashMap<String, String>,
        pub annotations: Vec<Annotation>,
    }

    impl GhActionsClient for TestGhActionsClient {
        fn fetch_job_details(&self, _: &GhActionsJobInfo) -> Result<JobDetails> {
            self.job_details
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no test job details"))
        }
        fn fetch_failed_step_logs(
            &self,
            _: &GhActionsJobInfo,
        ) -> Result<std::collections::HashMap<String, String>> {
            Ok(self.logs_by_step.clone())
        }
        fn fetch_annotations(&self, _: &GhActionsJobInfo) -> Result<Vec<Annotation>> {
            Ok(self.annotations.clone())
        }
    }

    #[test]
    fn get_failed_step_logs_returns_only_failed_steps() {
        let mut logs_by_step = std::collections::HashMap::new();
        logs_by_step.insert("bad".to_string(), "fail line\n".to_string());
        let client = TestGhActionsClient {
            job_details: Some(JobDetails {
                name: "test-job".into(),
                steps: vec![
                    Step {
                        name: "setup".into(),
                        conclusion: Some("success".into()),
                        started_at: Some("2026-01-01T00:00:00Z".into()),
                        completed_at: Some("2026-01-01T00:00:01Z".into()),
                    },
                    Step {
                        name: "bad".into(),
                        conclusion: Some("failure".into()),
                        started_at: Some("2026-01-01T00:00:01Z".into()),
                        completed_at: Some("2026-01-01T00:00:05Z".into()),
                    },
                ],
            }),
            logs_by_step,
            annotations: vec![],
        };
        let info = GhActionsJobInfo {
            owner: "o".into(),
            repo: "r".into(),
            job_id: 1,
        };
        let logs = get_failed_step_logs(&client, &info).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].step_name, "bad");
        assert!(logs[0].output.contains("fail line"));
    }
}
