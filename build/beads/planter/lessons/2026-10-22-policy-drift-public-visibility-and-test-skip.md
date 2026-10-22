# Reviewer prompts and checks drifted from settled visibility and test policy

- **Learned from:** audit of planter machinery against the settled GN-to-Bazel
  migration decisions (https://fxbug.dev/574121341)
- **Date:** 2026-10-22
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`,
  `prompts/reviewers/dual_build_sentinel.md`,
  `prompts/reviewers/target_parity.md`, `tools/manifest.json`

## Root cause

Rules written before three decisions settled were never updated, so agents got
instructions that contradict the style guide:

1. **Public visibility.** `prompts/reviewers/target_parity.md` (items 3 and 5)
   and `prompts/reviewers/dual_build_sentinel.md` (items 4 and 5) told
   reviewers to *require* `visibility = ["//visibility:public"]` on globally
   used targets ("just make it public if it is well and truly used
   everywhere") and to reject `"//:__subpackages__"` on them. The
   `visibility_audit` description in `checks/manifest.json` and the
   `find_rdeps` description in `tools/manifest.json` said the same thing, even
   though both scripts already recommend `["//:__subpackages__"]`. Public
   visibility is not allowed outside `build/bazel_sdk/`, including on exported
   test targets (`fx_test`, `fx_unittest_package`, `wrap_host_*`), and
   `"//:__subpackages__"` or a few top-level directories are the documented
   choice for codebase-wide targets.
2. **Umbrella wildcards on globally used targets.** `visibility_audit.sh`
   errored on `//sdk/lib:__subpackages__` even for globally used targets,
   although the style guide gives
   `["//sdk/lib:__subpackages__", "//src:__subpackages__"]` as its example for
   those. The check can't tell whether such a target is truly codebase-wide, so
   it now reports INFO for a human to confirm, and still errors on targets
   that are not globally used.
3. **Tests of dual-built libraries.** The Planter `review_diff` diffengine
   (outside this tree) still had the "tests move to Bazel, so every test in a
   dual-build `BUILD.bazel` needs `# @bazel2gn:skip`" rule. The 2026-10-16
   lesson `2026-10-16-dual-build-host-test-bazel2gn-skip-parity.md` was written
   to satisfy that rule and is stale. Tests of dual-built targets are defined
   without `# @bazel2gn:skip` by default, so bazel2gn generates GN copies.
   They are skipped only when the target under test is Bazel-only, bazel2gn
   can't translate the test, or dual-testing costs more than the risk it
   covers. The in-tree prompts, `migration_sanity.sh` and `cq_reachability.sh`
   already followed this decision and are unchanged.

`HOST_OS_CONSTRAINTS` from `//build/bazel/platforms:constraints.bzl` (not
`HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl`) was already the
rule everywhere in this tree. Only the diffengine remediation text was stale.

## Remediation

Cite the canonical docs instead of restating thresholds that can drift:

- docs/development/languages/bazel/style/common.md#do-not-use-public-visibility
- docs/development/languages/bazel/style/build_files.md#target-visibility
- docs/development/languages/bazel/style/build_files.md#target-compatible-with
- docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems
- `build/beads/references/target_compatible_with.md`

## Why not a one-off fix

Reviewer prompts run on every migration. As long as a prompt told reviewers to
require public visibility, any correct `["//:__subpackages__"]` or rollup in a
CL could be rejected, and the coder would then add the visibility that the
`bazel_lint` shac check and human reviewers reject.
