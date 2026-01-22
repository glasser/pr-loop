// Credential handling for GitHub and CircleCI APIs.
// Validates gh CLI authentication and reads CircleCI token from environment.

use anyhow::{Context, Result};
use std::process::Command;

/// Credentials needed to interact with CircleCI.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub circleci_token: Option<String>,
}

/// Trait for obtaining credentials, allowing test implementations.
pub trait CredentialProvider {
    fn get_credentials(&self) -> Result<Credentials>;
}

/// Real credential provider that validates gh auth and reads CircleCI token from env.
pub struct RealCredentialProvider;

impl CredentialProvider for RealCredentialProvider {
    fn get_credentials(&self) -> Result<Credentials> {
        // Validate gh CLI is authenticated (we use gh CLI for GitHub API calls)
        check_gh_auth()?;
        let circleci_token = get_circleci_token();

        Ok(Credentials { circleci_token })
    }
}

/// Verify gh CLI is authenticated by running `gh auth token`.
fn check_gh_auth() -> Result<()> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("Failed to run 'gh auth token'. Is the GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "GitHub CLI not authenticated: {}. Run 'gh auth login' first.",
            stderr.trim()
        );
    }

    Ok(())
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
        pub circleci_token: Option<String>,
    }

    impl CredentialProvider for TestCredentialProvider {
        fn get_credentials(&self) -> Result<Credentials> {
            Ok(Credentials {
                circleci_token: self.circleci_token.clone(),
            })
        }
    }

    #[test]
    fn test_provider_returns_credentials() {
        let provider = TestCredentialProvider {
            circleci_token: Some("cci_test_token".to_string()),
        };

        let creds = provider.get_credentials().unwrap();
        assert_eq!(creds.circleci_token, Some("cci_test_token".to_string()));
    }

    #[test]
    fn test_provider_without_circleci() {
        let provider = TestCredentialProvider {
            circleci_token: None,
        };

        let creds = provider.get_credentials().unwrap();
        assert!(creds.circleci_token.is_none());
    }
}
