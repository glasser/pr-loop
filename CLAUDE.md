# Agent Instructions

This project is **pr-loop**, a Rust CLI tool that helps Claude Code manage PR workflows. It analyzes PR state (CI checks, review threads) and recommends next actions.

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Project Structure

This is a standard single-crate Rust project:
- `Cargo.toml` and `src/` at the repo root
- Release binary: `target/release/pr-loop`
- Skills in `.claude/skills/` for Claude Code integration

## Development Ground Rules

### GitHub API Usage
- **Automated tests MUST NOT talk to real GitHub.** Use dependency injection with test implementations.
- **Manual testing only** against the designated test repo: `glasser/pr-loop-test-repo`
- Do NOT interact with any other GitHub repos during development unless explicitly authorized.

### Testing Requirements
- High level of automatically-enforced test coverage is required.
- Design with dependency injection: API clients should be traits with real and test implementations.
- Use recorded fixtures or constructed test data, not live API calls in tests.
- Manual QA against the test repo informs what mock behavior to implement.
- Browser-level web UI tests (`src/web/browser_tests.rs`, using `headless_chrome`) run as part of the
  default `cargo test` — some bugs (e.g. keyboard-event handling) only show up in a real DOM/browser,
  not in Rust unit tests. First run downloads and caches a pinned Chromium build (the `fetch` feature),
  and each run takes ~20-30s longer than the rest of the suite since it launches a real browser
  several times. That tradeoff is fine here: no CI, and the project isn't developed often enough for
  a fast/offline default to matter more than just running everything every time.

### Build Requirements
- **Always build release when completing work**: `cargo build --release`
- The user runs the tool directly from the release binary
- Run `cargo test` to verify all tests pass before committing

## Quick Reference

```bash
# Development
cargo build --release  # Build release binary (REQUIRED before finishing)
cargo test             # Run all tests

# Issue tracking
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --status in_progress  # Claim work
bd close <id>         # Complete work
bd sync               # Sync with git
```

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed):
   ```bash
   cargo test
   cargo build --release
   ```
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
- ALWAYS run `cargo build --release` before finishing - the user runs the release binary

