// Machine-wide registry of in-progress `--wait-until-actionable[-or-happy]`
// runs, so other things (the Claude Code status line via `pr-loop
// statusline`, the hub's PR page header) can tell which PRs are actively
// being looped on right now.
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
/// trailing marker, plain ones don't.
pub fn format_statusline(records: &[LoopRecord]) -> Option<String> {
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
            let base = if multi_repo { format!("{}#{}", repo, n) } else { format!("#{}", n) };
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
        assert_eq!(format_statusline(&[]), None);
    }

    #[test]
    fn statusline_single_plain() {
        let r = [rec(1, "r", 1234, LoopMode::Actionable)];
        assert_eq!(format_statusline(&r).unwrap(), "pr-looping on 1 PR: #1234");
    }

    #[test]
    fn statusline_marks_only_until_happy() {
        let r = [
            rec(1, "r", 1234, LoopMode::Actionable),
            rec(2, "r", 4567, LoopMode::UntilHappy),
        ];
        assert_eq!(
            format_statusline(&r).unwrap(),
            "pr-looping on 2 PRs: #1234, #4567 (until happy)"
        );
    }

    #[test]
    fn statusline_dedupes_same_pr_and_plain_wins() {
        let r = [
            rec(1, "r", 7, LoopMode::UntilHappy),
            rec(2, "r", 7, LoopMode::Actionable),
        ];
        assert_eq!(format_statusline(&r).unwrap(), "pr-looping on 1 PR: #7");
    }

    #[test]
    fn statusline_qualifies_when_multi_repo() {
        let r = [rec(1, "a", 1, LoopMode::Actionable), rec(2, "b", 2, LoopMode::Actionable)];
        assert_eq!(format_statusline(&r).unwrap(), "pr-looping on 2 PRs: a#1, b#2");
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
