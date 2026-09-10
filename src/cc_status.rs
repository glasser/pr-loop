// Peek at the Claude Code session transcript for the current working directory,
// so `pr-loop web` can show what CC is doing right now.
//
// Claude Code writes a JSONL transcript per session at
// ~/.claude/projects/<encoded-cwd>/<session-id>.jsonl where <encoded-cwd> is
// the absolute cwd with '/' replaced by '-' (so the path always starts with
// a leading '-'). Each line is a message event: `type` ("user" | "assistant"),
// `timestamp`, and `message.content` with `text` / `tool_use` / `tool_result`
// blocks.
//
// This module is best-effort — if the transcript is missing, the format
// shifts, or anything fails to parse, we return `None` and the UI simply
// doesn't render the status strip.

use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAX_TAIL_BYTES: u64 = 256 * 1024;
const PREVIEW_MAX: usize = 80;

#[derive(Debug, Clone, Serialize)]
pub struct InFlightTool {
    pub name: String,
    pub started_at: String,
    pub preview: Option<String>,
    pub is_sidechain: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CcActivity {
    /// At least one tool is currently running.
    Running,
    /// Assistant is generating a response (last event is a user message with
    /// no assistant event following).
    Thinking,
    /// Last event was an assistant message with no pending tool — CC is done
    /// with its turn and waiting for a new user prompt.
    Idle,
    /// CC is blocked on a permission prompt (or other user-approval gate).
    /// Detected from the session JSON, not the transcript.
    Waiting,
}

#[derive(Debug, Clone, Serialize)]
pub struct CcStatus {
    pub activity: CcActivity,
    pub in_flight: Vec<InFlightTool>,
    /// Most recent tool_use whose matching tool_result has been written —
    /// useful for showing "last: Edit foo.rs" when CC is thinking or idle.
    pub last_completed_tool: Option<InFlightTool>,
    pub last_activity_at: Option<String>,
    pub last_assistant_text: Option<String>,
    /// When `activity == Waiting`, the `waitingFor` string from the session
    /// JSON (e.g., "approve Edit"). None otherwise.
    pub waiting_for: Option<String>,
}

pub fn read_cc_status(cwd: &Path) -> Option<CcStatus> {
    diagnose_cc_status(cwd).status
}

/// Read `~/.claude/sessions/<pid>.json` directly by PID — the same file
/// `pick_live_session_for_cwd` scans by matching `cwd`, but looked up
/// directly instead of guessed at. Returns its path and parsed contents
/// (cwd, sessionId, status/waitingFor) — `None` if the file doesn't exist or
/// fails to parse (e.g. the process already exited and nothing's cleaned
/// the file up yet).
///
/// A caller that knows which Claude Code process it's ultimately running
/// under (e.g. via the `CLAUDE_PID` env var Claude Code sets on every
/// subprocess it spawns, including the Bash tool's shell) should prefer
/// this over its own process cwd: unlike a directory the caller reports,
/// a PID can't be stale from a `cd` the caller's shell made before invoking
/// it, and looking a specific PID's file up directly — rather than scanning
/// all session files for one whose `cwd` matches some directory — can't be
/// confused by multiple Claude Code sessions sharing a directory either.
fn read_live_session_by_pid(pid: u32) -> Option<(PathBuf, ParsedSession)> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join(".claude/sessions")
        .join(format!("{}.json", pid));
    let content = std::fs::read_to_string(&path).ok()?;
    let parsed = parse_session_full(&content)?;
    Some((path, parsed))
}

/// Like `read_cc_status`, but for a caller that already knows exactly which
/// Claude Code process it's asking about (by PID, e.g. from `CLAUDE_PID`).
/// Reads that PID's session file once and uses it for *both* the cwd and
/// the status/waitingFor overlay — where
/// `diagnose_cc_status` has to separately call `pick_live_session_for_cwd`,
/// rescanning every session file and matching by cwd, that's redundant work
/// here since we already know exactly which file we want, and it's a second
/// place multiple Claude Code sessions sharing a directory could pick the
/// wrong one.
pub fn read_cc_status_for_pid(pid: u32) -> Option<CcStatus> {
    let (session_file, parsed) = read_live_session_by_pid(pid)?;
    let cwd = PathBuf::from(parsed.cwd?);
    diagnose_cc_status_impl(&cwd, Some((session_file, parsed.state))).status
}

/// Detailed breakdown of what `read_cc_status` saw while computing the
/// returned status. Intended for the `cc-status` debug subcommand.
pub struct CcStatusDiagnostics {
    pub cwd: PathBuf,
    pub project_dir: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
    pub session_status_raw: Option<String>,
    pub session_waiting_for: Option<String>,
    pub status: Option<CcStatus>,
}

pub fn diagnose_cc_status(cwd: &Path) -> CcStatusDiagnostics {
    // Status/waiting_for come from the most attention-worthy live session
    // file whose cwd matches — we don't know a specific PID here (this path
    // is for the cwd-only fallback and the `cc-status` debug subcommand), so
    // scan for one. `read_cc_status_for_pid` skips this scan entirely when
    // the caller already knows which PID it means.
    let live_session = pick_live_session_for_cwd(cwd).map(|(path, state, _sid)| (path, state));
    diagnose_cc_status_impl(cwd, live_session)
}

/// Shared body of `diagnose_cc_status` and `read_cc_status_for_pid`: given a
/// resolved `cwd` and (if the caller already found one) the live session
/// file to overlay status/waitingFor from, computes the full diagnostics.
fn diagnose_cc_status_impl(
    cwd: &Path,
    live_session: Option<(PathBuf, SessionState)>,
) -> CcStatusDiagnostics {
    let mut diag = CcStatusDiagnostics {
        cwd: cwd.to_path_buf(),
        project_dir: None,
        transcript: None,
        session_id: None,
        session_file: None,
        session_status_raw: None,
        session_waiting_for: None,
        status: None,
    };
    let Some(dir) = session_dir_for_cwd(cwd) else { return diag };
    diag.project_dir = Some(dir.clone());

    // Content comes from the most-recently-written transcript in the
    // project dir. The sessionId in the live session JSON can't be trusted
    // to point at the active transcript — `claude -c` rotates the sessionId
    // on continue without updating the session file's sessionId field.
    let Some(file) = newest_jsonl(&dir) else { return diag };
    diag.transcript = Some(file.clone());
    diag.session_id = file.file_stem().and_then(|s| s.to_str()).map(str::to_string);
    let Ok(content) = read_tail(&file, MAX_TAIL_BYTES) else { return diag };
    let mut status = parse_events(&content);

    if let Some((path, state)) = live_session {
        diag.session_file = Some(path);
        diag.session_status_raw = state.status.clone();
        diag.session_waiting_for = state.waiting_for.clone();
        apply_session_state(&mut status, &state);
    }

    diag.status = Some(status);
    diag
}

/// Scan `~/.claude/sessions/*.json`, keep entries whose `cwd` matches `cwd`
/// (after canonicalization), and pick the most-recently-updated one.
/// Returns `(session_file_path, session_state, sessionId)`.
///
/// Sorting purely by `updatedAt` fails safe against orphaned session files:
/// a stale "waiting" file from a crashed CC can't outrank a currently-live
/// session. The cost is that if two live CCs share a cwd and the waiting
/// one's heartbeat happens to lag the busy one, we'd miss the waiting
/// signal — rare enough to ignore in practice.
fn pick_live_session_for_cwd(cwd: &Path) -> Option<(PathBuf, SessionState, String)> {
    let target = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf());
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".claude/sessions");
    let entries = std::fs::read_dir(&dir).ok()?;

    let mut candidates: Vec<(i64, PathBuf, SessionState, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let Some(parsed) = parse_session_full(&content) else { continue };
        let Some(session_cwd) = parsed.cwd.as_deref() else { continue };

        // Compare cwds after canonicalization on both sides so /private/var
        // vs /var symlinks don't cause false mismatches.
        let session_path = std::path::PathBuf::from(session_cwd);
        let canon = session_path.canonicalize().unwrap_or(session_path);
        if canon != target {
            continue;
        }

        candidates.push((parsed.updated_at, path, parsed.state, parsed.session_id));
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, path, state, sid)| (path, state, sid))
}

/// Richer session-file parse used by `pick_live_session_for_cwd`. Returns
/// everything the picker needs to rank candidates.
#[derive(Debug, Clone)]
struct ParsedSession {
    session_id: String,
    cwd: Option<String>,
    updated_at: i64,
    state: SessionState,
}

fn parse_session_full(content: &str) -> Option<ParsedSession> {
    let v: Value = serde_json::from_str(content).ok()?;
    let sid = v.get("sessionId").and_then(|s| s.as_str())?.to_string();
    let cwd = v.get("cwd").and_then(|s| s.as_str()).map(str::to_string);
    let updated_at = v.get("updatedAt").and_then(|s| s.as_i64()).unwrap_or(0);
    let state = SessionState {
        status: v.get("status").and_then(|s| s.as_str()).map(str::to_string),
        waiting_for: v
            .get("waitingFor")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    };
    Some(ParsedSession {
        session_id: sid,
        cwd,
        updated_at,
        state,
    })
}

/// Parsed fields from `~/.claude/sessions/<pid>.json`.
#[derive(Debug, Clone, Default)]
struct SessionState {
    status: Option<String>,
    waiting_for: Option<String>,
}

fn apply_session_state(status: &mut CcStatus, session: &SessionState) {
    if session.status.as_deref() == Some("waiting") {
        status.activity = CcActivity::Waiting;
        status.waiting_for = session.waiting_for.clone();
    }
}

fn session_dir_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let abs = cwd.canonicalize().ok().unwrap_or_else(|| cwd.to_path_buf());
    // Claude Code encodes the CWD for its project directory by replacing
    // anything that isn't alphanumeric, `-`, or `_` with `-`. So
    // `/Users/foo/.config` → `-Users-foo--config`,
    // `/Users/foo/monorepo.git/wt` → `-Users-foo-monorepo-git-wt`.
    let encoded: String = abs
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/projects").join(encoded))
}

fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let start = size.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Drop any partial leading line.
        if let Some(nl) = s.find('\n') {
            return Ok(s[nl + 1..].to_string());
        }
    }
    Ok(s)
}

fn parse_events(content: &str) -> CcStatus {
    // Parse every line into a Value (skipping junk).
    let events: Vec<Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let last_activity_at = events
        .iter()
        .rev()
        .find_map(|v| v.get("timestamp").and_then(|t| t.as_str()).map(str::to_string));

    let last_assistant_text = find_last_assistant_text(&events);

    // Find the most recent assistant event that contains tool_use blocks.
    // Any earlier turn's tool_uses must have been resolved before the next
    // assistant turn could begin, so they're not actually in-flight — just
    // orphaned in a sliding 256KB window.
    let mut in_flight: Vec<InFlightTool> = Vec::new();
    if let Some(idx) = events.iter().rposition(|ev| {
        ev.get("type").and_then(|t| t.as_str()) == Some("assistant")
            && ev
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                })
                .unwrap_or(false)
    }) {
        let turn = &events[idx];
        let timestamp = turn
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let is_sidechain = turn
            .get("isSidechain")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        let Some(blocks) = turn
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return CcStatus {
                activity: CcActivity::Idle,
                in_flight: vec![],
                last_completed_tool: None,
                last_activity_at,
                last_assistant_text,
                waiting_for: None,
            };
        };

        // Collect the turn's tool_use ids and metadata.
        let turn_tool_uses: Vec<(String, InFlightTool)> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .filter_map(|b| {
                let id = b.get("id").and_then(|i| i.as_str())?.to_string();
                let name = b
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string();
                let preview = preview_for_tool(&name, b.get("input"));
                Some((
                    id,
                    InFlightTool {
                        name,
                        started_at: timestamp.clone(),
                        preview,
                        is_sidechain,
                    },
                ))
            })
            .collect();

        // Any tool_result in later events matches a tool_use_id.
        let matched: std::collections::HashSet<String> = events[idx + 1..]
            .iter()
            .filter_map(|ev| {
                ev.get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
            })
            .flat_map(|arr| arr.iter())
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    b.get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect();

        for (id, tool) in turn_tool_uses {
            if !matched.contains(&id) {
                in_flight.push(tool);
            }
        }
    }

    let activity = if !in_flight.is_empty() {
        CcActivity::Running
    } else {
        // Look at the last event's top-level type. If it's "user" (either a
        // tool_result batch or a user text message), CC is generating its
        // next response. If it's "assistant", the turn is complete.
        match events
            .last()
            .and_then(|ev| ev.get("type").and_then(|t| t.as_str()))
        {
            Some("user") => CcActivity::Thinking,
            _ => CcActivity::Idle,
        }
    };

    let last_completed_tool = find_last_completed_tool(&events);

    CcStatus {
        activity,
        in_flight,
        last_completed_tool,
        last_activity_at,
        last_assistant_text,
        waiting_for: None,
    }
}

/// Walk events backward to find the most recent tool_use whose matching
/// tool_result has been written. Returns None if no completed tool is in
/// the window.
fn find_last_completed_tool(events: &[Value]) -> Option<InFlightTool> {
    // Collect all tool_result ids (these are "completed" tool_use ids).
    let completed_ids: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|ev| ev.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()))
        .flat_map(|arr| arr.iter())
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();

    // Walk events backward; for each assistant message, scan its tool_use
    // blocks (also in reverse) and return the first one whose id is in
    // `completed_ids`.
    for ev in events.iter().rev() {
        if ev.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = ev.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else { continue };
        let timestamp = ev.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let is_sidechain = ev.get("isSidechain").and_then(|b| b.as_bool()).unwrap_or(false);
        for block in blocks.iter().rev() {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") { continue; }
            let Some(id) = block.get("id").and_then(|i| i.as_str()) else { continue };
            if !completed_ids.contains(id) { continue; }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
            let preview = preview_for_tool(&name, block.get("input"));
            return Some(InFlightTool {
                name,
                started_at: timestamp,
                preview,
                is_sidechain,
            });
        }
    }
    None
}

fn find_last_assistant_text(events: &[Value]) -> Option<String> {
    for ev in events.iter().rev() {
        if ev.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(arr) = ev
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        return Some(truncate(trimmed, 200));
                    }
                }
            }
        }
    }
    None
}

fn preview_for_tool(name: &str, input: Option<&Value>) -> Option<String> {
    let input = input?;
    let field: Option<&str> = match name {
        "Bash" => input.get("command").and_then(|v| v.as_str()),
        "Edit" | "Write" | "Read" | "NotebookEdit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(basename),
        "Grep" => input.get("pattern").and_then(|v| v.as_str()),
        "Glob" => input.get("pattern").and_then(|v| v.as_str()),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .or_else(|| input.get("prompt").and_then(|v| v.as_str())),
        "TaskCreate" | "TaskUpdate" => input
            .get("subject")
            .and_then(|v| v.as_str())
            .or_else(|| input.get("description").and_then(|v| v.as_str())),
        "WebFetch" => input.get("url").and_then(|v| v.as_str()),
        "WebSearch" => input.get("query").and_then(|v| v.as_str()),
        "Skill" => input.get("skill").and_then(|v| v.as_str()),
        _ => None,
    };
    field.map(|s| truncate(s, PREVIEW_MAX))
}

fn basename<'a>(path: &'a str) -> &'a str {
    path.rsplit('/').next().unwrap_or(path)
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;



    #[test]
    fn parses_empty_input() {
        let s = parse_events("");
        assert!(s.in_flight.is_empty());
        assert!(s.last_activity_at.is_none());
    }

    #[test]
    fn pairs_tool_use_with_tool_result() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"content":[{"type":"tool_use","id":"tid1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"tid1"}]}}"#,
        ].join("\n");
        let s = parse_events(&lines);
        assert!(s.in_flight.is_empty());
        assert_eq!(s.last_activity_at.as_deref(), Some("2026-01-01T00:00:01Z"));
    }

    #[test]
    fn reports_unmatched_tool_use_as_in_flight() {
        let line = r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"content":[{"type":"tool_use","id":"tid1","name":"Bash","input":{"command":"sleep 5"}}]}}"#;
        let s = parse_events(line);
        assert_eq!(s.in_flight.len(), 1);
        assert_eq!(s.in_flight[0].name, "Bash");
        assert_eq!(s.in_flight[0].preview.as_deref(), Some("sleep 5"));
    }

    #[test]
    fn ignores_malformed_lines() {
        let lines = "not json\n{\"no_type\":true}\n";
        let s = parse_events(lines);
        assert!(s.in_flight.is_empty());
    }

    #[test]
    fn captures_last_assistant_text() {
        let line = r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
        let s = parse_events(line);
        assert_eq!(s.last_assistant_text.as_deref(), Some("Hello world"));
    }

    #[test]
    fn basename_strips_path() {
        assert_eq!(basename("/foo/bar/baz.rs"), "baz.rs");
        assert_eq!(basename("baz.rs"), "baz.rs");
    }

    #[test]
    fn truncate_respects_length() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("abcdefghijkl", 5), "abcd…");
    }

    #[test]
    fn session_json_tolerates_missing_status() {
        // Older CC versions omit status/waitingFor — parse_session_full
        // should still return the session, leaving apply_session_state a no-op.
        let body = r#"{"sessionId":"abc-123","pid":1}"#;
        let p = parse_session_full(body).expect("parse");
        assert!(p.state.status.is_none());
        assert!(p.state.waiting_for.is_none());
    }

    #[test]
    fn session_json_tolerates_malformed() {
        assert!(parse_session_full("not json").is_none());
        assert!(parse_session_full("{}").is_none());
    }

    #[test]
    fn apply_session_state_flips_to_waiting() {
        let mut status = CcStatus {
            activity: CcActivity::Running,
            in_flight: vec![InFlightTool {
                name: "Edit".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
                preview: Some("foo.rs".into()),
                is_sidechain: false,
            }],
            last_completed_tool: None,
            last_activity_at: None,
            last_assistant_text: None,
            waiting_for: None,
        };
        apply_session_state(
            &mut status,
            &SessionState {
                status: Some("waiting".into()),
                waiting_for: Some("approve Edit".into()),
            },
        );
        assert!(matches!(status.activity, CcActivity::Waiting));
        assert_eq!(status.waiting_for.as_deref(), Some("approve Edit"));
        // In-flight tool is preserved — the UI can still say "waiting on Edit foo.rs".
        assert_eq!(status.in_flight.len(), 1);
    }

    #[test]
    fn parse_session_full_captures_fields() {
        let body = r#"{"pid":1,"sessionId":"sid","cwd":"/x/y","status":"waiting","waitingFor":"approve Edit","updatedAt":42}"#;
        let p = parse_session_full(body).expect("parse");
        assert_eq!(p.session_id, "sid");
        assert_eq!(p.cwd.as_deref(), Some("/x/y"));
        assert_eq!(p.updated_at, 42);
        assert_eq!(p.state.status.as_deref(), Some("waiting"));
        assert_eq!(p.state.waiting_for.as_deref(), Some("approve Edit"));
    }

    #[test]
    fn apply_session_state_ignores_non_waiting() {
        let mut status = CcStatus {
            activity: CcActivity::Running,
            in_flight: vec![],
            last_completed_tool: None,
            last_activity_at: None,
            last_assistant_text: None,
            waiting_for: None,
        };
        apply_session_state(
            &mut status,
            &SessionState {
                status: Some("idle".into()),
                waiting_for: None,
            },
        );
        assert!(matches!(status.activity, CcActivity::Running));
    }

    /// Runs `body` with HOME pointed at a scratch dir, restoring it
    /// afterward. `body` gets the scratch dir so it can lay out
    /// `.claude/sessions` and `.claude/projects` under it.
    ///
    /// SAFETY: mutates the process-global HOME env var. Every test calling
    /// this (and config::tests' two HOME/XDG_CONFIG_HOME tests) is tagged
    /// `#[serial(env)]` so they can't race each other — cargo test runs a
    /// binary's tests in parallel by default, and without that they will
    /// intermittently clobber each other's HOME mid-test.
    fn with_scratch_home(test_name: &str, body: impl FnOnce(&Path)) {
        let prev_home = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "pr-loop-cc-status-test-{}-{}",
            test_name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        body(&tmp);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_live_session_by_pid_reads_file_directly() {
        with_scratch_home("read-live-session", |home| {
            let sessions_dir = home.join(".claude/sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            std::fs::write(
                sessions_dir.join("4242.json"),
                r#"{"pid":4242,"sessionId":"abc-123","cwd":"/some/real/project","status":"waiting","waitingFor":"approve Edit"}"#,
            )
            .unwrap();

            let (path, parsed) = read_live_session_by_pid(4242).expect("session file found");
            assert_eq!(path, sessions_dir.join("4242.json"));
            assert_eq!(parsed.cwd.as_deref(), Some("/some/real/project"));
            assert_eq!(parsed.session_id, "abc-123");
            assert_eq!(parsed.state.status.as_deref(), Some("waiting"));
            assert_eq!(parsed.state.waiting_for.as_deref(), Some("approve Edit"));

            assert!(read_live_session_by_pid(9999).is_none());
        });
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_cc_status_for_pid_none_when_session_file_missing() {
        // The exact contract web::handle_request's /api/state relies on for
        // its checkout_path fallback: no session file for this PID at all
        // means None, not a panic or a status for the wrong directory.
        with_scratch_home("no-session-file", |_home| {
            assert!(read_cc_status_for_pid(424242).is_none());
        });
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_cc_status_for_pid_finds_transcript_via_session_cwd() {
        with_scratch_home("full-pipeline", |home| {
            let project_dir = home.join("work/my-project");
            std::fs::create_dir_all(&project_dir).unwrap();

            let sessions_dir = home.join(".claude/sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            std::fs::write(
                sessions_dir.join("111.json"),
                format!(
                    r#"{{"pid":111,"sessionId":"sid-1","cwd":"{}"}}"#,
                    project_dir.display()
                ),
            )
            .unwrap();

            // The transcript directory is named by encoding the *session's*
            // cwd (not some other path) — this is the exact bug being
            // guarded against: using the right directory. Canonicalize
            // first, matching `session_dir_for_cwd` itself — on macOS
            // std::env::temp_dir() lives under /var, a symlink to
            // /private/var, so the raw and canonical paths differ.
            let encoded: String = project_dir
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let transcript_dir = home.join(".claude/projects").join(encoded);
            std::fs::create_dir_all(&transcript_dir).unwrap();
            std::fs::write(
                transcript_dir.join("sid-1.jsonl"),
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"content":[{"type":"text","text":"Hello from the right session"}]}}"#,
            )
            .unwrap();

            let status = read_cc_status_for_pid(111).expect("status found");
            assert_eq!(
                status.last_assistant_text.as_deref(),
                Some("Hello from the right session")
            );
        });
    }
}
