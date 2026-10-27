# Relative gn rdep discovery and host test bazel2gn skip alignment

- **Learned from:** inner loop friction on a local task (task `pkg-htt-paver`)
- **Date:** 2026-10-19
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`, `tools/find_rdeps.sh`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (test skips only)

## Status: partly superseded

The `# @bazel2gn:skip` guidance on `host_*_test` rules was superseded when
`bazel2gn` enabled translating test rules (`host_go_test` and `host_rustc_test`
generate GN copies without `# @bazel2gn:skip`). Sibling/cousin `../<name>`
relative GN label discovery in `visibility_audit.sh` and `find_rdeps.sh` still
applies.

## Root cause

visibility_audit.sh and find_rdeps.sh only searched for absolute //<pkg> and
ancestor-to-descendant relative GN labels (ignoring sibling/cousin ../<name> GN
labels and worktree .git files), causing visibility_audit to reject valid
subtree visibility as a false positive when sibling packages depend on ../<name>
in BUILD.gn. In addition, coder.md and case2_dual_build.md instructed omitting #
@bazel2gn:skip on host_*_test rules, conflicting with review_diff's dual-build
target alignment checks.

## Why not a one-off fix

Hardcoding a single package's visibility or test target name would fail on every
other multi-package subsystem where sibling BUILD.gn files use ../<subpkg>
relative labels or define host_*_test targets in dual-build packages; updating
relative GN label discovery in visibility_audit.sh and find_rdeps.sh and
aligning # @bazel2gn:skip guidance on host_*_test rules fixes the entire class
of migrations.
