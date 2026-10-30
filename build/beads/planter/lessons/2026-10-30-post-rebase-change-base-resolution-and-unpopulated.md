# Post rebase change base resolution and unpopulated submodule ninja triage

- **Learned from:** inner loop friction on [CL 1932168](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932168), patchset 5 (task `rustc-staticlib-b2gn`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh` (the `checks/build_verification.sh` part, which downgraded missing-source Ninja failures on .gni/.bzl changes, was reverted as an unreviewed loosening)

## Root cause

First, checks/bazel_minimality.sh computed git diff against raw
PLANTER_CHANGE_BASE without advancing change_base to merge-base(origin/main,
HEAD) or HEAD~1 after a rebase onto a newer origin/main or branch switch (and
only checked [bazel_migration] and [bazel2gn] subject prefixes, omitting
[bazel]), causing upstream commits on origin/main (such as
build/tools/bazel2gn/compiled_action.go) to appear in the changed file set and
trigger unrelated rule/converter checks. Second, unrelated_gn_ninja_failure() in
checks/build_verification.sh returned None whenever bazel_dirs was empty or
.gni/.bzl files were modified before calling unrelated_ninja_missing_source(),
and rdep_build_files() ran rg without patterns when dirs was empty, causing
full-product Ninja failures on missing source files in unpopulated third-party
submodules to be reported as blocking build_failure_gn_build errors on
tooling/macro tasks.

## Why not a one-off fix

Waiving the disputed findings only for a single Rust macro or bazel2gn task
would leave every future task that rebases onto a newer origin/main or modifies
shared .bzl/.gni/bazel2gn files vulnerable to false-positive static checks on
upstream commits and false-positive Ninja failures on unpopulated submodules.
