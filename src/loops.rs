// Machine-wide registry of in-progress `--wait-until-actionable[-or-happy]`
// runs, so other things can tell which PRs are actively being looped on right
// now: the hub's PR page header (any loop on that PR, from any session), and
// the Claude Code status line via `pr-loop statusline` (only loops started by
// that Claude Code session — see `filter_for_session`).
//
// Each waiting process drops a small JSON file named `<pid>.json` into
// `loops_dir()` and removes it when the wait ends. That removal can't be
// relied on — Claude Code kills Bash commands on timeout/interrupt, and
// `main.rs` leaves via `std::process::exit` in places — so readers also
// check that the PID is still alive and garbage-collect files whose process
// is gone. As a guard against PID reuse, a record also stops counting once
// the wait's own `--timeout` (plus slack) has passed.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Extra time past a wait's own timeout before its record is considered
/// stale regardless of PID liveness (covers slow final polls, clock skew).
const DEADLINE_SLACK_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// `--wait-until-actionable`: the normal attended loop.
    Actionable,
    /// `--wait-until-actionable-or-happy`: also exits once CI is green with
    /// nothing to address (the unattended loop, and `pr-ready`'s CI wait).
    UntilHappy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRecord {
    pub pid: u32,
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub mode: LoopMode,
    pub checkout_path: String,
    pub claude_pid: Option<u32>,
    /// `CLAUDE_CODE_SESSION_ID` of the Claude Code session that ran the
    /// wait, if any. Matches the `session_id` Claude Code passes a
    /// statusLine command on stdin.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// Unix seconds.
    pub started_at: u64,
    pub timeout_secs: u64,
}

impl LoopRecord {
    fn expired(&self, now: u64) -> bool {
        now > self
            .started_at
            .saturating_add(self.timeout_secs)
            .saturating_add(DEADLINE_SLACK_SECS)
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `$XDG_STATE_HOME/pr-loop/loops`, else `~/.local/state/pr-loop/loops`.
pub fn loops_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("pr-loop").join("loops"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/pr-loop/loops"))
}

/// True if a process with this PID exists. EPERM still means it exists
/// (just owned by someone else).
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes the record file when dropped. Callers should `drop` it explicitly
/// before any `std::process::exit`, which skips destructors.
pub struct LoopGuard {
    path: PathBuf,
}

impl Drop for LoopGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Best-effort: write `record` into `dir`. Returns `None` (and the wait just
/// proceeds untracked) if anything fails. Writes to a temp name and renames
/// so readers never see a half-written file.
pub fn register(dir: &Path, record: &LoopRecord) -> Option<LoopGuard> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{}.json", record.pid));
    let tmp = dir.join(format!(".{}.json.tmp", record.pid));
    let body = serde_json::to_vec(record).ok()?;
    std::fs::write(&tmp, body).ok()?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(LoopGuard { path })
}

/// All records in `dir` whose process is still alive and whose deadline
/// hasn't passed, sorted by (owner, repo, pr_number, pid). Files for dead or
/// expired waits are deleted along the way; unparseable files are skipped
/// (and left alone, in case they're from a newer pr-loop).
pub fn list_active(dir: &Path, now: u64, is_alive: &dyn Fn(u32) -> bool) -> Vec<LoopRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<LoopRecord>(&body) else {
            continue;
        };
        if record.expired(now) || !is_alive(record.pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        out.push(record);
    }
    out.sort_by(|a, b| {
        (&a.owner, &a.repo, a.pr_number, a.pid).cmp(&(&b.owner, &b.repo, b.pr_number, b.pid))
    });
    out
}

/// The real-world version of `list_active`.
pub fn list_active_now() -> Vec<LoopRecord> {
    match loops_dir() {
        Some(dir) => list_active(&dir, now_secs(), &pid_alive),
        None => vec![],
    }
}

/// The PID of the Claude Code process currently running `session_id`, per
/// its `<sessions_dir>/<pid>.json` state file (`~/.claude/sessions`).
pub fn claude_pid_for_session(sessions_dir: &Path, session_id: &str) -> Option<u32> {
    std::fs::read_dir(sessions_dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        if v.get("sessionId")?.as_str()? != session_id {
            return None;
        }
        v.get("pid")?.as_u64().and_then(|p| u32::try_from(p).ok())
    })
}

/// Just the loops started from one Claude Code session: matched by session
/// ID, or by the Claude Code PID now running that session (which also
/// catches a wait started before a `/clear` gave the process a new session
/// ID).
pub fn filter_for_session(
    records: &[LoopRecord],
    session_id: &str,
    claude_pid: Option<u32>,
) -> Vec<LoopRecord> {
    records
        .iter()
        .filter(|r| {
            r.claude_session_id.as_deref() == Some(session_id)
                || (claude_pid.is_some() && r.claude_pid == claude_pid)
        })
        .cloned()
        .collect()
}

/// Wrap `text` in an OSC 8 terminal hyperlink to `url`.
fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text)
}

/// How a single PR is being looped on, collapsing multiple waits on the
/// same PR. A plain `Actionable` wait wins over `UntilHappy` — the marker is
/// for "this will stop on its own once green", which isn't true if any wait
/// on the PR won't.
pub fn mode_for_pr(records: &[LoopRecord], owner: &str, repo: &str, pr_number: u64) -> Option<LoopMode> {
    let mut modes = records
        .iter()
        .filter(|r| r.owner == owner && r.repo == repo && r.pr_number == pr_number)
        .map(|r| r.mode);
    let first = modes.next()?;
    Some(if first == LoopMode::Actionable || modes.any(|m| m == LoopMode::Actionable) {
        LoopMode::Actionable
    } else {
        LoopMode::UntilHappy
    })
}

/// The one-line Claude Code status line text, or `None` when nothing is
/// looping (so the status line stays empty). PR refs are bare `#N` unless
/// the active loops span more than one repo; until-happy loops get a
/// trailing marker, plain ones don't. With `link`, each PR ref is an OSC 8
/// hyperlink to `link(owner, repo, pr_number)`.
pub fn format_statusline(
    records: &[LoopRecord],
    link: Option<&dyn Fn(&str, &str, u64) -> String>,
) -> Option<String> {
    let mut prs: Vec<(&str, &str, u64)> = records
        .iter()
        .map(|r| (r.owner.as_str(), r.repo.as_str(), r.pr_number))
        .collect();
    prs.dedup();
    if prs.is_empty() {
        return None;
    }
    let multi_repo = prs.iter().any(|(o, r, _)| (*o, *r) != (prs[0].0, prs[0].1));
    let refs: Vec<String> = prs
        .iter()
        .map(|&(owner, repo, n)| {
            let text = if multi_repo { format!("{}#{}", repo, n) } else { format!("#{}", n) };
            let base = match link {
                Some(link) => osc8(&link(owner, repo, n), &text),
                None => text,
            };
            match mode_for_pr(records, owner, repo, n) {
                Some(LoopMode::UntilHappy) => format!("{} (until happy)", base),
                _ => base,
            }
        })
        .collect();
    Some(format!(
        "pr-looping on {} PR{}: {}",
        prs.len(),
        if prs.len() == 1 { "" } else { "s" },
        refs.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, repo: &str, n: u64, mode: LoopMode) -> LoopRecord {
        LoopRecord {
            pid,
            owner: "o".into(),
            repo: repo.into(),
            pr_number: n,
            mode,
            checkout_path: "/x".into(),
            claude_pid: None,
            claude_session_id: None,
            started_at: 1000,
            timeout_secs: 1800,
        }
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pr-loop-loops-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn statusline_empty_is_none() {
        assert_eq!(format_statusline(&[], None), None);
    }

    #[test]
    fn statusline_single_plain() {
        let r = [rec(1, "r", 1234, LoopMode::Actionable)];
        assert_eq!(format_statusline(&r, None).unwrap(), "pr-looping on 1 PR: #1234");
    }

    #[test]
    fn statusline_marks_only_until_happy() {
        let r = [
            rec(1, "r", 1234, LoopMode::Actionable),
            rec(2, "r", 4567, LoopMode::UntilHappy),
        ];
        assert_eq!(
            format_statusline(&r, None).unwrap(),
            "pr-looping on 2 PRs: #1234, #4567 (until happy)"
        );
    }

    #[test]
    fn statusline_dedupes_same_pr_and_plain_wins() {
        let r = [
            rec(1, "r", 7, LoopMode::UntilHappy),
            rec(2, "r", 7, LoopMode::Actionable),
        ];
        assert_eq!(format_statusline(&r, None).unwrap(), "pr-looping on 1 PR: #7");
    }

    #[test]
    fn statusline_qualifies_when_multi_repo() {
        let r = [rec(1, "a", 1, LoopMode::Actionable), rec(2, "b", 2, LoopMode::Actionable)];
        assert_eq!(format_statusline(&r, None).unwrap(), "pr-looping on 2 PRs: a#1, b#2");
    }

    #[test]
    fn statusline_links_each_pr_ref() {
        let r = [rec(1, "r", 7, LoopMode::UntilHappy)];
        let link = |o: &str, repo: &str, n: u64| format!("http://hub/pr/{o}/{repo}/{n}/");
        assert_eq!(
            format_statusline(&r, Some(&link)).unwrap(),
            "pr-looping on 1 PR: \x1b]8;;http://hub/pr/o/r/7/\x1b\\#7\x1b]8;;\x1b\\ (until happy)"
        );
    }

    #[test]
    fn filter_for_session_matches_session_id_or_claude_pid() {
        let mut by_session = rec(1, "r", 1, LoopMode::Actionable);
        by_session.claude_session_id = Some("s1".into());
        let mut by_pid = rec(2, "r", 2, LoopMode::Actionable);
        by_pid.claude_pid = Some(500);
        by_pid.claude_session_id = Some("old-session".into());
        let mut other = rec(3, "r", 3, LoopMode::Actionable);
        other.claude_pid = Some(600);
        other.claude_session_id = Some("s2".into());
        let untagged = rec(4, "r", 4, LoopMode::Actionable);
        let all = [by_session.clone(), by_pid.clone(), other, untagged];

        assert_eq!(filter_for_session(&all, "s1", Some(500)), vec![by_session.clone(), by_pid]);
        assert_eq!(filter_for_session(&all, "s1", None), vec![by_session]);
        assert!(filter_for_session(&all, "nope", None).is_empty());
    }

    #[test]
    fn claude_pid_for_session_scans_session_files() {
        let dir = tempdir();
        std::fs::write(dir.join("100.json"), r#"{"pid":100,"sessionId":"a"}"#).unwrap();
        std::fs::write(dir.join("200.json"), r#"{"pid":200,"sessionId":"b","cwd":"/x"}"#).unwrap();
        std::fs::write(dir.join("300.json"), b"garbage").unwrap();
        std::fs::write(dir.join("200.abc.key"), b"{}").unwrap();
        assert_eq!(claude_pid_for_session(&dir, "b"), Some(200));
        assert_eq!(claude_pid_for_session(&dir, "zzz"), None);
        assert_eq!(claude_pid_for_session(Path::new("/nonexistent"), "b"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_without_session_id_still_parses() {
        let old = r#"{"pid":1,"owner":"o","repo":"r","pr_number":1,"mode":"actionable","checkout_path":"/x","claude_pid":null,"started_at":1,"timeout_secs":1}"#;
        let r: LoopRecord = serde_json::from_str(old).unwrap();
        assert_eq!(r.claude_session_id, None);
    }

    #[test]
    fn register_then_list_then_guard_drop_removes() {
        let dir = tempdir();
        let r = rec(42, "r", 9, LoopMode::UntilHappy);
        let guard = register(&dir, &r).unwrap();
        let got = list_active(&dir, 1001, &|_| true);
        assert_eq!(got, vec![r]);
        drop(guard);
        assert!(list_active(&dir, 1001, &|_| true).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_gcs_dead_and_expired_and_skips_junk() {
        let dir = tempdir();
        let alive = rec(1, "r", 1, LoopMode::Actionable);
        let dead = rec(2, "r", 2, LoopMode::Actionable);
        let mut expired = rec(3, "r", 3, LoopMode::Actionable);
        expired.started_at = 0;
        expired.timeout_secs = 10;
        for r in [&alive, &dead, &expired] {
            std::mem::forget(register(&dir, r).unwrap());
        }
        std::fs::write(dir.join("junk.json"), b"not json").unwrap();
        std::fs::write(dir.join(".5.json.tmp"), b"{}").unwrap();

        let got = list_active(&dir, 1001, &|pid| pid != 2);
        assert_eq!(got, vec![alive]);
        assert!(!dir.join("2.json").exists());
        assert!(!dir.join("3.json").exists());
        assert!(dir.join("junk.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_missing_dir_is_empty() {
        assert!(list_active(Path::new("/nonexistent/pr-loop/loops"), 0, &|_| true).is_empty());
    }

    #[test]
    fn mode_for_pr_filters_by_pr() {
        let r = [rec(1, "r", 1, LoopMode::UntilHappy), rec(2, "r", 2, LoopMode::Actionable)];
        assert_eq!(mode_for_pr(&r, "o", "r", 1), Some(LoopMode::UntilHappy));
        assert_eq!(mode_for_pr(&r, "o", "r", 3), None);
    }

    #[test]
    fn pid_alive_self_and_bogus() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }
}
