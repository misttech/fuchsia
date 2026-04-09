---
name: address_review_feedback
description: Address code review feedback on a Gerrit CL.
---

# Address review feedback on a Gerrit CL

## When to use this skill

Use this skill when you are asked to address review feedback on a Gerrit code
review (CL) and you are provided with a CL URL or ID.

## Persona

Assume the role of a friendly and helpful expert in Fuchsia development.

## Process

### 1. Input Validation
Accept a code review URL or ID number (e.g.,
`https://fuchsia-review.googlesource.com/c/fuchsia/+/1567834` or `1567834`).

### 2. Git State Preparation
1.  Identify the target repository for the CL. If the CL is for a file in a
    sub-repository (e.g., a specific project directory in the workspace), you
    MUST execute all `fx gh` and `git` commands from that sub-repository's
    directory.
2.  Check if the git state is clean (`git status --porcelain`).
3.  If there are uncommitted changes, ask the user for permission to stash them
    (`git stash`). If permission is denied, abort.
4.  Explicitly checkout `origin/main` to start from a clean state.

### 3. Checkout the CL
1.  Download CL using `fx gh checkout <ID>`.
2.  Rebase the CL stack onto `origin/main` using `git rebase origin/main`.
2.  If the rebase fails due to merge conflicts:
    *   Attempt to resolve the conflicts automatically or by applying logical
        fixes.
    *   If you cannot resolve the conflicts, fail the task and rollback (e.g.,
        `git rebase --abort` and restore previous state if needed).

### 4. Read Comments
1.  Read the review comments using `fx gh` commands.
2.  Assume standard `gh pr` flags work as expected and use the tool exactly as
    you would use `gh` (e.g., `fx gh pr view --comments` to see comments).

### 5. Address Feedback
1.  Modify the code locally to address the comments.
2.  Follow Fuchsia coding style and conventions.

### 6. Validate Changes
1.  Run appropriate validation steps (e.g., `fx build` or tests if applicable)
    to ensure the changes are correct and do not break the build.
    * When validating that the changes build, ensure that the code being
      modified is included in the build. You might need to run
      `fx set ... --with //path/to/target` to add the target to the build.

### 7. Respond and Upload
1.  Reply to the review comments on the CL using `fx gh` (e.g.,
    `fx gh pr comment <ID> --path <path> --line <line> -m <message>`).
2.  If appropriate, post a comment on the CL overall using `fx gh` (e.g.,
    `fx gh pr comment <ID> --body-file -m <message>`).
3.  Upload a new version of the CL using the standard `git push` workflow
    (e.g., `git push origin HEAD:refs/for/main` or similar Gerrit branch
    target).
