// `pr-loop hub`: the single long-running process. Fixed port, no separate
// `pr-loop web` processes to babysit.
//
// Any `pr-loop` invocation (any subcommand, plus the bare wait modes) pings
// `POST /api/register` on this fixed port right after it resolves its PR
// context — see `notify_register` below, called from `main.rs`. The hub
// keeps an in-process tracker (poll thread + cached state, from `crate::web`)
// per PR that's pinged it within `FRESHNESS_WINDOW`, and tears trackers down
// once they go stale. There's no discovery step and no proxying: the hub
// *is* the server for every tracked PR.
//
//   - Root `/` renders a chooser page listing all currently tracked PRs,
//     each link going to `/pr/<owner>/<repo>/<pr>/`.
//   - `/pr/<owner>/<repo>/<pr>/<rest>` is served in-process by that PR's
//     tracker.
//
// Use one stable URL (http://127.0.0.1:10099/) as your bookmark and never
// chase per-PR processes or ports. With a Tailscale bind address added in
// config, the same URL works from your phone.

use crate::github::PrContext;
use crate::web::{self, PeerInfo, RequestContext, Shared};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Response, Server};

const LAUNCHD_LABEL: &str = "local.pr-loop.hub";

/// How long a tracker survives without a fresh `/api/register` ping before
/// the hub tears it down. "Recently active" per the design: any `pr-loop`
/// invocation (not just long waits) counts, and a wait loop re-pings well
/// inside this window (see `main.rs`), so a PR stays tracked for as long as
/// you're actually working on it.
const FRESHNESS_WINDOW: Duration = Duration::from_secs(10 * 60);
/// How often the reaper sweeps for stale trackers.
const REAP_INTERVAL: Duration = Duration::from_secs(30);

type PrKey = (String, String, u64);

pub fn run(binds: &[String], port: u16) -> Result<()> {
    let bind_list: Vec<String> = if binds.is_empty() {
        vec![crate::config::DEFAULT_BIND.to_string()]
    } else {
        binds.to_vec()
    };

    let mut listeners = Vec::with_capacity(bind_list.len());
    for bind in &bind_list {
        let addr = parse_socket_addr(bind, port)?;
        let listener = TcpListener::bind(addr)
            .with_context(|| format!("bind {}", addr))?;
        listeners.push((bind.clone(), listener));
    }

    let shared = Arc::new(HubShared {
        trackers: Mutex::new(HashMap::new()),
        update_available: Arc::new(AtomicBool::new(false)),
    });

    // Watch our own binary for rebuilds. Canonicalize so we follow symlinks
    // (e.g., ~/.dotfiles/bin/pr-loop → target/release/pr-loop) and watch the
    // file `cargo build` actually rewrites. Best-effort — if any of this
    // fails we just skip the thread. This now covers the whole hub (there's
    // no per-PR process left to restart individually).
    if let Some((exe, mtime)) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|m| (p, m)))
    {
        eprintln!("pr-loop hub: watching binary for rebuilds at {}", exe.display());
        let update_available = Arc::clone(&shared.update_available);
        thread::spawn(move || web::watch_binary_mtime(exe, mtime, update_available));
    } else {
        eprintln!("pr-loop hub: could not resolve binary path — restart detection disabled");
    }

    // Reaper: drop trackers nobody has pinged in a while.
    let shared_reap = Arc::clone(&shared);
    thread::spawn(move || reap_loop(shared_reap));

    let mut handles = Vec::new();
    for (bind, listener) in listeners {
        let server = Server::from_listener(listener, None)
            .map_err(|e| anyhow::anyhow!("Failed to create HTTP server on {}: {}", bind, e))?;
        eprintln!("pr-loop hub: listening on http://{}:{}/", bind, port);
        let shared = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Err(e) = handle(request, &shared) {
                    eprintln!("hub: request error: {}", e);
                }
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn parse_socket_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let s = format!("{}:{}", host, port);
    s.parse::<SocketAddr>()
        .with_context(|| format!("parse bind address {}", s))
}

struct HubShared {
    trackers: Mutex<HashMap<PrKey, Arc<Shared>>>,
    update_available: Arc<AtomicBool>,
}

/// Periodically drop trackers that haven't been re-registered recently, or
/// that we've learned are merged. Merged is permanent (unlike closed, which
/// can be reopened), so it evicts regardless of how recently the PR was
/// pinged — there's nothing left to track.
fn reap_loop(shared: Arc<HubShared>) {
    loop {
        thread::sleep(REAP_INTERVAL);
        let now = Instant::now();
        let mut trackers = shared.trackers.lock().unwrap();
        trackers.retain(|_, t| {
            let fresh = now.duration_since(*t.last_seen.lock().unwrap()) < FRESHNESS_WINDOW;
            let keep = fresh && !t.is_merged();
            if !keep {
                t.request_stop();
            }
            keep
        });
    }
}

/// Snapshot every currently-tracked PR as `PeerInfo`, optionally excluding
/// one (for the in-page "N other pr-loops" widget, which excludes self).
fn snapshot_peers(shared: &HubShared, exclude: Option<&PrKey>) -> Vec<PeerInfo> {
    let trackers = shared.trackers.lock().unwrap();
    let mut peers: Vec<PeerInfo> = trackers
        .iter()
        .filter(|(k, _)| Some(*k) != exclude)
        .map(|(_, t)| web::peer_info(t))
        .collect();
    peers.sort_by(|a, b| {
        a.pr_owner
            .cmp(&b.pr_owner)
            .then_with(|| a.pr_repo.cmp(&b.pr_repo))
            .then_with(|| a.pr_number.cmp(&b.pr_number))
    });
    peers
}

#[derive(serde::Deserialize)]
struct RegisterReq {
    owner: String,
    repo: String,
    pr_number: u64,
    checkout_path: String,
    /// `CLAUDE_PID` from the registering process's environment, if it was
    /// run under Claude Code — see `web::Shared::claude_pid`.
    #[serde(default)]
    claude_pid: Option<u32>,
}

fn handle(mut request: tiny_http::Request, shared: &Arc<HubShared>) -> Result<()> {
    let method = request.method().clone();
    let raw_url = request.url().to_string();
    let path = raw_url.split('?').next().unwrap_or("").to_string();

    if method == Method::Post && path == "/api/register" {
        let mut body_bytes = Vec::new();
        request
            .as_reader()
            .read_to_end(&mut body_bytes)
            .context("read register body")?;
        return match serde_json::from_slice::<RegisterReq>(&body_bytes) {
            Ok(req) => {
                register(shared, req);
                respond_json(request, "{}", 200)
            }
            Err(e) => respond_json(
                request,
                &format!("{{\"error\":\"invalid JSON: {}\"}}", e),
                400,
            ),
        };
    }

    // Root page — always the chooser.
    if method == Method::Get && path == "/" {
        return serve_root(request, shared);
    }

    // Per-PR routes: /pr/<owner>/<repo>/<pr>/<rest>
    if let Some(pr) = parse_pr_path(&path) {
        // Only redirect-to-trailing-slash for the bare PR root like
        // `/pr/owner/repo/1` (no rest). Sub-paths like
        // `/pr/owner/repo/1/api/state` proxy as-is.
        if pr.rest.is_empty() && !pr.has_trailing_slash {
            let loc = format!(
                "/pr/{}/{}/{}/{}",
                pr.owner,
                pr.repo,
                pr.pr_number,
                pr.query(&raw_url)
            );
            return respond_redirect(request, 301, &loc);
        }

        let key = (pr.owner.clone(), pr.repo.clone(), pr.pr_number);
        let tracker = shared.trackers.lock().unwrap().get(&key).cloned();
        let Some(tracker) = tracker else {
            let resp = Response::from_string(format!(
                "No recent pr-loop activity for {}/{} #{}. Run any `pr-loop` command against \
                 it (e.g. `pr-loop checks`) and refresh.",
                pr.owner, pr.repo, pr.pr_number
            ))
            .with_status_code(404)
            .with_header(content_type("text/plain"));
            return request
                .respond(resp)
                .map_err(|e| anyhow::anyhow!("respond: {}", e));
        };

        let peers = snapshot_peers(shared, Some(&key));
        let ctx = RequestContext {
            update_available: shared.update_available.load(Ordering::Relaxed),
            peers: &peers,
        };
        let sub_path = format!("/{}", pr.rest);
        return web::handle_request(request, &sub_path, &tracker, &ctx);
    }

    let resp = Response::from_string("not found")
        .with_status_code(404)
        .with_header(content_type("text/plain"));
    request
        .respond(resp)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))
}

/// Create or refresh a tracker from a `/api/register` ping.
fn register(shared: &Arc<HubShared>, req: RegisterReq) {
    let key = (req.owner.clone(), req.repo.clone(), req.pr_number);
    let mut trackers = shared.trackers.lock().unwrap();
    match trackers.get(&key) {
        Some(t) => {
            *t.checkout_path.lock().unwrap() = PathBuf::from(&req.checkout_path);
            *t.claude_pid.lock().unwrap() = req.claude_pid;
            *t.last_seen.lock().unwrap() = Instant::now();
        }
        None => {
            let pr_context = PrContext {
                owner: req.owner,
                repo: req.repo,
                pr_number: req.pr_number,
            };
            eprintln!(
                "pr-loop hub: now tracking {}/{} #{}",
                pr_context.owner, pr_context.repo, pr_context.pr_number
            );
            let tracker = web::spawn_tracker(
                pr_context,
                PathBuf::from(&req.checkout_path),
                req.claude_pid,
            );
            trackers.insert(key, tracker);
        }
    }
}

fn respond_json(request: tiny_http::Request, body: &str, status: u16) -> Result<()> {
    let resp = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(content_type("application/json"));
    request
        .respond(resp)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRoute {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    /// The rest of the path after `/pr/<owner>/<repo>/<pr>/`, without the
    /// leading slash. Empty if the request is for the root of the instance.
    pub rest: String,
    pub has_trailing_slash: bool,
}

impl PrRoute {
    fn query<'a>(&self, raw_url: &'a str) -> &'a str {
        match raw_url.find('?') {
            Some(i) => &raw_url[i..],
            None => "",
        }
    }
}

/// Parse a request path of the form `/pr/<owner>/<repo>/<pr>[/<rest>]`.
/// Returns None if the path doesn't match the expected shape.
pub fn parse_pr_path(path: &str) -> Option<PrRoute> {
    let stripped = path.strip_prefix("/pr/")?;
    let mut parts = stripped.splitn(4, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let pr_str = parts.next()?;
    if owner.is_empty() || repo.is_empty() || pr_str.is_empty() {
        return None;
    }
    let pr_number: u64 = pr_str.parse().ok()?;
    let (rest, has_trailing_slash) = match parts.next() {
        // `/pr/a/b/1/` → rest "", had the trailing slash
        Some("") => (String::new(), true),
        Some(r) => (r.to_string(), r.ends_with('/')),
        None => (String::new(), false),
    };
    Some(PrRoute {
        owner: owner.to_string(),
        repo: repo.to_string(),
        pr_number,
        rest,
        has_trailing_slash,
    })
}

fn respond_redirect(request: tiny_http::Request, code: u16, location: &str) -> Result<()> {
    let resp = Response::from_string("")
        .with_status_code(code)
        .with_header(Header::from_bytes(&b"Location"[..], location.as_bytes()).unwrap());
    request
        .respond(resp)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))
}

fn serve_root(request: tiny_http::Request, shared: &Arc<HubShared>) -> Result<()> {
    let peers = snapshot_peers(shared, None);
    let body = if peers.is_empty() {
        render_none_page()
    } else {
        render_chooser_page(&peers)
    };
    let resp = Response::from_string(body).with_header(content_type("text/html; charset=utf-8"));
    request
        .respond(resp)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))
}

fn content_type(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).unwrap()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_none_page() -> String {
    r#"<!doctype html><html><head><meta charset="utf-8"><title>pr-loop hub</title>
<style>
body { font: 14px -apple-system, BlinkMacSystemFont, sans-serif;
       max-width: 480px; margin: 80px auto; padding: 0 16px; color: #1f2328; }
h1 { font-size: 18px; }
code { background: #f6f8fa; padding: 2px 5px; border-radius: 4px; }
</style></head><body>
<h1>No recently-active PRs</h1>
<p>Run any <code>pr-loop</code> command in a PR checkout and refresh this page.</p>
</body></html>"#
        .to_string()
}

pub fn render_chooser_page(peers: &[PeerInfo]) -> String {
    let cards = peers
        .iter()
        .map(|p| {
            let title = p.pr_title.as_deref().unwrap_or("");
            let attn = if p.needs_response > 0 {
                format!(
                    r#"<span class="attn">{} need{} reply</span> · "#,
                    p.needs_response,
                    if p.needs_response == 1 { "s" } else { "" }
                )
            } else {
                String::new()
            };
            let href = format!(
                "/pr/{}/{}/{}/",
                esc(&p.pr_owner),
                esc(&p.pr_repo),
                p.pr_number
            );
            format!(
                r#"<a class="peer" href="{href}">
  <div class="pr">{owner}/{repo} #{num}</div>
  <div class="title">{title}</div>
  <div class="meta">{attn}{unresolved} unresolved</div>
</a>"#,
                href = href,
                owner = esc(&p.pr_owner),
                repo = esc(&p.pr_repo),
                num = p.pr_number,
                title = esc(title),
                attn = attn,
                unresolved = p.unresolved_threads,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>pr-loop hub</title>
<style>
body {{ font: 14px -apple-system, BlinkMacSystemFont, sans-serif;
       max-width: 560px; margin: 48px auto; padding: 0 16px; color: #1f2328; }}
h1 {{ font-size: 18px; margin-bottom: 16px; }}
.peer {{ display: block; padding: 12px 14px; border: 1px solid #d0d7de;
         border-radius: 6px; margin-bottom: 10px; text-decoration: none; color: inherit; }}
.peer:hover {{ border-color: #0969da; }}
.pr {{ color: #656d76; font-size: 12px; }}
.title {{ font-weight: 600; margin-top: 2px; }}
.meta {{ margin-top: 4px; font-size: 12px; color: #656d76; }}
.attn {{ color: #cf222e; font-weight: 600; }}
</style></head><body>
<h1>Recently active PRs</h1>
{cards}
</body></html>"#,
    )
}

// -- Client-side helpers: any `pr-loop` invocation calls these -------------

/// Best-effort: tell the hub (assumed to be at the configured/default port
/// on 127.0.0.1 — no discovery, per design) that `pr-loop` was just run
/// against this PR from `checkout_path`. Creates the tracker if this is the
/// first ping, or just refreshes its checkout path + freshness clock.
///
/// Also reads `CLAUDE_PID` from our own environment and forwards it, if
/// set — Claude Code sets it on every subprocess it spawns, including the
/// Bash tool's shell, so this is automatic: no flag for the agent to pass,
/// nothing to get wrong by `cd`ing somewhere before running `pr-loop`. See
/// `web::Shared::claude_pid`.
///
/// Returns false if the hub couldn't be reached at all (e.g. not running).
/// Callers must never fail the command over this — it's purely so the web UI
/// picks the PR up; a non-fatal notice is the right response to `false`.
pub fn notify_register(pr_context: &PrContext, checkout_path: &Path) -> bool {
    let port = crate::config::load().hub_port();
    let url = format!("http://127.0.0.1:{}/api/register", port);
    let claude_pid: Option<u32> = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|s| s.parse().ok());
    let body = serde_json::json!({
        "owner": pr_context.owner,
        "repo": pr_context.repo,
        "pr_number": pr_context.pr_number,
        "checkout_path": checkout_path.to_string_lossy(),
        "claude_pid": claude_pid,
    });
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    client
        .post(&url)
        .json(&body)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Best-effort: if the hub has a tracker for this PR, ask it to refetch from
/// GitHub immediately, so a UI open on it updates right away after e.g.
/// `pr-loop reply`. Silently does nothing if the hub or the tracker isn't up
/// — same "don't fail the command over this" rule as `notify_register`.
pub fn poke(pr_context: &PrContext) -> bool {
    let port = crate::config::load().hub_port();
    let url = format!(
        "http://127.0.0.1:{}/pr/{}/{}/{}/api/poke",
        port, pr_context.owner, pr_context.repo, pr_context.pr_number
    );
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    client
        .post(&url)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// -- LaunchAgent install/uninstall (unchanged behavior) ----------------------

pub fn install() -> Result<()> {
    let plist_path = plist_path()?;
    let exe = std::env::current_exe().context("current_exe")?;
    let log_path = log_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).context("create LaunchAgents dir")?;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).context("create log dir")?;
    }
    let contents = render_plist(&exe, &log_path);
    std::fs::write(&plist_path, contents)
        .with_context(|| format!("write plist at {}", plist_path.display()))?;
    println!("Wrote {}", plist_path.display());
    println!("Binary: {}", exe.display());
    println!("Log:    {}", log_path.display());
    println!();
    println!("Bind addresses and port are picked up from ~/.config/pr-loop/config.toml.");
    println!("Example:");
    println!();
    println!("  [hub]");
    println!("  bind = [\"127.0.0.1\", \"100.64.1.2\"]  # add your tailnet IP");
    println!();
    println!("The plist is in place, so launchd will start the hub at your");
    println!("next login. To start it right now without logging out:");
    println!();
    println!("  If it's not already bootstrapped:");
    println!("    launchctl bootstrap gui/$UID {}", plist_path.display());
    println!("  If it's already running and you just changed this plist");
    println!("  (e.g. rerunning --install after an update): `kickstart` alone");
    println!("  restarts the process but won't pick up plist-level changes");
    println!("  like PATH — bootout then bootstrap again:");
    println!("    launchctl bootout gui/$UID/{}", LAUNCHD_LABEL);
    println!("    launchctl bootstrap gui/$UID {}", plist_path.display());
    println!();
    println!("Then open http://127.0.0.1:10099/ and bookmark it.");
    println!();
    println!("Note: the plist pins the binary to its current path. If you");
    println!("move or rebuild somewhere else, rerun `pr-loop hub --install`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let plist_path = plist_path()?;
    if !plist_path.exists() {
        println!("Nothing to uninstall — {} does not exist.", plist_path.display());
        return Ok(());
    }
    println!("To stop the hub right now (for this session):");
    println!("  launchctl bootout gui/$UID/{}", LAUNCHD_LABEL);
    println!();
    println!("To prevent it from starting again at next login, delete the plist:");
    println!("  rm {}", plist_path.display());
    println!();
    println!("(Not running these for you — up to you to confirm.)");
    Ok(())
}

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", LAUNCHD_LABEL)))
}

fn log_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Logs/pr-loop-hub.log"))
}

/// launchd hands agents a bare-bones default PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) — enough for `git` (Xcode CLT), but not
/// for `gh`, which is virtually always a Homebrew install. The hub shells
/// out to both, so bake Homebrew's bin dirs in (both Apple Silicon and Intel
/// locations, harmlessly — a nonexistent PATH entry is just skipped) rather
/// than relying on whatever shell happened to run `--install`.
const LAUNCHD_PATH: &str =
    "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin";

fn render_plist(exe: &std::path::Path, log: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>hub</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        log = log.display(),
        path = LAUNCHD_PATH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_root() {
        let pr = parse_pr_path("/pr/owner/repo/1234/").unwrap();
        assert_eq!(pr.owner, "owner");
        assert_eq!(pr.repo, "repo");
        assert_eq!(pr.pr_number, 1234);
        assert_eq!(pr.rest, "");
        assert!(pr.has_trailing_slash);
    }

    #[test]
    fn parses_pr_root_without_trailing_slash() {
        let pr = parse_pr_path("/pr/owner/repo/1234").unwrap();
        assert_eq!(pr.pr_number, 1234);
        assert_eq!(pr.rest, "");
        assert!(!pr.has_trailing_slash);
    }

    #[test]
    fn parses_pr_with_subpath() {
        let pr = parse_pr_path("/pr/a/b/1/api/state").unwrap();
        assert_eq!(pr.rest, "api/state");
        assert!(!pr.has_trailing_slash);
    }

    #[test]
    fn parses_pr_with_subdir_trailing_slash() {
        let pr = parse_pr_path("/pr/a/b/1/foo/").unwrap();
        assert_eq!(pr.rest, "foo/");
        assert!(pr.has_trailing_slash);
    }

    #[test]
    fn rejects_missing_segments() {
        assert!(parse_pr_path("/pr").is_none());
        assert!(parse_pr_path("/pr/owner").is_none());
        assert!(parse_pr_path("/pr/owner/repo").is_none());
        assert!(parse_pr_path("/pr//repo/1").is_none());
        assert!(parse_pr_path("/pr/owner//1").is_none());
    }

    #[test]
    fn rejects_non_numeric_pr() {
        assert!(parse_pr_path("/pr/a/b/xyz/").is_none());
    }

    #[test]
    fn rejects_non_pr_paths() {
        assert!(parse_pr_path("/").is_none());
        assert!(parse_pr_path("/api/state").is_none());
        assert!(parse_pr_path("/xy/a/b/1/").is_none());
    }

    #[test]
    fn chooser_uses_proxy_paths() {
        let peers = vec![PeerInfo {
            pr_owner: "a".into(),
            pr_repo: "b".into(),
            pr_number: 7,
            pr_title: Some("hello".into()),
            unresolved_threads: 2,
            needs_response: 1,
        }];
        let html = render_chooser_page(&peers);
        assert!(html.contains(r#"href="/pr/a/b/7/""#));
        assert!(html.contains("hello"));
        assert!(html.contains("1 needs reply"));
    }

    #[test]
    fn none_page_is_non_empty() {
        let html = render_none_page();
        assert!(html.contains("No"));
        assert!(html.contains("pr-loop"));
    }

    #[test]
    fn plist_sets_path_for_homebrew_gh() {
        // launchd's default PATH is just /usr/bin:/bin:/usr/sbin:/sbin,
        // which is enough for `git` but not a Homebrew-installed `gh` — the
        // hub shells out to both, so the plist must set PATH explicitly.
        let plist = render_plist(
            std::path::Path::new("/usr/local/bin/pr-loop"),
            std::path::Path::new("/tmp/pr-loop-hub.log"),
        );
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("/opt/homebrew/bin"));
        assert!(plist.contains("/usr/local/bin"));
    }
}
