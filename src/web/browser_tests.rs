// Browser-level tests for the web UI, using a real (headless) Chrome via the
// `headless_chrome` crate instead of just unit-testing Rust functions. These
// exist because some bugs only show up in actual DOM/keyboard-event
// handling — see `option_q_rewraps_paragraph_and_confirms_hooks_work` below,
// which is a regression test for exactly the kind of bug that motivated this
// file: a Mac Option+Q shortcut that silently typed "œ" instead of firing,
// because macOS remaps `KeyboardEvent.key` under Option to a composed
// character. That bug was invisible to `cargo test` before this file existed.
//
// `headless_chrome`'s `fetch` Cargo feature (see Cargo.toml) downloads and
// caches a pinned "known good" Chromium build the first time it's needed, so
// these tests don't depend on whatever Chrome happens to be on the machine.
// They're marked `#[ignore]` and excluded from the default `cargo test` run
// (which stays fast and fully offline) — run them explicitly with:
//
//   cargo test --test '*' -- --ignored   # or just target this module:
//   cargo test browser_tests -- --ignored

use super::*;
use headless_chrome::protocol::cdp::Input;
use headless_chrome::Browser;
use std::net::TcpListener;

/// Serves `state` from a fresh in-process HTTP server (no GitHub calls — the
/// state is a fixture, and there's no poll loop) and returns its base URL.
/// The server thread runs for the rest of the process's life, which is fine:
/// each test binds an OS-assigned port, and the test process exits when done.
fn start_test_server(state: State) -> String {
    let shared = Arc::new(Shared {
        pr_context: PrContext {
            owner: "glasser".to_string(),
            repo: "pr-loop-test-repo".to_string(),
            pr_number: 1,
        },
        state: Mutex::new(state),
        trigger: (Mutex::new(false), Condvar::new()),
        checkout_path: Mutex::new(PathBuf::from("/tmp")),
        claude_pid: Mutex::new(None),
        last_seen: Mutex::new(Instant::now()),
        merged: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let port = listener.local_addr().expect("local_addr").port();
    let server = tiny_http::Server::from_listener(listener, None).expect("tiny_http server");

    thread::spawn(move || {
        let empty_peers: Vec<PeerInfo> = Vec::new();
        for request in server.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("/").to_string();
            let ctx = RequestContext {
                update_available: false,
                peers: &empty_peers,
            };
            let _ = handle_request(request, &path, &shared, &ctx);
        }
    });

    format!("http://127.0.0.1:{port}/")
}

fn one_commit_state(message_body: &str) -> State {
    State {
        pr: Some(PrDto {
            owner: "glasser".to_string(),
            repo: "pr-loop-test-repo".to_string(),
            pr_number: 1,
            title: Some("Test PR".to_string()),
            url: None,
            state: "open",
            is_draft: false,
            is_in_merge_queue: false,
        }),
        commits: vec![CommitDto {
            sha: "abc123def456".to_string(),
            abbreviated_sha: "abc123d".to_string(),
            message_headline: "Headline unaffected by rewrap".to_string(),
            message_body_first_line: None,
            message_body: message_body.to_string(),
            committed_date: "2024-01-01T00:00:00Z".to_string(),
            author_name: None,
            author_login: None,
            url: "https://example.com/commit/abc123def456".to_string(),
            pending_reword: None,
        }],
        ..Default::default()
    }
}

/// Fast, no-browser check that every asset `index.html` references under
/// `vendor/` is actually served (and with a sane content type) — the failure
/// mode being a typo'd path or a route that never got wired up in
/// `handle_request`, which the browser tests below wouldn't distinguish from
/// "no Chrome available in this environment".
#[test]
fn vendor_assets_are_served() {
    let base = start_test_server(State::default());

    let paths = [
        ("vendor/preact.mjs", "text/javascript"),
        ("vendor/hooks.mjs", "text/javascript"),
        ("vendor/htm.mjs", "text/javascript"),
        ("vendor/markdown-it.mjs", "text/javascript"),
        ("vendor/markdown-it-emoji.mjs", "text/javascript"),
        ("vendor/highlightjs.mjs", "text/javascript"),
        ("vendor/github-markdown.min.css", "text/css"),
        ("vendor/highlightjs-github.min.css", "text/css"),
    ];

    for (path, expected_content_type) in paths {
        let resp = reqwest::blocking::get(format!("{base}{path}")).expect("request");
        assert_eq!(resp.status(), 200, "GET {path}");
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with(expected_content_type),
            "{path} content-type was {content_type:?}, expected to start with {expected_content_type:?}"
        );
    }
}

/// Regression test for the Option+Q Mac bug: dispatches the exact CDP event
/// sequence real macOS sends for Option+Q (code=KeyQ, a composed "char" event
/// with key/text="œ") rather than the crate's `press_key_with_modifiers`,
/// whose static US-keyboard-layout table doesn't model macOS's Option-key
/// character composition — it would send key="q" and pass against both the
/// buggy and the fixed code, proving nothing.
///
/// Along the way this also confirms preact/hooks actually work end-to-end
/// against the vendored (not CDN) build: opening the modal is driven by
/// `useState`, so a hooks regression (e.g. `preact` and `preact/hooks`
/// resolving to two separate module instances) would show up as the modal
/// simply never appearing.
#[test]
#[ignore = "spawns a real headless Chrome; run with `cargo test browser_tests -- --ignored`"]
fn option_q_rewraps_paragraph_and_confirms_hooks_work() {
    let base = start_test_server(one_commit_state(
        "Second paragraph here, also fairly long, to make sure paragraph boundary \
         detection works correctly when there is more than one paragraph in the \
         message body.",
    ));

    let browser = Browser::default().expect(
        "launch/fetch Chromium — see the `fetch` feature on the headless_chrome dev-dependency",
    );
    let tab = browser.new_tab().expect("open tab");
    tab.navigate_to(&base).expect("navigate");

    // Deliberately not `wait_until_navigated()` first — in practice that call
    // itself proved to be the flaky part (its own event-based wait can hang
    // even on a successful load). `wait_for_element` already retries on its
    // own for up to its timeout, which covers the same "page not loaded yet"
    // case more reliably.
    tab.wait_for_element(".edit-btn")
        .expect("edit button should render")
        .click()
        .expect("click edit button");

    let textarea = tab
        .wait_for_element(".reword-modal textarea")
        .expect("modal textarea should appear (useState-driven — a hooks regression breaks this)");
    textarea.focus().expect("focus textarea");

    // Place the cursor inside the body paragraph (not the headline).
    textarea
        .call_js_fn(
            "function() { const i = this.value.indexOf('Second paragraph'); this.setSelectionRange(i + 3, i + 3); }",
            vec![],
            false,
        )
        .expect("place cursor");

    // rawKeyDown, char (the composed "œ"), keyUp — the same three-event
    // sequence real macOS sends for Option+Q. modifiers: 1 = Alt (per the
    // CDP Input.dispatchKeyEvent spec).
    tab.call_method(Input::DispatchKeyEvent {
        Type: Input::DispatchKeyEventTypeOption::RawKeyDown,
        modifiers: Some(1),
        code: Some("KeyQ".to_string()),
        windows_virtual_key_code: Some(81),
        native_virtual_key_code: Some(81),
        key: None,
        text: None,
        unmodified_text: None,
        key_identifier: None,
        timestamp: None,
        auto_repeat: None,
        is_keypad: None,
        is_system_key: None,
        location: None,
        commands: None,
    })
    .expect("dispatch rawKeyDown");
    tab.call_method(Input::DispatchKeyEvent {
        Type: Input::DispatchKeyEventTypeOption::Char,
        modifiers: Some(1),
        code: Some("KeyQ".to_string()),
        key: Some("œ".to_string()),
        text: Some("œ".to_string()),
        unmodified_text: Some("q".to_string()),
        windows_virtual_key_code: None,
        native_virtual_key_code: None,
        timestamp: None,
        key_identifier: None,
        auto_repeat: None,
        is_keypad: None,
        is_system_key: None,
        location: None,
        commands: None,
    })
    .expect("dispatch char");
    tab.call_method(Input::DispatchKeyEvent {
        Type: Input::DispatchKeyEventTypeOption::KeyUp,
        modifiers: Some(1),
        code: Some("KeyQ".to_string()),
        key: Some("œ".to_string()),
        windows_virtual_key_code: Some(81),
        native_virtual_key_code: Some(81),
        text: None,
        unmodified_text: None,
        key_identifier: None,
        timestamp: None,
        auto_repeat: None,
        is_keypad: None,
        is_system_key: None,
        location: None,
        commands: None,
    })
    .expect("dispatch keyUp");

    thread::sleep(Duration::from_millis(200));

    let value: serde_json::Value = textarea
        .call_js_fn("function() { return this.value }", vec![], false)
        .expect("read textarea value")
        .value
        .expect("value present");
    let value = value.as_str().expect("value is a string").to_string();

    assert!(
        !value.contains('œ'),
        "Option+Q must not type œ into the textarea — got:\n{value}"
    );
    assert!(
        value.contains("Second paragraph here, also fairly long"),
        "paragraph content should survive the rewrap — got:\n{value}"
    );
    assert!(
        value.lines().all(|l| l.chars().count() <= 72),
        "rewrapped lines should fit in 72 columns — got:\n{value}"
    );
}
