// Local HTTP server for `pr-loop web`. Shows unresolved review threads + PR
// commits in a browser with live updates.

use crate::cc_status::{read_cc_status, CcStatus};
use crate::commits::{CommitsClient, PrCommit, RealCommitsClient};
use crate::git::{GitClient, RealGitClient};
use crate::github::PrContext;
use crate::reply::{RealReplyClient, ReplyClient};
use crate::threads::{RealThreadsClient, ReviewThread, ThreadComment, ThreadsClient};
use anyhow::{Context, Result};
use serde::Serialize;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Response, Server};

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
    committed_date: String,
    author_name: Option<String>,
    author_login: Option<String>,
    url: String,
}

impl From<&PrCommit> for CommitDto {
    fn from(c: &PrCommit) -> Self {
        Self {
            sha: c.sha.clone(),
            abbreviated_sha: c.abbreviated_sha.clone(),
            message_headline: c.message_headline.clone(),
            committed_date: c.committed_date.clone(),
            author_name: c.author_name.clone(),
            author_login: c.author_login.clone(),
            url: c.url.clone(),
        }
    }
}

#[derive(Clone, Serialize, Default)]
struct State {
    pr: Option<PrDto>,
    threads: Vec<ThreadDto>,
    commits: Vec<CommitDto>,
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
}

struct Shared {
    state: Mutex<State>,
    // Condvar-paired flag so handlers can poke the poller.
    trigger: (Mutex<bool>, Condvar),
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
}

pub fn run(pr_context: &PrContext, port: Option<u16>, no_open: bool) -> Result<()> {
    // Bind first so we know the assigned port before spawning anything.
    let addr: SocketAddr = ([127, 0, 0, 1], port.unwrap_or(0)).into();
    let listener = TcpListener::bind(addr).context("Failed to bind TCP listener")?;
    let bound_port = listener.local_addr()?.port();
    let server = Server::from_listener(listener, None)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP server: {}", e))?;
    let url = format!("http://127.0.0.1:{}/", bound_port);

    let port_file = write_port_file(pr_context, bound_port)?;

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
    });

    // Poller thread
    let pr_clone = pr_context.clone();
    let shared_poll = Arc::clone(&shared);
    thread::spawn(move || poll_loop(pr_clone, shared_poll));

    eprintln!("pr-loop web: serving at {}", url);
    eprintln!("PR: {}/{}#{}", pr_context.owner, pr_context.repo, pr_context.pr_number);
    if !no_open {
        if let Err(e) = open::that(&url) {
            eprintln!("Warning: Failed to open browser: {}. Open the URL manually.", e);
        }
    }

    // Ctrl-C cleanup for the port file
    let port_file_clone = port_file.clone();
    let _ = ctrlc_cleanup(port_file_clone);

    // Request loop
    let pr_for_handlers = pr_context.clone();
    for request in server.incoming_requests() {
        let shared = Arc::clone(&shared);
        let pr_ctx = pr_for_handlers.clone();
        // Handle synchronously — requests are cheap and the workload is small.
        // A reply or resolve mutation may take ~1s but that's fine for a local UI.
        if let Err(e) = handle_request(request, &shared, &pr_ctx) {
            eprintln!("web: request error: {}", e);
        }
    }

    let _ = std::fs::remove_file(&port_file);
    Ok(())
}

fn handle_request(
    mut request: tiny_http::Request,
    shared: &Arc<Shared>,
    pr_context: &PrContext,
) -> Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or(&url).to_string();

    let resp = match (&method, path.as_str()) {
        (&Method::Get, "/") => build_response(INDEX_HTML.to_string(), "text/html; charset=utf-8", 200),
        (&Method::Get, "/api/state") => {
            let state = shared.state.lock().unwrap().clone();
            let cwd = std::env::current_dir().ok();
            let cc_status = cwd.as_deref().and_then(read_cc_status);
            let response = StateResponse {
                state: &state,
                cc_status,
            };
            let body = serde_json::to_string(&response)?;
            build_response(body, "application/json", 200)
        }
        (&Method::Post, "/api/poke") => {
            shared.poke();
            build_response("{}".to_string(), "application/json", 200)
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

fn poll_loop(pr_context: PrContext, shared: Arc<Shared>) {
    let threads_client = RealThreadsClient;
    let commits_client = RealCommitsClient;
    let git = RealGitClient;

    let mut last_head: Option<String> = None;
    let mut last_fetch = Instant::now() - POLL_INTERVAL; // force immediate fetch

    loop {
        let now = Instant::now();
        let ref_changed = match git.get_head_hash() {
            Ok(h) => {
                let changed = last_head.as_deref() != Some(h.as_str());
                last_head = Some(h);
                changed
            }
            Err(_) => false,
        };

        let should_fetch = ref_changed || now.duration_since(last_fetch) >= POLL_INTERVAL;

        if should_fetch {
            fetch_now(&pr_context, &threads_client, &commits_client, &shared);
            last_fetch = Instant::now();
        }

        // Wait for either a poke or the git check interval to elapse.
        if shared.wait_for_poke(GIT_CHECK_INTERVAL) {
            // Poked — fetch immediately.
            fetch_now(&pr_context, &threads_client, &commits_client, &shared);
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
            state.last_error = None;
        }
        (Err(e), _) | (_, Err(e)) => {
            state.last_error = Some(e.to_string());
        }
    }
    state.last_fetched_at = Some(iso_now());
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

fn port_file_path(pr_context: &PrContext) -> Result<PathBuf> {
    let base = dirs_cache()?;
    let dir = base.join("pr-loop");
    std::fs::create_dir_all(&dir).context("create cache dir")?;
    let safe = format!(
        "web-{}-{}-{}.port",
        sanitize(&pr_context.owner),
        sanitize(&pr_context.repo),
        pr_context.pr_number
    );
    Ok(dir.join(safe))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn dirs_cache() -> Result<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from(home).join("Library/Caches"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(PathBuf::from(home).join(".cache"))
    }
}

fn write_port_file(pr_context: &PrContext, port: u16) -> Result<PathBuf> {
    let path = port_file_path(pr_context)?;
    std::fs::write(&path, port.to_string()).context("write port file")?;
    Ok(path)
}

/// Read a port file if present.
fn read_port_for_pr(pr_context: &PrContext) -> Option<u16> {
    let path = port_file_path(pr_context).ok()?;
    let s = std::fs::read_to_string(&path).ok()?;
    s.trim().parse().ok()
}

/// If a `pr-loop web` server is running for this PR, send it a poke so it
/// immediately re-fetches from GitHub. Best-effort — failures (no server,
/// stale port file, etc.) are silently ignored.
pub fn poke_running_server(pr_context: &PrContext) {
    let Some(port) = read_port_for_pr(pr_context) else {
        return;
    };
    // Short timeout so we don't block the CLI on a dead server.
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!("http://127.0.0.1:{}/api/poke", port);
    let _ = client.post(&url).send();
}

fn ctrlc_cleanup(port_file: PathBuf) -> Result<()> {
    // Best-effort: install a SIGINT handler that removes the port file and exits.
    // We don't add a signal-handling crate; use ctrlc-free approach via libc is overkill,
    // so we rely on the normal exit path in `run()` and the file being overwritten next time.
    let _ = port_file;
    Ok(())
}
