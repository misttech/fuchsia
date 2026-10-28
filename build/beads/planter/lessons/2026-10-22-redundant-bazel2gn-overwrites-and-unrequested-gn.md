# Redundant bazel2gn overwrites and unrequested gn conditions

- **Learned from:** human review on CL I000b341549b17aa703ec659e9f6acbf5d6ec4e9b, patchset 13 (task `src_lib_analytics_cpp_core_dev_tools`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

First, bazel_minimality skipped @bazel2gn:path_overwrite annotations on external
(@...) and //third_party/... labels and never checked whether an overwrite
annotation was placed inside a rule that bazel2gn does not translate (such as
host_cc_test) or duplicated an existing mapping in
build/tools/bazel2gn/third_party_target_map.json. Second, gn_attr_parity checked
for dropped GN-only target attributes but did not check whether retained
hand-written GN targets or group("tests") above the BAZEL2GN SENTINEL were newly
wrapped in platform or toolchain conditions (such as if (is_host)) that were
absent at the change base.

## Why not a one-off fix

Fixing only the single package leaves future migrations free to attach no-op
@bazel2gn:path_overwrite or @bazel2gn:raw_overwrite comments to untranslated
test/package rules or already-mapped third-party dependencies, and to wrap
retained hand-written GN targets or group("tests") above the sentinel in
unrequested if (is_host) or if (is_fuchsia) blocks across the tree.
