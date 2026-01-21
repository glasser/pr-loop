// Credential handling for GitHub and CircleCI APIs.
// GitHub token obtained via `gh auth token`, CircleCI token from environment.

use anyhow::{Context, Result};
use std::process::Command;

/// Credentials needed to interact with GitHub and CircleCI.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub github_token: String,
    pub circleci_token: Option<String>,
}

/// Trait for obtaining credentials, allowing test implementations.
pub trait CredentialProvider {
    fn get_credentials(&self) -> Result<Credentials>;
}

/// Real credential provider that reads from environment and runs `gh auth token`.
pub struct RealCredentialProvider;

impl CredentialProvider for RealCredentialProvider {
    fn get_credentials(&self) -> Result<Credentials> {
        let github_token = get_github_token()?;
        let circleci_token = get_circleci_token();

        Ok(Credentials {
            github_token,
            circleci_token,
        })
    }
}

/// Get GitHub token by running `gh auth token`.
fn get_github_token() -> Result<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("Failed to run 'gh auth token'. Is the GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to get GitHub token from 'gh auth token': {}",
            stderr.trim()
        );
    }

    let token = String::from_utf8(output.stdout)
        .context("GitHub token is not valid UTF-8")?
        .trim()
        .to_string();

    if token.is_empty() {
        anyhow::bail!("GitHub token is empty. Run 'gh auth login' to authenticate.");
    }

    Ok(token)
}

/// Get CircleCI token from CIRCLECI_TOKEN environment variable.
fn get_circleci_token() -> Option<String> {
    std::env::var("CIRCLECI_TOKEN").ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test credential provider that returns fixed credentials.
    pub struct TestCredentialProvider {
        pub github_token: String,
        pub circleci_token: Option<String>,
    }

    impl CredentialProvider for TestCredentialProvider {
        fn get_credentials(&self) -> Result<Credentials> {
            Ok(Credentials {
                github_token: self.github_token.clone(),
                circleci_token: self.circleci_token.clone(),
            })
        }
    }

    #[test]
    fn test_provider_returns_credentials() {
        let provider = TestCredentialProvider {
            github_token: "gh_test_token".to_string(),
            circleci_token: Some("cci_test_token".to_string()),
        };

        let creds = provider.get_credentials().unwrap();
        assert_eq!(creds.github_token, "gh_test_token");
        assert_eq!(creds.circleci_token, Some("cci_test_token".to_string()));
    }

    #[test]
    fn test_provider_without_circleci() {
        let provider = TestCredentialProvider {
            github_token: "gh_test_token".to_string(),
            circleci_token: None,
        };

        let creds = provider.get_credentials().unwrap();
        assert_eq!(creds.github_token, "gh_test_token");
        assert!(creds.circleci_token.is_none());
    }
}
