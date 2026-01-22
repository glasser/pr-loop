---
name: pr-ready
description: Mark a draft PR as ready for review after validating it's in a good state
disable-model-invocation: true
---

# PR Ready

Mark a draft PR as ready for human review. This validates the PR is in a good state before transitioning it out of draft mode.

## Instructions

Run `pr-loop ready` to:

1. Verify the PR is currently in draft mode
2. Validate that:
   - All CI checks are passing (no failures or pending)
   - No unresolved review threads need response
3. Remove the LLM iteration status block from the PR description
4. Mark the PR as ready for review (non-draft)

## When to Use

Use this skill when:
- You've finished iterating on a PR with `/pr-loop` or `/pr-loop-unattended`
- The PR is in a good state (CI green, comments addressed)
- You're ready for human reviewers to look at it

## Example

```
pr-loop ready
```

Output on success:
```
Checking PR draft status...
✓ PR is in draft mode
Validating PR state...
✓ No unresolved threads
✓ All CI checks passed
Removing status block from PR description...
✓ Status block removed
Marking PR as ready for review...
✓ PR marked as ready for review

🎉 PR is now ready for human review!
```

## Important Notes

- The PR must be in draft mode - this command is for transitioning drafts to ready
- CI must be fully passing (not pending) before marking ready
- All review threads must be resolved or have Claude's response as the last comment
