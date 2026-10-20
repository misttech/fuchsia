# Bazel test crate reuse visibility and build flags ifttt hygiene

- **Learned from:** human review on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 1 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/bazel_minimality.sh`, `checks/shared_config_reuse.sh`, `checks/visibility_audit.sh`

## Root cause

Existing static checks lacked rules for three recurring BUILD.bazel defects: (1)
shared_config_reuse.sh did not verify that hand-maintained build_flags() targets
mirroring hand-written GN config() blocks carry bidirectional
LINT.IfChange/LINT.ThenChange markers, (2) bazel_minimality.sh did not flag
rustc_test unit-test targets that repeat a sibling library's srcs, deps,
proc_macro_deps, and crate_name instead of setting crate = ':<lib>', and (3)
visibility_audit.sh skipped '//visibility:private' instead of flagging it as
redundant when package default_visibility is unset.

## Why not a one-off fix

Fixing only the single package leaves all future Rust library-variant migrations
and shared build_flags() migrations vulnerable to repeating library
sources/deps/crate_name in rustc_test, emitting redundant '//visibility:private'
attributes, and omitting LINT.IfChange/LINT.ThenChange on dual-maintained
build_flags()/config() pairs.
