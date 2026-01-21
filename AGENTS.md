# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Development Ground Rules

### GitHub API Usage
- **Automated tests MUST NOT talk to real GitHub.** Use dependency injection with test implementations.
- **Manual testing only** against the designated test repo: `glasser/pr-loop-test-repo`
- Do NOT interact with any other GitHub repos during development unless explicitly authorized.

### Testing Requirements
- High level of automatically-enforced test coverage is required.
- Design with dependency injection: API clients should be interfaces with real and test implementations.
- Use recorded fixtures or constructed test data, not live API calls in tests.
- Manual QA against the test repo informs what mock behavior to implement.

### Current Scope (V1)
- GitHub integration: review threads, reply command - fully implemented
- CI status: visible (pass/fail/pending) but CircleCI log fetching is DEFERRED
- See backlog issues (P4) for deferred CircleCI log work

## Quick Reference

```bash
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
2. **Run quality gates** (if code changed) - Tests, linters, builds
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

