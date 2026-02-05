---
name: pr-loop
description: Monitor a PR and respond to review comments and CI failures in a loop
---

# PR Loop - Attended Mode

Run the pr-loop tool in attended mode, responding to review comments and CI failures until you are told to stop.

## Instructions

1. Run `pr-loop --wait-until-actionable --maintain-status` to wait for the PR to need attention
2. When the tool returns, read its output carefully:
   - If there are **review comments needing response**, pick one to address - make the requested changes and reply using `pr-loop reply` as instructed in the output
   - If there are **CI failures**, investigate and fix them
3. Commit your changes (as a new commit, not amending) and push
4. Return to step 1 and wait for the next actionable state

## Status Messages

You can communicate your current status by passing `--status-message` to pr-loop:

```
pr-loop --wait-until-actionable --maintain-status --status-message "Working on CI failures"
```

This updates a status block in the PR description that's visible to humans. Use this to communicate:
- What you're currently working on
- If you're struggling with a particular issue
- Any context that might be helpful

If you don't pass `--status-message`, any previous status message is cleared (the status block remains but without the custom message). This is fine if you don't have anything particular to say.

## Important Notes

- This loop runs indefinitely until the user tells you to stop
- The PR must be in draft mode to use `--maintain-status`
- Address one item at a time to keep the iteration loop fast - don't batch everything before pushing
- Unless explicitly told otherwise, create new commits for each fix rather than amending previous commits
- When replying to review comments, be concise and explain what you changed
- If you're unsure how to address a comment, ask the user for guidance
- If CI keeps failing on the same issue, you can ask the user for help while continuing to attempt fixes
