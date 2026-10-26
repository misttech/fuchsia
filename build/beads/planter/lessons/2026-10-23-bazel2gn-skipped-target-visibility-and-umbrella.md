# Bazel2gn skipped target visibility and umbrella expanded area rollups

- **Learned from:** inner loop friction on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 9 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

First, visibility_audit assumed bazel2gn syncs every target's Bazel visibility
list into BUILD.gn, so uncovered_rdep_visibility checked GN callers against the
Bazel visibility of '# @bazel2gn:skip' targets (such as a Bazel alias whose GN
group counterpart is hand-written above the BAZEL2GN SENTINEL with unrestricted
GN visibility). Second, recommend_area_rollup_visibility emitted banned umbrella
wildcards for depth-2 non-src/sdk subtrees (such as third_party/rust_crates) and
visibility_audit capped globally-used target rollups at 15 entries instead of
max(15, len(area_rollup_vis)), rejecting minimal 2-3 level area rollups whose
entry count exceeds 15 solely because banned umbrella directories (src/lib,
src/testing, third_party/rust_crates) must be expanded into individual package
entries.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel cannot resolve the contradiction: any
dual-build package with a '# @bazel2gn:skip' target backed by an unrestricted
pre-sentinel GN target, or any widely-used library whose 2-3 level area rollup
exceeds 15 entries after expanding banned multi-domain umbrella directories,
will hit the same false-positive visibility_audit errors.
