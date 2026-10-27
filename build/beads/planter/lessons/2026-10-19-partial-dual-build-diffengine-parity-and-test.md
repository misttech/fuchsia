# Partial dual-build diffengine parity and Test: footer verification when tests.json is unpopulated

- **Learned from:** inner loop friction on CL Icafd4780c5159f1d1713dee3a2e5e4e9bd516e8e, patchset 1 (task `pkg-system-updater`)
- **Date:** 2026-10-19
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/migration_sanity.sh`

## Root cause

In partial dual-build migrations where GN-only actions, test binaries, or
package targets remain above the BAZEL2GN SENTINEL, diffengine positional
pairing and gn_attr_list_items missed bare GN variable identifiers in
inputs/sources lists and non-candidate changed directories when pre-populating
INFO findings in migration_sanity. Additionally, commit_message_format and
build_verification remediation strings did not clarify that recording fx bazel
test --config=host or fx bazel build --config=fuchsia_platform instead of fx
test when <build dir>/tests.json lacks the target is the required convention
rather than a machinery_conflict.

## Why not a one-off fix

Any partial dual-build package that uses GN variable identifiers in
inputs/sources, modifies helper BUILD.bazel files outside the primary target
directory, or migrates tests in a build configuration where <build
dir>/tests.json is empty will encounter the same diffengine false positives and
Test: footer confusion unless migration_sanity, commit_message_format, and
build_verification handle them generally.
