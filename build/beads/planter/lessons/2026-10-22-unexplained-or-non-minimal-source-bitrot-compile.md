# Unexplained or non-minimal source bitrot compile fixes in unbuilt GN packages during Bazel migration

- **Learned from:** human review on CL Idf93c5351e44d4bdcf56467d632edd9268390ab5, patchset 1 (task `src_sys_time_time_test_client`)
- **Date:** 2026-10-22
- **Changed:** `checks/build_only_scope.sh`, `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

While build_only_scope permitted source compile-fix edits in packages
disconnected from the configured GN build graph (ninja_outputs.json) and
prompts/coder.md asked to note them in the commit body, commit_message_format
only verified commit body explanations for .bzl and MODULE.bazel files outside
the migrated directory. It did not verify that modified source files (.rs, .cc,
.h, etc.) inside an unbuilt package were mentioned and explained as pre-existing
compile bitrot fixes in the commit body, nor did build_only_scope caution
against reformatting surrounding expressions (such as moving and reformatting a
multi-line closure when converting a misplaced inner /// doc comment to //
suffices). Additionally, commit_message_format's redundant-body regex missed
'Migrate //<dir> from GN to Bazel and generate the corresponding GN targets
using bazel2gn' and its Bazel-Migration-Target validator accepted
'//<dir>/BUILD.gn' file paths.

## Why not a one-off fix

Addressing only a single package leaves all future migrations of unbuilt GN
packages vulnerable to uploading source bitrot fixes (such as Rust E0658
expression-attribute errors or outdated API types) with generic boilerplate
commit messages and unnecessary expression reformatting that look like unrelated
drive-by source edits to human reviewers.
