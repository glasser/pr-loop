---
name: pr-loop
description: Monitor a PR and respond to review comments and CI failures in a loop
disable-model-invocation: true
---

# PR Loop - Attended Mode

Run the pr-loop tool in attended mode, responding to review comments and CI failures until you are told to stop.

## Instructions

1. Run `pr-loop --wait-until-actionable` to wait for the PR to need attention
2. When the tool returns, read its output carefully:
   - If there are **review comments needing response**, pick one to address - make the requested changes and reply to the thread using `pr-loop reply --thread <id> --message "<response>" --resolve`
   - If there are **CI failures**, investigate and fix them
3. Commit your changes (as a new commit, not amending) and push
4. Return to step 1 and wait for the next actionable state

## Important Notes

- This loop runs indefinitely until the user tells you to stop
- Address one item at a time to keep the iteration loop fast - don't batch everything before pushing
- Unless explicitly told otherwise, create new commits for each fix rather than amending previous commits
- When replying to review comments, be concise and explain what you changed
- If you're unsure how to address a comment, ask the user for guidance
- If CI keeps failing on the same issue, you can ask the user for help while continuing to attempt fixes
