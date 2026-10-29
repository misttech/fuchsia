# Bazel2gn no gn equivalent sentinel drift compatibility

- **Learned from:** inner loop friction on CL I6cfabf00ecba0f03ecb2720d36e57639b83181c8, patchset 4 (task `src-developer-ffx-plugins-platform`)
- **Date:** 2026-10-28
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When upstream build/tools/bazel2gn/constants.go maps rules such as ffx_tool and
idk_ffx_tool to noGNEquivalent, fx bazel2gn omits those targets below ##
BAZEL2GN SENTINEL in BUILD.gn without requiring # @bazel2gn:skip in BUILD.bazel.
While migration_sanity.sh already detected FFX_TOOL_NO_GN_EQUIV for
CONVERTIBLE_BAZEL_RULES, it did not emit non-blocking INFO sentinel_drift
compatibility entries for unskipped noGNEquivalent targets omitted below the
sentinel (nor did run_checks.sh downgrade them in standalone review_diff runs),
causing review_diff to report a false-positive sentinel_drift ERROR.

## Why not a one-off fix

Suppressing sentinel_drift only for a single ffx plugin directory would leave
every other dual-build package containing ffx_tool, idk_ffx_tool, or future
rules mapped to noGNEquivalent in build/tools/bazel2gn/constants.go blocked on
false-positive review_diff sentinel_drift errors after running fx bazel2gn.
