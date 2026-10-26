# Explicit narrow GN visibility preservation and commit message subsystem/target/test footer validation

- **Learned from:** human review on CL If551a3c64703a2bcc7982992aa26b17f54a716bd, patchset 1 (task `src_sys_time_lib_time_util`)
- **Date:** 2026-10-23
- **Changed:** `checks/commit_message_format.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

1. find_rdeps.sh and visibility_audit.sh computed narrow visibility
recommendations strictly from active callers in the dependency graph and
rejected pre-existing explicit narrow GN subtree/package visibility bounds (e.g.
a depth-3 //area/subcomponent/* scope in BUILD.gn when only one caller currently
depends on the target), forcing the coder to tighten developer-chosen visibility
and rejecting the restored scope with overbroad_subpackages_visibility. 2.
commit_message_format.sh counted cross-cutting language/chore sweep tags (such
as [rust], [rs], [flake], [cleanup]) in git log history even when not part of
the directory path, accepted build file paths like //<dir>/BUILD.gn in
Bazel-Migration-Target: footers because the label regex allowed uncolonized
slash/dot paths, and did not warn when a Test: fx test //<dir>:<target> footer
in a dual-build package referenced the GN test target instead of //<dir>:tests.

## Why not a one-off fix

Fixing only a single package leaves find_rdeps.sh and visibility_audit.sh
tightening deliberate narrow GN visibility scopes across all future migrations
with sparse current callers, and leaves commit_message_format.sh accepting build
file paths in Bazel-Migration-Target: footers, cross-cutting sweep tags in
commit subjects, and GN-only test target labels in dual-build fx test footers.
