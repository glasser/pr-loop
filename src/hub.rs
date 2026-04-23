// Fixed-port reverse proxy that fronts your running `pr-loop web` instances.
//
// Each pr-loop web binds to a random port on 127.0.0.1. The hub enumerates
// those (via ~/Library/Caches/pr-loop/web-*.port) and:
//   - Root `/` renders a chooser page listing all currently running instances,
//     each link going to `/pr/<owner>/<repo>/<pr>/`.
//   - Any request under `/pr/<owner>/<repo>/<pr>/<rest>` is proxied to
//     http://127.0.0.1:<port>/<rest> for that specific instance.
//
// Use one stable URL (http://127.0.0.1:10099/) as your bookmark and never
// chase random ports. With a Tailscale bind address added in config, the
// same URL works from your phone.

use crate::web::{fetch_all_peers, read_port, PeerSummary};
use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Response, Server};

const LAUNCHD_LABEL: &str = "local.pr-loop.hub";

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

    let shared = Arc::new(HubShared { port });

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
    port: u16,
}

fn handle(request: tiny_http::Request, shared: &Arc<HubShared>) -> Result<()> {
    let method = request.method().clone();
    let raw_url = request.url().to_string();
    let path = raw_url.split('?').next().unwrap_or("").to_string();

    // Root page — always the chooser.
    if method == Method::Get && path == "/" {
        return serve_root(request, shared.port);
    }

    // Proxy routes: /pr/<owner>/<repo>/<pr>/<rest>
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
        return proxy_request(request, &pr);
    }

    let resp = Response::from_string("not found")
        .with_status_code(404)
        .with_header(content_type("text/plain"));
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

/// Forward a request to the pr-loop web instance serving the given PR.
fn proxy_request(mut request: tiny_http::Request, pr: &PrRoute) -> Result<()> {
    let Some(backend_port) = read_port(&pr.owner, &pr.repo, pr.pr_number) else {
        let resp = Response::from_string(format!(
            "No pr-loop web running for {}/{} #{}.",
            pr.owner, pr.repo, pr.pr_number
        ))
        .with_status_code(404)
        .with_header(content_type("text/plain"));
        return request
            .respond(resp)
            .map_err(|e| anyhow::anyhow!("respond: {}", e));
    };

    // Read the request body (if any) before we lose access to the reader.
    let mut body_bytes = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body_bytes)
        .context("read proxied request body")?;

    let raw_url = request.url().to_string();
    let query = match raw_url.find('?') {
        Some(i) => &raw_url[i..],
        None => "",
    };
    let target_url = format!(
        "http://127.0.0.1:{}/{}{}",
        backend_port, pr.rest, query
    );

    // Forward the request with a short-ish timeout — backend is local.
    let method_bytes = request.method().as_str().as_bytes().to_vec();
    let method =
        reqwest::Method::from_bytes(&method_bytes).context("unsupported HTTP method")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut req_builder = client.request(method, &target_url);

    // Forward request headers, dropping hop-by-hop + Host (reqwest sets Host).
    for h in request.headers() {
        let name = h.field.as_str().as_str().to_ascii_lowercase();
        if is_hop_by_hop(&name) || name == "host" || name == "content-length" {
            continue;
        }
        req_builder = req_builder.header(h.field.as_str().as_str(), h.value.as_str());
    }
    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes);
    }

    let resp = match req_builder.send() {
        Ok(r) => r,
        Err(e) => {
            let r = Response::from_string(format!("proxy error: {}", e))
                .with_status_code(502)
                .with_header(content_type("text/plain"));
            return request
                .respond(r)
                .map_err(|e| anyhow::anyhow!("respond: {}", e));
        }
    };

    let status_code = resp.status().as_u16();

    // Preserve response headers except hop-by-hop, content-length (tiny_http
    // writes its own), and transfer-encoding.
    let mut forwarded_headers: Vec<Header> = Vec::new();
    for (name, value) in resp.headers().iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&lname) || lname == "content-length" || lname == "transfer-encoding" {
            continue;
        }
        if let (Ok(v), Ok(h)) = (
            value.to_str(),
            Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()),
        ) {
            let _ = v;
            forwarded_headers.push(h);
        }
    }

    let body = resp.bytes().unwrap_or_default();
    let body_len = body.len();
    let mut out = Response::from_data(body.to_vec()).with_status_code(status_code);
    for h in forwarded_headers {
        out = out.with_header(h);
    }
    let _ = body_len;
    request
        .respond(out)
        .map_err(|e| anyhow::anyhow!("respond: {}", e))
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "upgrade"
    )
}

fn serve_root(request: tiny_http::Request, own_port: u16) -> Result<()> {
    let peers = fetch_all_peers(own_port);
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
<h1>No <code>pr-loop web</code> instances running</h1>
<p>Run <code>pr-loop web</code> in a PR checkout and refresh this page.</p>
</body></html>"#
        .to_string()
}

pub fn render_chooser_page(peers: &[PeerSummary]) -> String {
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
            // Use a proxy-relative URL: /pr/<owner>/<repo>/<pr>/
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
<h1>Running pr-loop web instances</h1>
{cards}
</body></html>"#,
    )
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
    println!("next login. To start it right now without logging out, run:");
    println!();
    println!("  launchctl bootstrap gui/$UID {}", plist_path.display());
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
        let peers = vec![PeerSummary {
            port: 12345,
            url: "http://127.0.0.1:12345/".to_string(),
            pr_owner: "a".into(),
            pr_repo: "b".into(),
            pr_number: 7,
            pr_title: Some("hello".into()),
            pr_url: None,
            unresolved_threads: 2,
            needs_response: 1,
            last_commit_at: None,
            last_comment_at: None,
            unreachable: false,
        }];
        let html = render_chooser_page(&peers);
        assert!(html.contains(r#"href="/pr/a/b/7/""#));
        // Should NOT contain the direct localhost URL as a link.
        assert!(!html.contains(r#"href="http://127.0.0.1:12345/""#));
        assert!(html.contains("hello"));
        assert!(html.contains("1 needs reply"));
    }

    #[test]
    fn none_page_is_non_empty() {
        let html = render_none_page();
        assert!(html.contains("No"));
        assert!(html.contains("pr-loop web"));
    }
}
