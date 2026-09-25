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
// They run as part of the default `cargo test` — this project has no CI and
// isn't developed often enough for a fast/offline default to matter more
// than just running everything every time.

use super::*;
use headless_chrome::protocol::cdp::Input;
use headless_chrome::{Browser, Element, Tab};
use std::net::TcpListener;

/// `navigate_to` + `wait_for_element` in one call, retrying the whole
/// navigation a couple of times on failure. `headless_chrome`/CDP session
/// setup has some inherent one-shot flakiness around a freshly created tab's
/// very first navigation (independent of anything in the app — observed
/// with no JS exception and no failed network request on the failing
/// attempt), so a fresh `navigate_to` retry is more reliable here than
/// leaning harder on a single `wait_for_element`'s internal polling.
fn navigate_and_wait_for<'a>(tab: &'a Tab, url: &str, selector: &str) -> Element<'a> {
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(300));
        }
        tab.navigate_to(url).expect("navigate");
        match tab.wait_for_element(selector) {
            Ok(el) => return el,
            Err(e) => last_err = Some(e),
        }
    }
    panic!(
        "never found {selector:?} at {url} after retries: {:?}",
        last_err.unwrap()
    );
}

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

fn pr_dto(state: &'static str, is_draft: bool, is_in_merge_queue: bool) -> PrDto {
    PrDto {
        owner: "glasser".to_string(),
        repo: "pr-loop-test-repo".to_string(),
        pr_number: 1,
        title: Some("Test PR".to_string()),
        url: None,
        state,
        is_draft,
        is_in_merge_queue,
    }
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
    navigate_and_wait_for(&tab, &base, ".edit-btn")
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

/// The badge is pure rendering logic (github.com-style icon/label from
/// `pr.state` + `is_draft` + `is_in_merge_queue`), so this could in principle
/// be a Rust unit test if that logic lived in Rust — it doesn't, it's inline
/// JS in index.html, so a browser is the only way to actually exercise it.
#[test]
fn pr_status_badge_renders_for_each_state() {
    let browser = Browser::default().expect("launch/fetch Chromium");

    let cases = [
        ("open", false, false, "open", "Open"),
        ("open", true, false, "draft", "Draft"),
        ("open", false, true, "queued", "Queued to merge"),
        ("closed", false, false, "closed", "Closed"),
        ("merged", false, false, "merged", "Merged"),
    ];

    for (pr_state, is_draft, is_in_merge_queue, expected_class, expected_label) in cases {
        let base = start_test_server(State {
            pr: Some(pr_dto(pr_state, is_draft, is_in_merge_queue)),
            ..Default::default()
        });

        let tab = browser.new_tab().expect("open tab");
        let badge = navigate_and_wait_for(&tab, &base, ".pr-status");

        let class = badge.get_attribute_value("class").unwrap().unwrap_or_default();
        let text = badge.get_inner_text().unwrap();

        assert!(
            class.split(' ').any(|c| c == expected_class),
            "state={pr_state} draft={is_draft} queued={is_in_merge_queue}: class was {class:?}, expected to contain {expected_class:?}"
        );
        assert!(
            text.contains(expected_label),
            "state={pr_state} draft={is_draft} queued={is_in_merge_queue}: text was {text:?}, expected to contain {expected_label:?}"
        );

        tab.close(false).ok();
    }
}

/// Escape is not subject to the OS key-composition quirk that
/// `option_q_rewraps_paragraph_and_confirms_hooks_work` guards against, so
/// the crate's high-level `press_key` (unlike a Mac Option+letter) is a
/// faithful stand-in for a real keypress here.
#[test]
fn escape_closes_reword_modal_without_saving() {
    let base = start_test_server(one_commit_state("Some commit body."));

    let browser = Browser::default().expect("launch/fetch Chromium");
    let tab = browser.new_tab().expect("open tab");
    navigate_and_wait_for(&tab, &base, ".edit-btn")
        .click()
        .expect("click edit button");
    let textarea = tab
        .wait_for_element(".reword-modal textarea")
        .expect("modal should open");
    textarea.focus().expect("focus textarea");

    tab.press_key("Escape").expect("press Escape");
    thread::sleep(Duration::from_millis(150));

    let modal_gone: bool = tab
        .evaluate("!document.querySelector('.reword-modal')", false)
        .expect("evaluate")
        .value
        .and_then(|v| v.as_bool())
        .expect("boolean result");
    assert!(modal_gone, "Escape should close the reword modal");

    // No pending-reword request should have been created — Escape cancels,
    // it doesn't submit. (No network call happens either way here since we
    // never clicked "Request reword", but this also guards against a future
    // change accidentally wiring Escape to submit.)
    let pending_reword_shown: bool = tab
        .evaluate("!!document.querySelector('.reword-pending')", false)
        .expect("evaluate")
        .value
        .and_then(|v| v.as_bool())
        .expect("boolean result");
    assert!(!pending_reword_shown, "Escape must not create a reword request");
}

/// Confirms the vendored markdown-it + markdown-it-emoji + highlight.js
/// bundle actually still renders correctly end-to-end (fenced code gets
/// syntax-highlighted, emoji shortcodes get replaced) — nothing else tests
/// that these libraries still *work*, only that their files are servable
/// (`vendor_assets_are_served`).
#[test]
fn comment_body_renders_markdown_emoji_and_code_highlighting() {
    let state = State {
        pr: Some(pr_dto("open", false, false)),
        threads: vec![ThreadDto {
            id: "thread-1".to_string(),
            is_resolved: false,
            is_outdated: false,
            is_paperclip: false,
            is_in_progress: false,
            started_by_me: true,
            path: None,
            line: None,
            comments: vec![CommentDto {
                id: "comment-1".to_string(),
                author: "octocat".to_string(),
                body: "Nice work :tada:\n\n```rust\nfn main() {}\n```".to_string(),
                diff_hunk: None,
                url: None,
                created_at: None,
            }],
        }],
        ..Default::default()
    };
    let base = start_test_server(state);

    let browser = Browser::default().expect("launch/fetch Chromium");
    let tab = browser.new_tab().expect("open tab");
    let body = navigate_and_wait_for(&tab, &base, ".comment-body");
    let html = body.get_content().expect("get rendered HTML");

    assert!(
        html.contains('🎉'),
        "markdown-it-emoji should replace :tada: with 🎉 — got:\n{html}"
    );
    assert!(
        html.contains("hljs language-rust"),
        "fenced rust code should be routed through the highlight.js fence renderer — got:\n{html}"
    );
    assert!(
        html.contains("hljs-"),
        "highlight.js should emit at least one hljs-* token span — got:\n{html}"
    );
}

fn thread_dto(id: &str, started_by_me: bool, author: &str) -> ThreadDto {
    ThreadDto {
        id: id.to_string(),
        is_resolved: false,
        is_outdated: false,
        is_paperclip: false,
        is_in_progress: false,
        started_by_me,
        path: Some("src/main.rs".to_string()),
        line: Some(1),
        comments: vec![CommentDto {
            id: format!("{id}-c1"),
            author: author.to_string(),
            body: "Please take a look at this".to_string(),
            diff_hunk: None,
            url: None,
            created_at: None,
        }],
    }
}

/// Regression test for the "only mine by default" thread filter: threads
/// someone else (or a bot) started shouldn't clutter the default view, but
/// must still be reachable via the "show all" checkbox.
#[test]
fn mine_only_filter_hides_threads_started_by_others_until_toggled() {
    let base = start_test_server(State {
        pr: Some(pr_dto("open", false, false)),
        threads: vec![
            thread_dto("mine", true, "glasser"),
            thread_dto("bots", false, "coderabbitai"),
        ],
        ..Default::default()
    });

    let browser = Browser::default().expect("launch/fetch Chromium");
    let tab = browser.new_tab().expect("open tab");
    navigate_and_wait_for(&tab, &base, ".thread");

    let thread_count = |tab: &Tab| -> i64 {
        tab.evaluate("document.querySelectorAll('.thread').length", false)
            .expect("evaluate")
            .value
            .and_then(|v| v.as_i64())
            .expect("count is a number")
    };
    assert_eq!(
        thread_count(&tab),
        1,
        "only the thread the authenticated user started should show by default"
    );

    tab.wait_for_element(".thread-filter-bar input[type=checkbox]")
        .expect("filter checkbox")
        .click()
        .expect("click checkbox");
    thread::sleep(Duration::from_millis(150));

    assert_eq!(
        thread_count(&tab),
        2,
        "checking \"show all\" should reveal the bot-started thread too"
    );
}

/// Regression test for the empty-state clean-up prompt: when nothing needs
/// review but `clean-threads` would still delete stale pure-Claude threads,
/// the UI offers a two-click confirm. Deliberately never completes the
/// second click here — that would hit `/api/clean-threads`, which deletes
/// real GitHub comments and has no place in an automated test.
#[test]
fn empty_state_clean_up_button_requires_a_second_click_to_confirm() {
    let base = start_test_server(State {
        pr: Some(pr_dto("open", false, false)),
        threads: vec![],
        cleanable_thread_count: 2,
        // The empty-state cleanup prompt only renders once a GitHub fetch
        // has actually completed — otherwise the UI shows "Waiting for
        // first GitHub fetch…" instead.
        last_fetched_at: Some("2024-01-01T00:00:00Z".to_string()),
        ..Default::default()
    });

    let browser = Browser::default().expect("launch/fetch Chromium");
    let tab = browser.new_tab().expect("open tab");
    let button = navigate_and_wait_for(&tab, &base, ".cleanup-prompt button.danger");
    assert_eq!(button.get_inner_text().unwrap().trim(), "Clean up 2");

    button.click().expect("first click arms confirmation");
    thread::sleep(Duration::from_millis(150));
    let armed = tab
        .wait_for_element(".cleanup-prompt button.danger")
        .expect("button still present after arming");
    assert_eq!(armed.get_inner_text().unwrap().trim(), "Really delete 2?");

    let cancel = tab
        .wait_for_element(".cleanup-prompt button:not(.danger)")
        .expect("cancel button appears once armed");
    assert_eq!(cancel.get_inner_text().unwrap().trim(), "Cancel");
    cancel.click().expect("click cancel");
    thread::sleep(Duration::from_millis(150));

    let disarmed = tab
        .wait_for_element(".cleanup-prompt button.danger")
        .expect("button still present after cancel");
    assert_eq!(disarmed.get_inner_text().unwrap().trim(), "Clean up 2");
}
