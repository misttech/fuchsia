# Bazel dependency package existence and stacked ancestry verification

- **Learned from:** cq failure on CL Ib7799fbf9e2961e87bb3740435eed930c8b89743, patchset 1 (task `src-developer-ffx-lib-target`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When a migrated BUILD.bazel references a main-repository //package:target
dependency whose BUILD.bazel is missing from the checkout or provided by an
unmerged stacked parent commit that was dropped during rebase, CQ tests.json
generation (bazel cquery) fails with 'no such package'. Extending DEP_ATTRS in
cq_reachability.sh ensures non_test_deps, actual, tools, and manifest label
attributes are also audited alongside standard dependency attributes.

## Why not a one-off fix

Fixing or rebasing only a single package directory does not prevent future
stacked migrations or BUILD.bazel rules using non_test_deps, alias(actual),
genrule(tools), or component manifest labels from referencing unmigrated or
dropped parent Bazel packages.
