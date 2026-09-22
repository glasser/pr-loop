// Per-PR "tracker" logic used by `pr-loop hub`. Each tracked PR gets a
// `Shared` + background poll thread (spawned by `spawn_tracker`, owned by the
// hub's tracker map) that mirrors what a dedicated `pr-loop web` process used
// to do on its own — except now `pr-loop hub` is the only long-running
// process, and it multiplexes many PRs in one server.
//
// A PR becomes tracked when the hub receives a `POST /api/register` (sent by
// any `pr-loop` invocation right after it resolves its PR context) naming
// that PR's owner/repo/number and the checkout path it was run from. The hub
// prunes trackers that haven't been re-registered recently — see
// `hub::FRESHNESS_WINDOW`.

use crate::cc_status::{read_cc_status, read_cc_status_for_pid, CcStatus};
use crate::checks::{Check, CheckStatus, ChecksClient, RealChecksClient};
use crate::commits::{CommitsClient, PrCommit, RealCommitsClient};
use crate::git::{GitClient, RealGitClient};
use crate::github::PrContext;
use crate::reply::{RealReplyClient, ReplyClient};
use crate::threads::{CLAUDE_IN_PROGRESS_MARKER, CLAUDE_MARKER};
use crate::threads::{RealThreadsClient, ReviewThread, ThreadComment, ThreadsClient};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Response};

const INDEX_HTML: &str = include_str!("index.html");

/// How often the poller re-fetches from GitHub even when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How often the poller checks the local git ref for changes.
const GIT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Serialize)]
struct PrDto {
    owner: String,
    repo: String,
    pr_number: u64,
    title: Option<String>,
    url: Option<String>,
}

#[derive(Clone, Serialize)]
struct CommentDto {
    id: String,
    author: String,
    body: String,
    diff_hunk: Option<String>,
    url: Option<String>,
    created_at: Option<String>,
}

impl From<&ThreadComment> for CommentDto {
    fn from(c: &ThreadComment) -> Self {
        Self {
            id: c.id.clone(),
            author: c.author.clone(),
            body: c.body.clone(),
            diff_hunk: c.diff_hunk.clone(),
            url: c.url.clone(),
            created_at: c.created_at.clone(),
        }
    }
}

#[derive(Clone, Serialize)]
struct ThreadDto {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    is_paperclip: bool,
    /// True if the last comment is Claude's interim acknowledgment rather
    /// than a final reply — the thread is being worked on but still needs
    /// a real response.
    is_in_progress: bool,
    path: Option<String>,
    line: Option<u64>,
    comments: Vec<CommentDto>,
}

impl From<&ReviewThread> for ThreadDto {
    fn from(t: &ReviewThread) -> Self {
        Self {
            id: t.id.clone(),
            is_resolved: t.is_resolved,
            is_outdated: t.is_outdated,
            is_paperclip: t.has_paperclip(),
            is_in_progress: t
                .last_comment()
                .is_some_and(|c| c.body.starts_with(CLAUDE_IN_PROGRESS_MARKER)),
            path: t.path.clone(),
            line: t.line,
            comments: t.comments.iter().map(CommentDto::from).collect(),
        }
    }
}

#[derive(Clone, Serialize)]
struct CommitDto {
    sha: String,
    abbreviated_sha: String,
    message_headline: String,
    /// First non-empty line of the commit body; None if empty.
    message_body_first_line: Option<String>,
    committed_date: String,
    author_name: Option<String>,
    author_login: Option<String>,
    url: String,
}

impl From<&PrCommit> for CommitDto {
    fn from(c: &PrCommit) -> Self {
        let message_body_first_line = c
            .message_body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.to_string());
        Self {
            sha: c.sha.clone(),
            abbreviated_sha: c.abbreviated_sha.clone(),
            message_headline: c.message_headline.clone(),
            message_body_first_line,
            committed_date: c.committed_date.clone(),
            author_name: c.author_name.clone(),
            author_login: c.author_login.clone(),
            url: c.url.clone(),
        }
    }
}

#[derive(Clone, Serialize)]
struct CheckDto {
    name: String,
    /// One of: "pass", "fail", "pending", "skipping", "cancelled".
    status: &'static str,
    url: Option<String>,
}

impl From<&Check> for CheckDto {
    fn from(c: &Check) -> Self {
        let status = match c.status {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "fail",
            CheckStatus::Pending => "pending",
            CheckStatus::Skipping => "skipping",
            CheckStatus::Cancelled => "cancelled",
        };
        Self {
            name: c.name.clone(),
            status,
            url: c.url.clone(),
        }
    }
}

#[derive(Clone, Serialize, Default)]
struct State {
    pr: Option<PrDto>,
    threads: Vec<ThreadDto>,
    commits: Vec<CommitDto>,
    checks: Vec<CheckDto>,
    last_fetched_at: Option<String>,
    last_error: Option<String>,
}

/// What gets serialized to the client on each /api/state call. Combines the
/// cached PR state with a fresh snapshot of Claude Code's transcript so the
/// CC status updates at client-poll cadence (1s), not GitHub-poll cadence.
#[derive(Serialize)]
struct StateResponse<'a> {
    #[serde(flatten)]
    state: &'a State,
    cc_status: Option<CcStatus>,
    /// True when the `pr-loop` binary on disk has been rebuilt since the hub
    /// process started. The UI surfaces a "restart" pill when this flips.
    update_available: bool,
    /// The checkout path the most recent `/api/register` ping reported for
    /// this PR — used for the local git-ref fast path, and as a cc_status
    /// fallback when `claude_pid` isn't set. Surfaced for debugging.
    checkout_path: String,
    /// The `CLAUDE_PID` the most recent `/api/register` ping reported, if
    /// any — see `Shared::claude_pid`. Surfaced for debugging "why is this
    /// showing the wrong session".
    claude_pid: Option<u32>,
}

/// Per-PR tracker state, owned by the hub's tracker map. One of these exists
/// for as long as the hub considers the PR "recently active" (see
/// `hub::FRESHNESS_WINDOW`); the hub tears it down (via `request_stop`) once
/// it goes stale.
pub struct Shared {
    pub pr_context: PrContext,
    state: Mutex<State>,
    // Condvar-paired flag so handlers can poke the poller.
    trigger: (Mutex<bool>, Condvar),
    /// The checkout directory the most recent `pr-loop` invocation for this
    /// PR ran from. Used for cc_status lookup and the local git-ref-change
    /// fast path — both inherently path-scoped, not tied to the hub's own
    /// cwd. Refreshed on every `/api/register` ping.
    pub checkout_path: Mutex<PathBuf>,
    /// The PID of the Claude Code process that most recently ran a
    /// `pr-loop` command against this PR, if any (from the `CLAUDE_PID` env
    /// var — see `register`). Preferred over `checkout_path` for cc_status:
    /// it's Claude Code's own recorded cwd for that exact session, so it
    /// can't be thrown off by the invoking shell having `cd`'d somewhere
    /// else before running `pr-loop`, and can't be confused by another
    /// Claude Code session that happens to share a directory.
    pub claude_pid: Mutex<Option<u32>>,
    /// Last time `/api/register` refreshed this tracker. The hub's reaper
    /// uses this to decide when to tear the tracker down.
    pub last_seen: Mutex<Instant>,
    /// Set once a fetch observes the PR as merged. Unlike staleness this is
    /// permanent — a merged PR can't become unmerged — so once set, the hub
    /// tears the tracker down regardless of how recently it was pinged.
    merged: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    fn poke(&self) {
        let (lock, cvar) = &self.trigger;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Wait up to `timeout` for a poke. Returns true if poked.
    fn wait_for_poke(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &self.trigger;
        let guard = lock.lock().unwrap();
        let (mut guard, _) = cvar.wait_timeout(guard, timeout).unwrap();
        let was_poked = *guard;
        *guard = false;
        was_poked
    }

    /// Tell this tracker's poll thread to exit at its next check (within
    /// `GIT_CHECK_INTERVAL`). Called by the hub's reaper when the tracker
    /// goes stale or merged.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// True once a fetch has observed the PR as merged.
    pub fn is_merged(&self) -> bool {
        self.merged.load(Ordering::Relaxed)
    }
}

/// Extra per-request context the hub supplies that isn't specific to any one
/// tracker: whether a binary rebuild was detected (hub-wide), and the list of
/// other currently-tracked PRs (for the in-page "N other pr-loops" widget).
pub struct RequestContext<'a> {
    pub update_available: bool,
    pub peers: &'a [PeerInfo],
}

/// Create a tracker for `pr_context` and spawn its background poll thread.
/// Returns the shared handle for the hub to store in its tracker map and
/// route requests into.
pub fn spawn_tracker(
    pr_context: PrContext,
    checkout_path: PathBuf,
    claude_pid: Option<u32>,
) -> Arc<Shared> {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            pr: Some(PrDto {
                owner: pr_context.owner.clone(),
                repo: pr_context.repo.clone(),
                pr_number: pr_context.pr_number,
                title: None,
                url: None,
            }),
            ..Default::default()
        }),
        trigger: (Mutex::new(false), Condvar::new()),
        checkout_path: Mutex::new(checkout_path),
        claude_pid: Mutex::new(claude_pid),
        last_seen: Mutex::new(Instant::now()),
        merged: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        pr_context,
    });

    let shared_poll = Arc::clone(&shared);
    thread::spawn(move || poll_loop(shared_poll));

    shared
}

/// Summary of a tracked PR for the hub's chooser page and the in-page
/// "N other pr-loops" widget. Computed directly from a tracker's cached
/// state — no network round-trip needed since the hub holds every tracker
/// in-process.
#[derive(Clone, Serialize, Default)]
pub struct PeerInfo {
    pub pr_owner: String,
    pub pr_repo: String,
    pub pr_number: u64,
    pub pr_title: Option<String>,
    pub unresolved_threads: u32,
    pub needs_response: u32,
}

/// Build a `PeerInfo` snapshot for one tracker.
pub fn peer_info(shared: &Shared) -> PeerInfo {
    let state = shared.state.lock().unwrap();
    let mut unresolved_threads = 0u32;
    let mut needs_response = 0u32;
    for t in &state.threads {
        if t.is_resolved || t.is_paperclip {
            continue;
        }
        unresolved_threads += 1;
        if let Some(last) = t.comments.last() {
            if !last.body.starts_with(CLAUDE_MARKER) {
                needs_response += 1;
            }
        }
    }
    PeerInfo {
        pr_owner: shared.pr_context.owner.clone(),
        pr_repo: shared.pr_context.repo.clone(),
        pr_number: shared.pr_context.pr_number,
        pr_title: state.pr.as_ref().and_then(|p| p.title.clone()),
        unresolved_threads,
        needs_response,
    }
}

/// Handle one HTTP request routed to this tracker. `path` is the request
/// path *within* the PR's namespace (e.g. `/api/state`), already stripped of
/// the hub's `/pr/<owner>/<repo>/<pr>` prefix.
pub fn handle_request(
    mut request: tiny_http::Request,
    path: &str,
    shared: &Arc<Shared>,
    ctx: &RequestContext,
) -> Result<()> {
    let pr_context = &shared.pr_context;
    let method = request.method().clone();

    let resp = match (&method, path) {
        (&Method::Get, "/") => build_response(INDEX_HTML.to_string(), "text/html; charset=utf-8", 200),
        (&Method::Get, "/api/state") => {
            let state = shared.state.lock().unwrap().clone();
            let checkout_path = shared.checkout_path.lock().unwrap().clone();
            let claude_pid = *shared.claude_pid.lock().unwrap();
            // Prefer the Claude Code session we know is driving this PR
            // (looked up directly by PID) over checkout_path — the latter is
            // only pr-loop's invocation cwd, which can differ if the agent
            // `cd`'d into a checkout before running `pr-loop` there. Falls
            // back to the checkout_path-based lookup if there's no PID on
            // record, or its session file is gone (process exited).
            let cc_status = claude_pid
                .and_then(read_cc_status_for_pid)
                .or_else(|| read_cc_status(&checkout_path));
            let response = StateResponse {
                state: &state,
                cc_status,
                update_available: ctx.update_available,
                checkout_path: checkout_path.to_string_lossy().into_owned(),
                claude_pid,
            };
            let body = serde_json::to_string(&response)?;
            build_response(body, "application/json", 200)
        }
        (&Method::Post, "/api/poke") => {
            shared.poke();
            build_response("{}".to_string(), "application/json", 200)
        }
        (&Method::Post, "/api/restart") => {
            #[cfg(unix)]
            {
                thread::spawn(restart_self);
                build_response("{}".to_string(), "application/json", 200)
            }
            #[cfg(not(unix))]
            {
                build_response(
                    r#"{"error":"restart is only supported on Unix"}"#.to_string(),
                    "application/json",
                    501,
                )
            }
        }
        (&Method::Get, "/api/peers") => {
            let body = serde_json::to_string(ctx.peers)?;
            build_response(body, "application/json", 200)
        }
        (&Method::Post, p) if p.starts_with("/api/threads/") && p.ends_with("/resolve") => {
            let thread_id =
                decode_thread_id(&p["/api/threads/".len()..p.len() - "/resolve".len()]);
            let client = RealReplyClient;
            match client.resolve_thread(&thread_id) {
                Ok(()) => {
                    // Synchronously re-fetch so the client's next /api/state
                    // call (which usually follows immediately) sees the new
                    // state. Also poke the poller to reset its interval.
                    refresh_state(pr_context, shared);
                    shared.poke();
                    build_response("{}".to_string(), "application/json", 200)
                }
                Err(e) => build_response(
                    format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'")),
                    "application/json",
                    500,
                ),
            }
        }
        (&Method::Post, p) if p.starts_with("/api/threads/") && p.ends_with("/reply") => {
            let thread_id =
                decode_thread_id(&p["/api/threads/".len()..p.len() - "/reply".len()]);
            let mut body_bytes = Vec::new();
            request
                .as_reader()
                .read_to_end(&mut body_bytes)
                .context("read request body")?;

            #[derive(serde::Deserialize)]
            struct ReplyReq {
                body: String,
            }

            match serde_json::from_slice::<ReplyReq>(&body_bytes) {
                Ok(payload) => {
                    let client = RealReplyClient;
                    // Post the user's reply verbatim — the UI is driven by a
                    // human, so we don't apply the Claude marker prefix.
                    match client.post_reply(&thread_id, &payload.body) {
                        Ok(_) => {
                            refresh_state(pr_context, shared);
                            shared.poke();
                            build_response("{}".to_string(), "application/json", 200)
                        }
                        Err(e) => build_response(
                            format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'")),
                            "application/json",
                            500,
                        ),
                    }
                }
                Err(e) => build_response(
                    format!("{{\"error\":\"invalid JSON: {}\"}}", e),
                    "application/json",
                    400,
                ),
            }
        }
        _ => build_response("not found".to_string(), "text/plain", 404),
    };

    request
        .respond(resp)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))?;
    Ok(())
}

/// Synchronously re-fetch threads + commits from GitHub and update the cache.
/// Called from mutation handlers so the next /api/state is fresh.
fn refresh_state(pr_context: &PrContext, shared: &Arc<Shared>) {
    let threads_client = RealThreadsClient;
    let commits_client = RealCommitsClient;
    fetch_now(pr_context, &threads_client, &commits_client, shared);
}

fn build_response(body: String, ct: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(content_type(ct))
        .with_status_code(status)
}

fn content_type(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).unwrap()
}

fn decode_thread_id(raw: &str) -> String {
    urlencoding::decode(raw)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

fn poll_loop(shared: Arc<Shared>) {
    let threads_client = RealThreadsClient;
    let commits_client = RealCommitsClient;
    let git = RealGitClient;

    let mut last_head: Option<String> = None;
    let mut last_fetch = Instant::now() - POLL_INTERVAL; // force immediate fetch

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }

        let now = Instant::now();
        let checkout_path = shared.checkout_path.lock().unwrap().clone();
        let ref_changed = match git.get_head_hash_at(&checkout_path) {
            Ok(h) => {
                let changed = last_head.as_deref() != Some(h.as_str());
                last_head = Some(h);
                changed
            }
            Err(_) => false,
        };

        let should_fetch = ref_changed || now.duration_since(last_fetch) >= POLL_INTERVAL;

        if should_fetch {
            fetch_now(&shared.pr_context, &threads_client, &commits_client, &shared);
            last_fetch = Instant::now();
        }

        // Once merged there's nothing left to poll for — a merged PR can't
        // reopen or change further. Stop immediately rather than waiting for
        // the hub's reaper to notice (it will, within REAP_INTERVAL, and tear
        // this tracker down regardless of freshness).
        if shared.stop.load(Ordering::Relaxed) || shared.is_merged() {
            return;
        }

        // Wait for either a poke or the git check interval to elapse.
        if shared.wait_for_poke(GIT_CHECK_INTERVAL) {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            // Poked — fetch immediately.
            fetch_now(&shared.pr_context, &threads_client, &commits_client, &shared);
            last_fetch = Instant::now();
        }
    }
}

fn fetch_now(
    pr_context: &PrContext,
    threads_client: &dyn ThreadsClient,
    commits_client: &dyn CommitsClient,
    shared: &Arc<Shared>,
) {
    let threads_result =
        threads_client.fetch_threads(&pr_context.owner, &pr_context.repo, pr_context.pr_number);
    let pr_info_result =
        commits_client.fetch_pr_info(&pr_context.owner, &pr_context.repo, pr_context.pr_number);
    // Checks failures shouldn't block threads/commits from rendering, so
    // fetch independently and keep whatever works.
    let checks_client = RealChecksClient;
    let checks_result =
        checks_client.fetch_checks(&pr_context.owner, &pr_context.repo, pr_context.pr_number);

    let mut is_merged = false;
    let mut state = shared.state.lock().unwrap();

    match (threads_result, pr_info_result) {
        (Ok(threads), Ok(pr_info)) => {
            state.threads = threads.iter().map(ThreadDto::from).collect();
            // GitHub returns commits oldest-first; UI shows newest on top.
            state.commits = pr_info.commits.iter().rev().map(CommitDto::from).collect();
            if let Some(pr) = state.pr.as_mut() {
                pr.title = Some(pr_info.title).filter(|s| !s.is_empty());
                pr.url = Some(pr_info.url).filter(|s| !s.is_empty());
            }
            is_merged = pr_info.is_merged;
            state.last_error = None;
        }
        (Err(e), _) | (_, Err(e)) => {
            state.last_error = Some(e.to_string());
        }
    }
    if let Ok(checks) = checks_result {
        state.checks = checks.iter().map(CheckDto::from).collect();
    }
    state.last_fetched_at = Some(iso_now());
    drop(state);

    if is_merged {
        shared.merged.store(true, Ordering::Relaxed);
    }
}

fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_iso8601(secs)
}

/// Format a Unix timestamp as an ISO-8601 UTC string (seconds precision).
/// Tiny hand-rolled impl to avoid pulling in a date crate for one call site.
fn format_iso8601(secs: i64) -> String {
    // Roughly accurate for dates within a sane range. Good enough for "X min ago".
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, mo, d, h, m, s
    )
}

/// Howard Hinnant's civil_from_days algorithm. Converts days since 1970-01-01 to (y,m,d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Re-exec ourselves with the original argv after a short pause so the
/// restart HTTP response has time to flush. Never returns on success. Since
/// the hub is the only long-running process now, this restarts the hub
/// itself, dropping every tracker's cache (cheap — each refetches on its
/// next poll).
#[cfg(unix)]
fn restart_self() {
    use std::os::unix::process::CommandExt;
    thread::sleep(Duration::from_millis(200));
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pr-loop hub: restart failed (current_exe): {}", e);
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!(
        "pr-loop hub: restarting via exec: {} {}",
        exe.display(),
        args.join(" ")
    );
    let err = std::process::Command::new(exe).args(&args).exec();
    // exec() only returns on failure.
    eprintln!("pr-loop hub: restart failed (exec): {}", err);
    std::process::exit(1);
}

/// Poll the binary's mtime every few seconds; when it changes from what we
/// observed at startup, flip `update_available`. Never flips back — the UI's
/// restart action is the only way to "reset" it (via exec). Runs forever;
/// spawned once by the hub, not per-tracker.
pub fn watch_binary_mtime(exe: PathBuf, original_mtime: SystemTime, update_available: Arc<AtomicBool>) {
    let mut announced = false;
    loop {
        thread::sleep(Duration::from_secs(2));
        let Ok(meta) = std::fs::metadata(&exe) else { continue };
        let Ok(m) = meta.modified() else { continue };
        if m != original_mtime {
            if !announced {
                eprintln!("pr-loop hub: detected binary rebuild at {}", exe.display());
                announced = true;
            }
            update_available.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_with_threads(threads: Vec<ThreadDto>) -> Shared {
        Shared {
            pr_context: PrContext {
                owner: "o".to_string(),
                repo: "r".to_string(),
                pr_number: 1,
            },
            state: Mutex::new(State {
                threads,
                ..Default::default()
            }),
            trigger: (Mutex::new(false), Condvar::new()),
            checkout_path: Mutex::new(PathBuf::from(".")),
            claude_pid: Mutex::new(None),
            last_seen: Mutex::new(Instant::now()),
            merged: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }

    fn thread(id: &str, resolved: bool, paperclip: bool, last_comment_body: &str) -> ThreadDto {
        ThreadDto {
            id: id.to_string(),
            is_resolved: resolved,
            is_outdated: false,
            is_paperclip: paperclip,
            is_in_progress: last_comment_body.starts_with(CLAUDE_IN_PROGRESS_MARKER),
            path: None,
            line: None,
            comments: vec![CommentDto {
                id: "c1".to_string(),
                author: "reviewer".to_string(),
                body: last_comment_body.to_string(),
                diff_hunk: None,
                url: None,
                created_at: None,
            }],
        }
    }

    #[test]
    fn peer_info_counts_unresolved_and_needs_response() {
        let shared = shared_with_threads(vec![
            thread("t1", false, false, "please fix this"),
            thread("t2", false, false, &format!("{} done", CLAUDE_MARKER)),
            thread("t3", true, false, "please fix this"), // resolved, excluded
            thread("t4", false, true, "please fix this"), // paperclip, excluded
        ]);

        let info = peer_info(&shared);

        assert_eq!(info.pr_owner, "o");
        assert_eq!(info.pr_repo, "r");
        assert_eq!(info.pr_number, 1);
        assert_eq!(info.unresolved_threads, 2);
        assert_eq!(info.needs_response, 1);
    }

    #[test]
    fn peer_info_empty_threads() {
        let shared = shared_with_threads(vec![]);
        let info = peer_info(&shared);
        assert_eq!(info.unresolved_threads, 0);
        assert_eq!(info.needs_response, 0);
    }

    #[test]
    fn peer_info_in_progress_ack_still_needs_response() {
        let shared = shared_with_threads(vec![thread(
            "t1",
            false,
            false,
            &format!("{} Looking into it", CLAUDE_IN_PROGRESS_MARKER),
        )]);

        let info = peer_info(&shared);

        assert_eq!(info.unresolved_threads, 1);
        assert_eq!(info.needs_response, 1);
    }

    #[test]
    fn peer_info_no_comments_never_needs_response() {
        // A thread with no comments shouldn't happen in practice, but make
        // sure it doesn't panic or count as needing a response.
        let mut t = thread("t1", false, false, "unused");
        t.comments.clear();
        let shared = shared_with_threads(vec![t]);
        let info = peer_info(&shared);
        assert_eq!(info.unresolved_threads, 1);
        assert_eq!(info.needs_response, 0);
    }
}
