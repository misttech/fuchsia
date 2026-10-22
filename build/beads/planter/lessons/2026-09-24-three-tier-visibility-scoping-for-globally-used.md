# Three tier visibility scoping for globally used platform libraries

- **Learned from:** human review on [CL 1842102](https://fuchsia-review.googlesource.com/c/fuchsia/+/1842102), patchset 12 (task `cl-1842102`)
- **Date:** 2026-09-24
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`, `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`, `tools/find_rdeps.sh`, `tools/manifest.json`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md`

## Status: superseded

Do not follow the `//visibility:public` tier of this lesson.
`"//visibility:public"` is not allowed outside `build/bazel_sdk/`
(docs/development/languages/bazel/style/common.md#do-not-use-public-visibility).
Globally used targets use a 2-3 level area rollup, a `package_group`, a few
top-level directories, or `["//:__subpackages__"]`
(docs/development/languages/bazel/style/build_files.md#target-visibility).
The rest of the three-tier scoping still applies. The text below is kept only
as history.

## Root cause

Existing visibility rules and checks treated every non-private target uniformly
by forbidding target-level //visibility:public whenever reverse dependencies
were enumerable, without distinguishing narrow subsystem targets from
globally-used internal platform/SDK-like libraries and test utilities depended
upon across >= 6 top-level areas (> 15 narrow entries / >= 20 rdep packages).
This forced coding agents to replace package-level public visibility with
sprawling 30-50+ line allowlists of leaf :__pkg__ and deep :__subpackages__
entries.

## Why not a one-off fix

Updating only a single BUILD.bazel file leaves find_rdeps, visibility_audit.sh,
coder.md, playbooks, and reviewer prompts unaware of the 3-tier visibility
hierarchy (narrow subsystem scoping vs. 2-3 level area rollups vs. target-level
//visibility:public for globally-used libraries), causing future migrations of
widely-used platform/testing crates across the repository to continue generating
unmaintainable 30-50+ line visibility allowlists.
