---
name: pr-loop-unattended
description: Monitor a PR and respond to comments/CI failures until the PR is happy (CI passing, no comments)
disable-model-invocation: true
---

# PR Loop - Unattended Mode

Run the pr-loop tool in unattended mode, responding to review comments and CI failures until the PR is "happy" (CI passing with no unaddressed comments).

## Instructions

1. Run `pr-loop --wait-until-actionable-or-happy --maintain-status` to wait for the PR to need attention or become happy
2. Check the exit status and output:
   - If the PR is **happy** (CI passing, no comments), you're done! Report success to the user.
   - If the PR is **actionable**, continue to step 3
3. Read the tool output and address one issue:
   - If there are **review comments needing response**, pick one to address - make the requested changes and reply to the thread using `pr-loop reply --thread <id> --message "<response>" --resolve`
   - If there are **CI failures**, investigate and fix them
4. Commit your changes (as a new commit, not amending) and push
5. Return to step 1

## Status Messages

You can communicate your current status by passing `--status-message` to pr-loop:

```
pr-loop --wait-until-actionable-or-happy --maintain-status --status-message "Investigating flaky test"
```

This updates a status block in the PR description that's visible to humans. Use this to communicate:
- What you're currently working on
- If you're struggling with a particular issue (e.g., "Attempting fix #3 for CI timeout")
- Any context that might be helpful for when the user returns

If you don't pass `--status-message`, any previous status message is cleared (the status block remains but without the custom message). This is fine if you don't have anything particular to say.

## Important Notes

- This mode is for autonomous operation - the user may be away
- You must still address any comments or CI failures that appear - the difference from attended mode is that you can finish successfully once there's nothing left to do
- The PR must be in draft mode to use `--maintain-status`
- Address one item at a time to keep the iteration loop fast - don't batch everything before pushing
- Unless explicitly told otherwise, create new commits for each fix rather than amending previous commits
- When replying to review comments, be concise and explain what you changed
- If you encounter a difficult issue, keep trying different approaches - the user isn't here to help anyway. Only give up and report the problem if you've truly exhausted your options.
- The tool waits at least 30 seconds after your last push before declaring "happy" to ensure CI has triggered
