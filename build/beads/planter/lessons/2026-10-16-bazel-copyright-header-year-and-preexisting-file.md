# Bazel copyright header year and preexisting file parity

- **Learned from:** inner loop friction on a local task (task `todo-unised`)
- **Date:** 2026-10-16
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

No deterministic static check in checks/bazel_minimality.sh audited Fuchsia
copyright headers on newly added Starlark files or recorded INFO
copyright_header coverage when newly added BUILD.bazel files already use the
current year while modified pre-existing BUILD.bazel files (such as shared
third-party crate visibility allowlists) legitimately retain their original
creation year.

## Why not a one-off fix

Any GN-to-Bazel migration that adds a new BUILD.bazel and/or grants visibility
in a pre-existing shared BUILD.bazel file created in a prior year can either
introduce a stale copyright year in the new file or trigger a false-positive
review_diff copyright_header warning on the pre-existing file unless
bazel_minimality deterministically validates new files and records INFO coverage
for pre-existing files.
