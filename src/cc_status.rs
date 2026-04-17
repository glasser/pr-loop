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
}

#[derive(Debug, Clone, Serialize)]
pub struct CcStatus {
    pub activity: CcActivity,
    pub in_flight: Vec<InFlightTool>,
    pub last_activity_at: Option<String>,
    pub last_assistant_text: Option<String>,
}

pub fn read_cc_status(cwd: &Path) -> Option<CcStatus> {
    let dir = session_dir_for_cwd(cwd)?;
    let file = newest_jsonl(&dir)?;
    let content = read_tail(&file, MAX_TAIL_BYTES).ok()?;
    Some(parse_events(&content))
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
                last_activity_at,
                last_assistant_text,
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

    CcStatus {
        activity,
        in_flight,
        last_activity_at,
        last_assistant_text,
    }
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
}
