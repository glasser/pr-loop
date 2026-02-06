# pr-loop

A CLI tool to help Claude Code manage PR workflows. It monitors PRs for review comments and CI failures, enabling LLM-assisted iteration on pull requests.

## Features

- **PR Status Analysis**: Shows current state of CI checks and review threads
- **Wait Modes**: Block until the PR needs attention or becomes "happy" (CI passing, no unaddressed comments)
- **Review Thread Management**: Reply to review comments with Claude-marked messages
- **CI Failure Investigation**: Fetches CircleCI logs for failed checks
- **Status Tracking**: Maintains a status block in the PR description showing iteration progress

## Installation

```bash
cargo install --path .
```

Requires the `gh` CLI to be installed and authenticated.

## Usage

### Check PR Status

```bash
# In a repo with a PR for the current branch
pr-loop

# Or specify explicitly
pr-loop --repo owner/repo --pr 123
```

### Wait for PR to Need Attention

```bash
# Wait until there are review comments or CI failures
pr-loop --wait-until-actionable --maintain-status

# Wait until PR is "happy" (CI passing, no comments) or needs attention
pr-loop --wait-until-actionable-or-happy --maintain-status
```

### Reply to Review Comments

```bash
pr-loop reply --in-reply-to COMMENT_ID --message "Fixed the issue"
```

The message will be prefixed with a Claude marker. If there are newer comments posted while you were working, they'll be shown for you to address.

### Mark PR as Ready

```bash
pr-loop ready
```

Validates CI is passing and no unresolved threads, removes the status block, and marks the PR as non-draft.

## CI Check Filtering

Filter which CI checks to monitor:

```bash
# Only include specific checks
pr-loop --include-checks "build,test/*"

# Exclude checks
pr-loop --exclude-checks "codecov/*,lint"
```

Or via environment variables:

```bash
export PR_LOOP_INCLUDE_CHECKS="ci/*,build"
export PR_LOOP_EXCLUDE_CHECKS="lint"
```

## Claude Code Skills

This repo includes Claude Code skills in `.claude/skills/` that automate PR iteration:

- **`/pr-loop`** (Attended Mode): Runs a loop responding to review comments and CI failures until you tell it to stop. Use when you want to stay engaged and provide guidance.

- **`/pr-loop-unattended`** (Unattended Mode): Runs autonomously until the PR is "happy" (CI passing, no unaddressed comments). Use when you're stepping away and want Claude to handle things.

Both skills require the PR to be in draft mode when using `--maintain-status`.

## GraphQL Schema Validation

The project validates all GraphQL queries against GitHub's schema at test time. Query files are in `graphql/operation/` and the schema is in `graphql/schema/`. The source code uses `include_str!` to load queries from these files, ensuring the validated queries are the same ones used at runtime.

## License

MIT
