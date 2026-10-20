# Bazel2gn check cfg build flags diffengine alignment parity

- **Learned from:** inner loop friction on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 1 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

While migration_sanity requires migrating GN check-cfg config() targets to
rustc_check_cfg_build_flags() in BUILD.bazel without # @bazel2gn:skip so
bazel2gn generates the config() below the sentinel, the built-in diffengine
(review_diff) lacked a rule-mapping and attribute exemption for config() ->
rustc_check_cfg_build_flags(cfgs, features) replacing --check-cfg rustflags,
emitting two false-positive blocking attribute_alignment warnings unless
pre-populated as non-blocking INFO coverage by a check.

## Why not a one-off fix

Any Rust package that defines a local GN config() with --check-cfg=cfg(...)
rustflags and migrates it to rustc_check_cfg_build_flags() in BUILD.bazel will
trigger the exact same pair of false-positive review_diff attribute_alignment
warnings unless migration_sanity and run_checks.sh systemically record
non-blocking INFO coverage for rustc_check_cfg_build_flags targets.
