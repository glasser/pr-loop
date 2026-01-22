---
name: pr-ready
description: Mark a draft PR as ready for review after validating it's in a good state
disable-model-invocation: true
---

# PR Ready

Mark a draft PR as ready for human review. This validates the PR is in a good state before transitioning it out of draft mode.

## Instructions

Run `pr-loop ready` to (add `--delete-claude-threads` if user passed "delete" as an argument to this skill):

1. Verify the PR is currently in draft mode
2. Verify the PR has exactly one commit (fail with squash instructions if not)
3. Validate that:
   - All CI checks are passing (no failures or pending)
   - All review threads are resolved (not just responded to)
4. Remove the LLM iteration status block from the PR description
5. Mark the PR as ready for review (non-draft)

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

## Deleting Pure-Claude Threads (Optional)

When iterating with an LLM, you may end up with review threads where all comments are from Claude (e.g., Claude talking to itself during iterations). These can be noise for human reviewers.

**Do not use this option unless the user specifically requests it** (e.g., `/pr-ready delete`).

This will delete all comments in resolved threads where every comment starts with the Claude marker. Threads with any non-Claude comments are preserved.

## Important Notes

- The PR must be in draft mode - this command is for transitioning drafts to ready
- The PR must have exactly one commit - squash your commits before running
- CI must be fully passing (not pending) before marking ready
- All review threads must be actually resolved (having Claude's response as the last comment is not enough)
