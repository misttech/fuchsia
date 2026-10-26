# Non blocking fyi coverage builder on unbuilt rust bitrot fix

- **Learned from:** inner loop friction on CL Idf93c5351e44d4bdcf56467d632edd9268390ab5, patchset 2 (task `src_sys_time_time_test_client`)
- **Date:** 2026-10-22
- **Changed:** `checks/build_only_scope.sh`, `prompts/coder.md`

## Root cause

When a migration repairs pre-existing compile bitrot in a .rs source file
belonging to a package that was disconnected from the GN build graph and has no
affected GN tests producing llvm-profile sinks, the non-blocking experimental
FYI builder fuchsia-coverage (cq_experimental=true, is_fyi=true) fails
Step('covargs') with 'llvm-profdata merge: error: no input files specified'. The
machinery lacked explicit guidance in build_only_scope and coder.md instructing
the coder not to modify files or upload a new patchset (which would invalidate
an existing Code-Review+2 vote) when CQ remediation is triggered solely by this
FYI builder failure.

## Why not a one-off fix

Any GN-to-Bazel migration of a previously unbuilt Rust target requiring minimal
source bitrot repairs without affected GN tests will trigger the same covargs
failure on the experimental FYI fuchsia-coverage builder, so documenting this
handling in build_only_scope and coder.md (while shrinking coder.md) is required
across all future migrations.
