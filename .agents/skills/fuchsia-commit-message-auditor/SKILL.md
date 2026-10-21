---
name: fuchsia-commit-message-auditor
description: >
  Audits, validates, and formats Fuchsia Git commit messages against the
  Fuchsia commit message style guide (50-char subject, imperative mood,
  72-char wrap, Bug:, Test:, Change-Id: footers). Use when writing, editing,
  or auditing commit messages. Do NOT use for reviewing code logic or diffs
  (use /review instead).
---

# Commit message auditor and diff matcher

Follow this 5-step workflow to prepare, audit, and commit changes.

## Step 1: Code Formatting & Diff Hygiene

1.  **Format Code**: Run from the repository root (or pass explicit files)
    before staging:

    ```bash
    fx format-code
    # Or: fx format-code --files=path/to/file1.cc,path/to/file2.rs
    ```

2.  **Lint Code**: Run static analysis linters across modified files and apply
    automated fixes before staging:

    ```bash
    fx lint --fix
    # Or: fx lint --fix --files=path/to/file1.cc,path/to/file2.rs
    ```

3.  **Explicit Staging**: Stage only the intended files explicitly by path:

    ```bash
    git add path/to/file1.cc path/to/file2.rs
    ```

    - **NEVER** use catch-all staging (`git add .`, `git add -A`, `git add -u`,
      `git commit -a`).
    - In multi-repo workspaces, execute git commands in the target repository
      root (e.g., `//` or `//vendor/google`).
4.  **Diff Hygiene**: Inspect `git diff --staged` and `git status --short`
    (without `-uno`, which hides untracked files) to ensure only intended
    changes are staged and no temporary files remain in the working tree:
    - **Accidentally Staged Files**: If scratch scripts, temporary files
      (`.tmp`, `.log`), or unrelated files were staged, unstage them (and remove
      any temporary files):

     ```bash
     git restore --staged path/to/unwanted_file
     rm -f path/to/temporary_file
     ```

    - **No Debug Code**: Strip debug prints (e.g., `printf("DEBUG_PRINT_TRAP")`)
      and temporary logging from staged code.
    - **No Gratuitous Churn**: Avoid unrelated comment/docstring rewording or
      purely stylistic variable renames.

## Step 2: Draft Commit Message to Scratch File

Important: Always draft and edit commit messages in a scratch file in your
designated scratch directory (e.g., `/tmp/commit_msg_<workspace_or_id>.txt`,
never inside the repository or a shared `/tmp/commit_msg.txt` on multi-session
hosts) using native file tools (e.g., `write_to_file`). NEVER use shell commands
(`echo`, `cat << 'EOF'`, heredocs, redirection) or CLI flags (`-m`, `--text`).

### Commit Message Structure & Example

```none
[component] Imperative summary line under 50 chars

Explain why this change is needed and what architectural problem it solves.
Wrap all body lines at 72 characters. Describe the net change relative to
the parent commit; use "add" or "create" for newly introduced files.

Bug: 123456
Test: fx test commit_msg_checker_test
```

### Formatting & Content Rules

- **Summary Line (Line 1)**:
  - Format: `[component] Imperative summary`, `subsystem: Imperative summary`,
    or `Imperative summary`.
  - Style: Capitalize first word after prefix; use imperative mood ("Add", not
    "Added"/"Adds").
  - Length: <= 50 chars recommended; hard limit <= 65 chars (revert/reland
    exempt).
  - Constraints: No trailing period. No issue tracker tags (`Bug:`) on line 1.
- **Blank Line (Line 2)**: Mandatory.
- **Body**:
  - Hard line-wrap at **72 characters** (exempt: URLs, bug IDs on `Bug:` lines,
    indented code, quote blocks).
  - **Engineering Rationale**: Explain *why* (architectural context), not just
    *what* (the diff shows *what*).
  - **Tone & Style**: Direct, professional engineering prose. Avoid robotic
    buzzwords, nominalization bloat, or parallelism breaks. No transient
    procedural commentary (e.g. "Ran tool X", "All files staged").
  - **Net Atomic Change**: Narrate net state relative to parent (`<commit>^`).
    Describe newly introduced files (`status A`) as `add`/`create`/`introduce`,
    never "update".
  - **Command Output & Code Blocks (4-Space Indented)**: When including sample
    command output or verification transcripts in the body (above the footers),
    indent lines by 4 spaces and copy the output verbatim from an actual command
    execution rather than fabricating it.
  - **Series**: For multi-part migrations, indicate series step in summary or
    body: `(1/3)`.
  - Never include "DO NOT SUBMIT".
- **Footers (Bottom, separated by blank line)**:
  - `Test: <actual_verification>` (**Required**): State how the change was
    verified. Must describe the concrete automated test targets that actually
    ran (e.g., `Test: fx test //src/diagnostics/iquery:lib_test`, not a `:tests`
    group label that matched zero tests), manual testing, or ad-hoc verification
    genuinely performed (or `Test: None, doc update`). Diagnostics (e.g.,
    `--list`, `git status`) and code formatting are **not** tests. Never
    fabricate tests or use `TODO`/`TBD`. Wrap at 72 chars; use multiple `Test:`
    lines for multiple suites.
  - `Bug: <id>` / `Fixed: <id>` (**Recommended**): One per line (`Bug: None` if
    standalone). `Fixed:` auto-closes the issue upon submission.
  - `Multiply: <test_name>` (Optional): Trigger deflake runs in infra for
    new/modified tests.
  - `Change-Id:` (**Gerrit Hook**):
    - **New commit**: Omit; generated automatically by Gerrit commit hook upon
      `git commit` (never pass `--no-verify`).
    - **Amending existing commit**: Preserve exact `Change-Id: I...` (must be
      the final line).
  - Presubmit Controls (Optional): `Depends-on: <Change-Id>`, `Run-All-Tests:
    true`, `Cq-Include-Trybots: <builder>`.

## Step 3: Semantic Intent-to-Diff Audit

Inspect state efficiently in a single tool call:

- **For uncommitted / staged changes**: Run `git diff --staged --stat -p` to
  inspect the touched files summary and code diff at once.
- **For an existing commit on HEAD**: Run `git show --stat HEAD` to inspect the
  commit message, metadata footers, touched file summary, and code diff at once.

Cross-check the diff against your draft scratch file:

1.  **Diff → Narrative (Completeness & Scope)**:
    - Does the message account for all functional changes in the staged diff?
    - Are touched files described with verbs matching git status (`add`/`create`
      for `A`, `update`/`modify` for `M`)?
    - Is the patch cleanly scoped to a single logical change without unrelated
      refactors?
2.  **Narrative → Diff (Factual Support & Veracity)**:
    - Is every factual claim, bug fix, and architectural explanation backed by
      the staged code?
    - No ghost claims (unhandled edge cases or unadded features mentioned in
      text).
    - Component names match touched files in `git diff --stat` (no
      overgeneralized subsystem claims).
    - Every `Test:` line describes actual automated, manual, or ad-hoc
      verification performed in this session. Diagnostic queries (like `--list`
      or `status`) and formatting tools are not tests.

## Step 4: Mechanical Lint Check (Pre-Commit)

Warning: Mechanical commit-message linters are line-length and footer checkers
only, NOT semantic reviewers. Passing the linter (`exit code 0`) only proves
lines are wrapped and tags are not duplicated. It CANNOT verify whether your
claims match the diff, whether tests actually ran, or whether debug noise was
staged. You MUST complete the Semantic Intent Audit in Step 3 before running
`git commit`.

Validate your draft scratch file before committing (`commit_msg_checker.py` is
registered in `fx lint` as the `commit_msg` SHAC check for committed changes via
`fx lint --only=commit_msg`, but pre-commit validation of an uncommitted
`<scratch_file>` invokes `scripts/shac/commit_msg_checker.py` directly because
SHAC's `commit_msg` check inspects committed git history and `fx lint` treats
arguments after `--` as file path filters):

```bash
./scripts/fuchsia-vendored-python scripts/shac/commit_msg_checker.py \
  --strict --message-file <scratch_file>
```

Note: For a read-only audit of a pre-existing commit on `HEAD` without amending,
run `fx lint --only=commit_msg` (or `./scripts/fuchsia-vendored-python
scripts/shac/commit_msg_checker.py --strict` to exit non-zero on warnings).

`fx lint --only=commit_msg` / `commit_msg_checker.py --strict` validates:

- First line summary length (<= 65 characters)
- Body line wrapping (<= 72 characters)
- No duplicate `Change-Id` footers
- No duplicate `TAG` or `CONV` metadata tags

If the linter reports warnings or errors, edit your draft scratch file and
re-run Step 4 until clean.

## Step 5: Execute Commit or Amend

### Initial Commit

```bash
git commit -F <scratch_file> && rm -f <scratch_file>
```

### Amending Existing Commit / Rebase

1.  **Inspect & Preserve Metadata**: Run `git show --stat HEAD` to inspect the
    existing commit message and diff. Retain existing `Change-Id: I...` (must
    remain the final line) and existing `Bug:` tags in `<scratch_file>`.
2.  **Lint Draft**:

    ```bash
    ./scripts/fuchsia-vendored-python scripts/shac/commit_msg_checker.py \
      --strict --message-file <scratch_file>
    ```

3.  **Stage & Amend**:

    ```bash
    git add <explicit_modified_files>
    git commit --amend -F <scratch_file> && rm -f <scratch_file>
    ```

    (If currently in an interactive rebase: `GIT_EDITOR=true git rebase
    --continue`)

4.  **Completion**: Once `git commit` or `git commit --amend` succeeds, the task
    is finished. Do not re-run `fx lint`, `fx format-code`, or redundant log
    queries post-commit.
