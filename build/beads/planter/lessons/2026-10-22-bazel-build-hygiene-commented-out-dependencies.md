# Bazel build hygiene commented out dependencies

- **Learned from:** human review on CL I0ead572c4a5deb29da8937074f99a45810686959, patchset 1 (task `src_sys_time_testing_fake_hrtimer`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The existing bazel_minimality check audited redundant default attributes,
single-use private variables, and buildifier lint rules, but did not inspect
list attributes (such as deps or srcs) in BUILD.bazel for dead commented-out
dependency or source label lines carried over from BUILD.gn.

## Why not a one-off fix

Removing the commented-out dependency lines only in the single package's
BUILD.bazel would leave future migrations vulnerable whenever a pre-existing
BUILD.gn contains commented-out dependency or source strings and the coding
agent copies them verbatim under the general comment-preservation rule.
