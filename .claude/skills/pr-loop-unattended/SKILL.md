---
name: pr-loop-unattended
description: Monitor a PR and respond to comments/CI failures until the PR is happy (CI passing, no comments)
---

# PR Loop - Unattended Mode

Run the pr-loop tool in unattended mode, responding to review comments and CI failures until the PR is "happy" (CI passing with no unaddressed comments).

## Instructions

1. Run `pr-loop --wait-until-actionable-or-happy --maintain-status` to wait for the PR to need attention or become happy
2. Check the exit status and output:
   - If the PR is **happy** (CI passing, no comments), you're done! Report success to the user.
   - If the PR is **actionable**, continue to step 3
3. Read the tool output and address one issue:
   - If there are **commit message reword requests**, apply them first (see below) — they're quick and mechanical
   - If there are **review comments needing response**, pick one to address - make the requested changes and reply using `pr-loop reply` as instructed in the output
   - If there are **CI failures**, investigate and fix them
4. Commit your changes (as a new commit, not amending) and push
5. Return to step 1

## Commit Message Reword Requests

A human can request that a specific commit's message be reworded — typically via the web UI's click-to-edit on a commit, sometimes filed by hand as a PR comment. The tool's output includes the exact command to run:

```
pr-loop reword-commit --commit <sha> --message "<exact text>" --request-id <comment_id>
```

Use the `--message` text exactly as shown in the tool's output — don't paraphrase or improve it; the human dictated the wording. This rewrites the target commit's message via a local `git rebase -i` and deletes the request comment on success. It does **not** push for you, and it does **not** create a new commit — this is the deliberate exception to "create new commits, don't amend." After it succeeds, push with:

```
git push --force-with-lease
```

(not a plain `git push` — the rebase changed history, and `--force-with-lease` is the safe form since it fails instead of clobbering if someone else pushed in the meantime).

## Interim Acknowledgments

As soon as you've formed an initial thought on a comment but still need to do more research or work before you can actually address it, post a quick interim reply:

```
pr-loop reply --in-reply-to <comment_id> --message "Looking into this now." --in-progress
```

This lets a human who checks in on the PR know you've seen the comment and share your initial take, without waiting for the real fix. The thread will still show up as needing a response on your next loop iteration — `--in-progress` marks the reply as an ack, not a final answer, so it can't get lost. Once you've actually addressed the comment, reply again normally (without `--in-progress`) to close it out.

Don't bother with an interim ack if you can address the comment immediately.

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

- **If pr-loop is already running as a background task**, stop it first before starting a new loop.
- This mode is for autonomous operation - the user may be away
- You must still address any comments or CI failures that appear - the difference from attended mode is that you can finish successfully once there's nothing left to do
- The PR must be in draft mode to use `--maintain-status`
- Address one item at a time to keep the iteration loop fast - don't batch everything before pushing
- Unless explicitly told otherwise, create new commits for each fix rather than amending previous commits
- When replying to review comments, be concise and explain what you changed
- If you encounter a difficult issue, keep trying different approaches - the user isn't here to help anyway. Only give up and report the problem if you've truly exhausted your options.
- The tool waits at least 30 seconds after your last push before declaring "happy" to ensure CI has triggered
