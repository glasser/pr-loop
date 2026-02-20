---
name: pr-ready
description: Mark a draft PR as ready for review after validating it's in a good state
disable-model-invocation: true
argument-hint: "[preserve]"
---

# PR Ready

Mark a draft PR as ready for human review. This validates the PR is in a good state before transitioning it out of draft mode.

## Instructions

Run `pr-loop ready` to (add `--preserve-claude-threads` if user passed "preserve" as an argument to this skill):

1. Verify the PR is currently in draft mode
2. Verify the PR has exactly one commit. If not, follow the squash instructions in the output, force-push, then run `pr-loop --wait-until-actionable-or-happy --maintain-status` to wait for CI, and finally run `pr-loop ready` again
3. Validate that:
   - All CI checks are passing (no failures or pending)
   - All review threads are resolved (not just responded to)
4. Delete resolved review threads where all comments are from Claude (unless `--preserve-claude-threads` is passed)
5. Remove the LLM iteration status block from the PR description
6. Mark the PR as ready for review (non-draft)

## When to Use

Use this skill when:
- You've finished iterating on a PR with `/pr-loop` or `/pr-loop-unattended`
- The PR is in a good state (CI green, all threads resolved)
- The PR has been squashed to a single commit
- You're ready for human reviewers to look at it

## Example

```
pr-loop ready
```

Output on success:
```
Checking PR draft status...
✓ PR is in draft mode
Checking PR commit count...
✓ PR has a single commit
Validating PR state...
✓ All threads resolved
✓ All CI checks passed
Removing status block from PR description...
✓ Status block removed
Marking PR as ready for review...
✓ PR marked as ready for review

🎉 PR is now ready for human review!
```

## Preserving Pure-Claude Threads (Optional)

When iterating with an LLM, you may end up with review threads where all comments are from Claude (e.g., Claude talking to itself during iterations). These are typically noise for human reviewers, so by default they are deleted.

**If the user specifically requests preservation** (e.g., `/pr-ready preserve`), pass `--preserve-claude-threads` to keep these threads.

By default, all comments in resolved threads where every comment starts with the Claude marker will be deleted. Threads with any non-Claude comments are always preserved.

## If Squashing Is Needed

If `pr-loop ready` fails because the PR has multiple commits, follow the squash instructions in the output, then:

1. Force-push the squashed branch: `git push --force-with-lease`
2. Wait for CI by running exactly: `pr-loop --wait-until-actionable-or-happy --maintain-status`
3. Once that exits successfully (meaning CI passed), run `pr-loop ready` again

**CRITICAL**: You MUST use `--wait-until-actionable-or-happy` (NOT `--wait-until-actionable`). The `--wait-until-actionable` flag will NOT exit when CI passes — it only exits when there are comments or failures to address. The `-or-happy` variant is what exits successfully when CI is green with no comments.

## If CI Is Still Pending

If `pr-loop ready` fails because CI checks are still pending, wait for CI by running exactly:

```
pr-loop --wait-until-actionable-or-happy --maintain-status
```

Once that exits successfully, run `pr-loop ready` again.

**CRITICAL**: Use `--wait-until-actionable-or-happy` (NOT `--wait-until-actionable`). Only the `-or-happy` variant exits successfully when CI passes with no comments. Do NOT use `sleep` to wait for CI either.

## Important Notes

- The PR must be in draft mode - this command is for transitioning drafts to ready
- The PR must have exactly one commit - squash your commits before running
- CI must be fully passing (not pending) before marking ready
- All review threads must be actually resolved (having Claude's response as the last comment is not enough)
