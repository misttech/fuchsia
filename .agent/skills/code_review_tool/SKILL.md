---
name: code-review-tool
description: Guide for using fx gh (Gerrit CLI) to interact with code reviews.
---

# Using fx gh for Gerrit Code Reviews

## Overview

The `fx gh` tool provides a command-line interface for managing Gerrit code
reviews that matches the GitHub CLI (`gh`). Coding agents can leverage their
parametric knowledge of the `gh` tool to use `fx gh`.

## Key Concepts and Differences

While the interface is the same as `gh`, it interacts with Gerrit. Here are the
key mappings and differences you need to know:

### 1. Pull Requests are Gerrit Changes (CLs)
- In `fx gh`, a "Pull Request" (PR) corresponds to a Gerrit Change (commonly
  referred to as a CL in Fuchsia).
- Commands like `fx gh pr list`, `fx gh pr view`, and `fx gh pr comment` work on
  Gerrit changes.

### 2. Identifying Changes
- `fx gh` works with Gerrit change IDs.
- You can use the numeric ID from the URL (e.g., `1569017` from
  `https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1569017`).
- You can also use the `Change-Id` value from the commit message (e.g.,
  `Id102da53bb4ebe08aa599f1479650553fb3f118d`).
- **Important**: The `Change-Id` line in the commit message is critical for
  Gerrit to link subsequent commits back to the same code review. Do not remove
  or alter it unless intentionally creating a new change.

### 3. Command Specifics

#### `pr list`
- **Assignee vs Reviewer**: Gerrit does not have a direct equivalent to GitHub's
  "assignee". The `-a, --assignee` flag is mapped to filter by **reviewer** in
  Gerrit.
- To filter by author/owner, use the `--author` flag.

#### `pr comment`
- **Threading and Replying to Comments**: When posting a comment with `--path`
  and `--line`, the tool automatically detects existing threads on that line and
  replies to the latest comment. You do **not** need to manage thread IDs
  manually.
- If there are multiple independent threads on the same line, the tool replies
  to the one with the most recent activity.
- **Example**: To reply to a review comment on line 42 of `src/foo.cc`: `fx gh
  pr comment 1569017 --path src/foo.cc --line 42 -m "Done. Fixed as suggested."`

#### `pr edit` and `pr review`
- **Approving a Change**: To add a `Code-Review+2` vote, use `fx gh pr review
  <id> --approve`.
- **Triggering CQ (Commit Queue)**: To set Gerrit labels like `Commit-Queue+1`
  or `Commit-Queue+2`, you must use the `edit` subcommand with the `--add-label`
  flag.
  - **Example**: `fx gh pr edit <id> --add-label Commit-Queue+1`

#### `pr checks`
- **Viewing Check Status**: To see the status of CI checks and failing bots for
  a Gerrit change, use `fx gh pr checks <id>`. This command provides the
  detailed logs and failure reasons needed to debug failing checks.

#### `pr cherry-pick`
- **Cherry-picking a CL**: To fetch and apply a Gerrit change to your current
  local Git branch, use `fx gh pr cherry-pick <id>`.

### 4. Uploading Changes
- You do **not** need to use `fx gh` to upload or update CLs.
- Use the normal `git push` commands to upload changes to Gerrit (e.g., `git
  push origin HEAD:refs/for/main`).

## Usage Examples

- View a change: `fx gh pr view 1569017`
- List open changes: `fx gh pr list --author me`
- Comment on a specific line: `fx gh pr comment 1569017 --path src/foo.cc --line
  42 -m "Looks good"`
- Mark a CL for CQ dry-run: `fx gh pr edit 1569017 --add-label Commit-Queue+1`
- Cherry-pick a change locally: `fx gh pr cherry-pick 1569017`
- View check status: `fx gh pr checks 1569017`
