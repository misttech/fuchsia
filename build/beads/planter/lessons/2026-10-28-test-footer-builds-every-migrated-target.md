# `Test:` footers build every migrated non-test target

- **Learned from:** human review of CL 1918746 and CL 1918766
  (`//zircon/system/utest/core/*`) and CL 1926390 (`//src/security/tests/pkg_test/*`)
- **Date:** 2026-10-28
- **Changed:** `checks/commit_message_format.sh`, `prompts/coder.md`

## Root cause

Each change migrated five or more targets but its `Test:` footer built one of
them. All of their users were still GN targets, so presubmit only compiled the
bazel2gn copies, never the Bazel targets (`verify_bazel2gn` only checks that the
generated BUILD.gn is in sync). Reviewers had no evidence the other Bazel
targets built at all. `build_verification` builds them locally, but the commit
message didn't say so, and nothing checked the footer against the
`Bazel-Migration-Target:` list.

## Remediation

1. `commit_message_format.sh` reports `test_footer_missing_migrated_target`
   (warning) for each `Bazel-Migration-Target:` that is not a test entry point
   and that no `Test:` line builds (by label, package shorthand, `:all`, or an
   ancestor `/...`). The remediation suggests one `fx bazel build` line.
2. `prompts/coder.md` says to name every migrated non-test target in one
   `fx bazel build` line per `--config`, which still fits the 3-line limit.
