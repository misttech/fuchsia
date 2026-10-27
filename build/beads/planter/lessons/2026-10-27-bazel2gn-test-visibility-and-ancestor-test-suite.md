# Bazel2gn test visibility and ancestor test suite reachability

- **Learned from:** human review on CL Ic5bb4e10a944dd20a41c157911fdb6208c7aad15, patchset 3 (task `src-developer-ffx-lib-diagnostics-analytics`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `checks/visibility_audit.sh`, `prompts/playbooks/case2_dual_build.md`, `tools/find_rdeps.sh`

## Root cause

First, find_rdeps.sh and visibility_audit.sh classified bazel2gn-synced test
rules (rustc_test, host_rustc_test, go_test, host_go_test) in
BAZEL_NON_GN_SYNCED_RULES, which caused them to ignore external BUILD.gn callers
that depend directly on the generated GN test target (e.g., a parent
group("tests") depending on "<subpkg>:lib_test" rather than "<subpkg>:tests")
and falsely reject visibility entries for those GN callers as
false_positive_rdep_visibility, even though bazel2gn copies visibility from the
Bazel test rule onto the generated GN test target. Second, cq_reachability.sh
assumed any local group("tests") above BAZEL2GN SENTINEL in a package's BUILD.gn
was reachable in the GN build graph even when no ancestor BUILD.gn or .gn/.gni
file referenced //<pkg>:tests (because an ancestor BUILD.gn referenced
//<pkg>:<test_target> directly), and its ancestor test_suite remediation neither
instructed removing the unreachable local group("tests") nor included direct GN
test callers in the recommended test target visibility.

## Why not a one-off fix

Many Fuchsia subsystems (such as ffx libraries and subtools) aggregate child
unit tests by depending directly on "<child>:lib_test" in an intermediate
BUILD.gn group("tests") while exporting Bazel tests through an area-level
test_suite in an ancestor BUILD.bazel. Fixing only a single package leaves
visibility_audit and find_rdeps rejecting required GN caller visibility on
bazel2gn-synced test targets and leaves cq_reachability permitting unreachable
local group("tests") and bazel_test_suite targets across all similarly
structured packages.
