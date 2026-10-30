# Starlark .bzl files shared with GN via import(): buildifier module-docstring lint conflicts with GN syntax

- **Learned from:** inner loop friction on [CL 1932208](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932208), patchset 3 (task `fx-bootfs-rule-and-core-cutover`)
- **Date:** 2026-10-29
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

bazel_minimality's buildifier_lint remediation always demanded a triple-quoted
module docstring, ignoring that some .bzl files are import()ed by .gni/.gn files
where a Starlark docstring is a GN parse error; buildifier also kept reporting
module-docstring despite a file-level disable comment, so the only GN-compatible
fix could never clear the check.

## Why not a one-off fix

Any .bzl constants file shared between GN and Bazel (imported by a .gni) hits
the same contradictory remediation; suppressing in one file or disputing the
finding per task leaves every future migration touching such shared files stuck
in the same machinery conflict.
