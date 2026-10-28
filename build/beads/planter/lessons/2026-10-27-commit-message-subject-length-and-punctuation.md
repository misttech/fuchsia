# Commit message subject length and punctuation validation

- **Learned from:** human review on [CL 1912201](https://fuchsia-review.googlesource.com/c/fuchsia/+/1912201), patchset 10 (task `ffx_platform_preflight_product_common`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

When adding a [<subsystem>] tag to a deep //<dir> path pushes the commit subject
over 65 characters, the commit message formatter wraps or truncates the subject
after [<subsystem>] and moves the full [bazel_migration][<subsystem>] //<dir>.
line into the commit body. Furthermore, commit_message_format.sh previously
suggested the un-abbreviated [bazel_migration][<subsystem>] //<dir> string in
its remediation even when that string exceeded 65 characters, did not flag
subjects over 65 characters when they matched the regex, and allowed trailing
punctuation on //<dir> in the subject.

## Why not a one-off fix

Fixing only the single CL's commit message leaves commit_message_format.sh
suggesting >65-character subjects and accepting overlong or period-terminated
subjects across all deep directory migrations, allowing future commits to suffer
the same subject line truncation or wrapping.
