// A tiny fixed-port server that knows how to find your currently-running
// `pr-loop web` instances and redirect or chooser-page you to them.
//
// Run it at login (via a LaunchAgent plist; see `--install`) and you get a
// stable bookmark like http://127.0.0.1:9876/ that always resolves to
// whichever PR view you have open.

use crate::web::{fetch_all_peers, PeerSummary};
use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use tiny_http::{Header, Method, Response, Server};

const LAUNCHD_LABEL: &str = "local.pr-loop.hub";

pub fn run(port: u16) -> Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = TcpListener::bind(addr)
        .with_context(|| format!("Failed to bind 127.0.0.1:{}", port))?;
    let bound = listener.local_addr()?.port();
    let server = Server::from_listener(listener, None)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP server: {}", e))?;

    eprintln!("pr-loop hub: listening on http://127.0.0.1:{}/", bound);

    for request in server.incoming_requests() {
        if let Err(e) = handle(request, bound) {
            eprintln!("hub: request error: {}", e);
        }
    }
    Ok(())
}

fn handle(request: tiny_http::Request, own_port: u16) -> Result<()> {
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();

    if request.method() != &Method::Get || path != "/" {
        let resp = Response::from_string("not found")
            .with_status_code(404)
            .with_header(content_type("text/plain"));
        return request
            .respond(resp)
            .map_err(|e| anyhow::anyhow!("respond: {}", e));
    }

    let peers = fetch_all_peers(own_port);
    match peers.len() {
        0 => {
            let body = render_none_page();
            let resp = Response::from_string(body)
                .with_header(content_type("text/html; charset=utf-8"));
            request
                .respond(resp)
                .map_err(|e| anyhow::anyhow!("respond: {}", e))
        }
        1 => {
            let url = peers[0].url.clone();
            let resp = Response::from_string("")
                .with_status_code(302)
                .with_header(
                    Header::from_bytes(&b"Location"[..], url.as_bytes()).unwrap(),
                );
            request
                .respond(resp)
                .map_err(|e| anyhow::anyhow!("respond: {}", e))
        }
        _ => {
            let body = render_chooser_page(&peers);
            let resp = Response::from_string(body)
                .with_header(content_type("text/html; charset=utf-8"));
            request
                .respond(resp)
                .map_err(|e| anyhow::anyhow!("respond: {}", e))
        }
    }
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
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>pr-loop hub</title>
<style>
body {{ font: 14px -apple-system, BlinkMacSystemFont, sans-serif;
       max-width: 480px; margin: 80px auto; padding: 0 16px; color: #1f2328; }}
h1 {{ font-size: 18px; }}
code {{ background: #f6f8fa; padding: 2px 5px; border-radius: 4px; }}
</style></head><body>
<h1>No <code>pr-loop web</code> instances running</h1>
<p>Run <code>pr-loop web</code> in a PR checkout and refresh this page.</p>
</body></html>"#,
    )
}

fn render_chooser_page(peers: &[PeerSummary]) -> String {
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
            format!(
                r#"<a class="peer" href="{url}">
  <div class="pr">{owner}/{repo} #{num}</div>
  <div class="title">{title}</div>
  <div class="meta">{attn}{unresolved} unresolved</div>
</a>"#,
                url = esc(&p.url),
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

/// Print the launchctl command the user can run to load the LaunchAgent,
/// and write the plist to ~/Library/LaunchAgents. Does NOT load it.
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
    std::fs::write(&plist_path, contents).with_context(|| {
        format!("write plist at {}", plist_path.display())
    })?;
    println!("Wrote {}", plist_path.display());
    println!("Binary: {}", exe.display());
    println!("Log:    {}", log_path.display());
    println!();
    println!("To start it now (and at every login):");
    println!("  launchctl load {}", plist_path.display());
    println!();
    println!("Then open http://127.0.0.1:9876/ — bookmark away.");
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
    println!("To stop the hub now:");
    println!("  launchctl unload {}", plist_path.display());
    println!();
    println!("Then to remove the plist:");
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

