# Bazel visibility scoping and reviewer check alignment

- **Learned from:** inner loop friction on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 9 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `prompts/panel.json`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`, `tools/manifest.json`

## Root cause

While visibility_audit.sh, find_rdeps.sh, and review_diff disallow
'//visibility:public' outside '//build/bazel_sdk/...' and accept area-rollup
visibility lists from find_rdeps (even when expanded non-umbrella entries under
//src/lib, //sdk/lib, //src/testing, or //third_party/rust_crates exceed 15
items, or '//:__subpackages__' for foundational tree-wide libraries), stale
duplicated rules in dual_build_sentinel.md, target_parity.md,
checks/manifest.json, and tools/manifest.json still instructed reviewers to
reject >15-entry lists on per_target_is_globally_used targets and require
'visibility = ["//visibility:public"]'.

## Why not a one-off fix

Fixing only a single library package leaves the contradictory
'//visibility:public' rule in the reviewer prompts and manifests, causing the
reviewer panel to reject valid find_rdeps area-rollup or '//:__subpackages__'
visibility lists on every future widely-used platform library migration.
