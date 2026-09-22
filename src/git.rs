// Git operations for detecting push/commit times, and rewording commits.
// Uses git CLI to get commit timestamps.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Trait for git operations, allowing test implementations.
pub trait GitClient {
    /// Get the timestamp of the last commit on the current branch.
    fn get_last_commit_time(&self) -> Result<SystemTime>;

    /// Get the hash of HEAD in `dir`. Used to detect local ref changes
    /// cheaply. Takes an explicit directory (rather than relying on the
    /// process's cwd) because the hub polls this on behalf of PRs it isn't
    /// itself checked out in — `dir` is whatever checkout path the last
    /// `pr-loop` invocation for that PR reported.
    fn get_head_hash_at(&self, dir: &Path) -> Result<String>;

    /// Reword `sha`'s commit message to `new_message`, in the repo at the
    /// current working directory. Implemented as a non-interactive
    /// `git rebase -i`: every commit after `sha` gets replayed unchanged
    /// (same tree, new parent chain), so this can't produce merge conflicts
    /// — the only failure modes are `sha` not being an ancestor of HEAD, or
    /// a rebase already in progress. Leaves the new history checked out
    /// locally; the caller is responsible for pushing (with
    /// `--force-with-lease`, since this rewrites history other clones may
    /// have).
    fn reword_commit(&self, sha: &str, new_message: &str) -> Result<()>;
}

/// Real git client that uses the `git` CLI.
pub struct RealGitClient;

impl GitClient for RealGitClient {
    fn get_last_commit_time(&self) -> Result<SystemTime> {
        get_last_commit_time_from_git()
    }

    fn get_head_hash_at(&self, dir: &Path) -> Result<String> {
        get_head_hash_from_git_at(dir)
    }

    fn reword_commit(&self, sha: &str, new_message: &str) -> Result<()> {
        reword_commit_with_git(sha, new_message)
    }
}

/// Reword `sha`'s commit message using a scripted, non-interactive
/// `git rebase -i` in the current working directory. Every commit is
/// replayed with its original tree unchanged — only the target commit's
/// message and everyone's SHAs change — so this can never produce a merge
/// conflict.
fn reword_commit_with_git(sha: &str, new_message: &str) -> Result<()> {
    let full_sha = run_git(&["rev-parse", sha]).context("Could not resolve commit")?;

    run_git(&["merge-base", "--is-ancestor", &full_sha, "HEAD"])
        .map_err(|_| anyhow::anyhow!("Commit {} is not an ancestor of HEAD", full_sha))?;

    if run_git(&["rev-parse", "--verify", &format!("{}^", full_sha)]).is_err() {
        anyhow::bail!("Cannot reword {}: it has no parent commit", full_sha);
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "pr-loop-reword-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp_dir).context("Failed to create temp dir for rebase scripts")?;
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    };

    let message_path = tmp_dir.join("message.txt");
    let seq_editor_path = tmp_dir.join("seq_editor.sh");
    let msg_editor_path = tmp_dir.join("msg_editor.sh");

    if let Err(e) = write_reword_scripts(&message_path, &seq_editor_path, &msg_editor_path, new_message) {
        cleanup();
        return Err(e);
    }

    let output = Command::new("git")
        .args(["rebase", "-i", &format!("{}^", full_sha)])
        .env("GIT_SEQUENCE_EDITOR", &seq_editor_path)
        .env("GIT_EDITOR", &msg_editor_path)
        .env("PR_LOOP_REWORD_SHA", &full_sha)
        .env("PR_LOOP_REWORD_MSG_FILE", &message_path)
        .output()
        .context("Failed to run 'git rebase -i'");

    cleanup();

    let output = output?;
    if !output.status.success() {
        // Best-effort: leave the repo usable rather than mid-rebase.
        let _ = Command::new("git").args(["rebase", "--abort"]).output();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("git rebase -i failed: {}{}", stderr.trim(), stdout.trim());
    }

    Ok(())
}

/// Write the two shell scripts `git rebase -i` will invoke non-interactively:
/// one to flip the target commit's todo line from `pick` to `reword`
/// (matched by resolving each todo line's abbreviated SHA back to full, so
/// it's independent of `core.abbrev`), and one to supply the new commit
/// message when git asks for it.
fn write_reword_scripts(
    message_path: &Path,
    seq_editor_path: &Path,
    msg_editor_path: &Path,
    new_message: &str,
) -> Result<()> {
    std::fs::write(message_path, new_message).context("Failed to write new commit message")?;

    let seq_script = r#"#!/bin/sh
set -e
tmp="$1.prloop"
: > "$tmp"
while IFS= read -r line; do
  case "$line" in
    pick\ *)
      rest="${line#pick }"
      short="${rest%% *}"
      full="$(git rev-parse "$short" 2>/dev/null || true)"
      if [ "$full" = "$PR_LOOP_REWORD_SHA" ]; then
        echo "reword $rest" >> "$tmp"
      else
        echo "$line" >> "$tmp"
      fi
      ;;
    *)
      echo "$line" >> "$tmp"
      ;;
  esac
done < "$1"
mv "$tmp" "$1"
"#;
    let msg_script = "#!/bin/sh\ncp \"$PR_LOOP_REWORD_MSG_FILE\" \"$1\"\n";

    write_executable(seq_editor_path, seq_script)?;
    write_executable(msg_editor_path, msg_script)?;
    Ok(())
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).context("Failed to write rebase helper script")?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Run a git command in the current directory, returning trimmed stdout.
fn run_git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("Failed to run 'git {}'", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_head_hash_from_git_at(dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("Failed to run 'git rev-parse'")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get HEAD hash: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Get the Unix timestamp of the last commit using `git log`.
fn get_last_commit_time_from_git() -> Result<SystemTime> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct"])
        .output()
        .context("Failed to run 'git log'. Is this a git repository?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get last commit time: {}", stderr.trim());
    }

    let timestamp_str = String::from_utf8_lossy(&output.stdout);
    let timestamp: u64 = timestamp_str
        .trim()
        .parse()
        .context("Failed to parse commit timestamp")?;

    Ok(UNIX_EPOCH + Duration::from_secs(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Test git client that returns a fixed timestamp.
    pub struct TestGitClient {
        pub last_commit_time: SystemTime,
        pub head_hash: String,
    }

    impl GitClient for TestGitClient {
        fn get_last_commit_time(&self) -> Result<SystemTime> {
            Ok(self.last_commit_time)
        }
        fn get_head_hash_at(&self, _dir: &std::path::Path) -> Result<String> {
            Ok(self.head_hash.clone())
        }
        fn reword_commit(&self, _sha: &str, _new_message: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_git_client_returns_time() {
        let now = SystemTime::now();
        let client = TestGitClient {
            last_commit_time: now,
            head_hash: "abc123".to_string(),
        };
        let result = client.get_last_commit_time().unwrap();
        assert_eq!(result, now);
    }

    /// A throwaway git repo in a temp dir, with the process's cwd pointed at
    /// it for the duration of the guard. `reword_commit` operates on cwd (no
    /// `-C` support, unlike the other GitClient methods), so tests need to
    /// actually chdir — serialized via #[serial] since cwd is process-global.
    struct TempRepo {
        _dir: tempfile_dir::TempDir,
        original_cwd: std::path::PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let dir = tempfile_dir::TempDir::new();
            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir.path()).unwrap();
            run_git(&["init", "-q", "-b", "main"]).unwrap();
            run_git(&["config", "user.email", "test@example.com"]).unwrap();
            run_git(&["config", "user.name", "Test"]).unwrap();
            Self { _dir: dir, original_cwd }
        }

        fn commit(&self, filename: &str, headline: &str) -> String {
            std::fs::write(filename, headline).unwrap();
            run_git(&["add", filename]).unwrap();
            run_git(&["commit", "-q", "-m", headline]).unwrap();
            run_git(&["rev-parse", "HEAD"]).unwrap()
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
        }
    }

    /// Minimal owned-tempdir helper so this test module doesn't need the
    /// `tempfile` crate as a real dependency just for a handful of tests.
    mod tempfile_dir {
        pub struct TempDir(std::path::PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "pr-loop-git-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    #[serial]
    fn reword_commit_changes_message_and_preserves_tree() {
        let repo = TempRepo::new();
        let _root = repo.commit("a.txt", "Root commit");
        let second = repo.commit("b.txt", "Second commit");
        let third = repo.commit("c.txt", "Third commit");

        let client = RealGitClient;
        client.reword_commit(&second, "Reworded second commit").unwrap();

        let log = run_git(&["log", "--format=%H %s"]).unwrap();
        assert!(log.contains("Reworded second commit"));
        assert!(!log.contains("Second commit"));

        // Other commits were replayed, not lost or altered in content.
        assert!(log.contains("Root commit"));
        assert!(log.contains("Third commit"));
        assert!(std::path::Path::new("a.txt").exists());
        assert!(std::path::Path::new("b.txt").exists());
        assert!(std::path::Path::new("c.txt").exists());

        // The tip's tree is byte-identical to before the rebase even though
        // its SHA necessarily changed (its parent chain changed) — a pure
        // reword can't alter any commit's content.
        let diff = run_git(&["diff", &third, "HEAD"]).unwrap();
        assert!(diff.is_empty(), "tree changed across reword: {}", diff);
    }

    #[test]
    #[serial]
    fn reword_commit_on_tip() {
        let repo = TempRepo::new();
        let _first = repo.commit("a.txt", "First commit");
        let second = repo.commit("b.txt", "Second commit");

        let client = RealGitClient;
        client.reword_commit(&second, "New tip message").unwrap();

        let log = run_git(&["log", "-1", "--format=%s"]).unwrap();
        assert_eq!(log, "New tip message");
    }

    #[test]
    #[serial]
    fn reword_commit_multiline_message() {
        let repo = TempRepo::new();
        let _root = repo.commit("a.txt", "Root commit");
        let second = repo.commit("b.txt", "Second commit");

        let client = RealGitClient;
        client
            .reword_commit(&second, "New headline\n\nAnd a body paragraph.")
            .unwrap();

        let headline = run_git(&["log", "-1", "--format=%s"]).unwrap();
        let body = run_git(&["log", "-1", "--format=%b"]).unwrap();
        assert_eq!(headline, "New headline");
        assert_eq!(body, "And a body paragraph.");
    }

    #[test]
    #[serial]
    fn reword_commit_rejects_root_commit() {
        let repo = TempRepo::new();
        let root = repo.commit("a.txt", "Root commit");

        let client = RealGitClient;
        let err = client.reword_commit(&root, "New message").unwrap_err();
        assert!(err.to_string().contains("no parent"));
    }

    #[test]
    #[serial]
    fn reword_commit_rejects_unknown_sha() {
        let _repo = TempRepo::new();
        let client = RealGitClient;
        let err = client
            .reword_commit("0000000000000000000000000000000000dead", "msg")
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
