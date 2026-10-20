# Bazel2gn convertible rustc check cfg build flags parity

- **Learned from:** inner loop friction on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 1 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh omitted rustc_check_cfg_build_flags from
CONVERTIBLE_BAZEL_RULES and ignored check-cfg config() definitions above the
BAZEL2GN SENTINEL in BUILD.gn, even though bazel2gn natively translates
rustc_check_cfg_build_flags into a GN config() below the sentinel. When the
coding agent annotated rustc_check_cfg_build_flags with # @bazel2gn:skip and
placed a manual config() with --check-cfg=cfg(...) rustflags above the sentinel,
migration_sanity and verify_bazel2gn both passed silently, deferring detection
to the review panel.

## Why not a one-off fix

Any dual-build Rust crate migration that defines check_cfgs or features via
rustc_check_cfg_build_flags can hit the same false negative if migration_sanity
does not treat rustc_check_cfg_build_flags and its generated check-cfg GN
config() as bazel2gn-convertible targets.
