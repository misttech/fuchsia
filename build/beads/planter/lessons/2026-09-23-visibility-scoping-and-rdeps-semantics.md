# Visibility scoping and rdeps semantics

- **Learned from:** human review on [CL 1842102](https://fuchsia-review.googlesource.com/c/fuchsia/+/1842102), patchset 8 (task `cl-1842102`)
- **Date:** 2026-09-23
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`, `prompts/coder.md`, `prompts/panel.json`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`, `tools/find_rdeps.sh`, `tools/manifest.json`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (public visibility only)

## Status: partly superseded

Do not declare `"//visibility:public"` for tree-wide APIs: it is not allowed
outside `build/bazel_sdk/`
(docs/development/languages/bazel/style/common.md#do-not-use-public-visibility).
Use `["//:__subpackages__"]` or a few top-level directories instead
(docs/development/languages/bazel/style/build_files.md#target-visibility).
The rdeps semantics below still apply.

## Root cause

The migration machinery lacked rules and deterministic checks distinguishing
true reverse dependencies (deps, public_deps, test_deps) from visibility
allowlist references (such as visibility.gni or other targets' visibility
attributes), failed to forbid redundant ':__pkg__' entries added to BUILD.bazel
as a workaround for GN ':*' visibility, and allowed disguised repository-wide
wildcards ('//:__subpackages__' or enumerating 5+ top-level
'//<dir>:__subpackages__' directories) instead of either scoping to actual
callers or declaring target-level '//visibility:public' for genuine tree-wide
APIs.

## Why not a one-off fix

Manually editing the visibility lists in the four reviewed BUILD.bazel files
would leave the coder, reviewer seats, and check suite vulnerable to repeating
the same visibility scoping errors across all future Fuchsia GN-to-Bazel
migrations. Systemically preventing these defects requires upgrading the coder
and playbook instructions, equipping reviewers with an rdeps discovery tool that
filters out visibility allowlists, and enforcing a deterministic AST check
script (visibility_audit.sh) across all migrated BUILD.bazel files.
