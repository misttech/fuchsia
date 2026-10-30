# Shac Bazel style lint parity (bzl load visibility, workspace-root package labels, public target visibility) on migrated BUILD.bazel/.bzl files

- **Learned from:** cq failure on [CL 1932170](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932170), patchset 2 (task `htt-system-tests-bazel`)
- **Date:** 2026-10-29
- **Changed:** `checks/bazel_shac_lint.sh`, `checks/manifest.json`

## Root cause

The shac Bazel lint rules (bazel_bzl_require_load_visibility,
bazel_disallowed_workspace_root_package_labels, bazel_target_visibility) were
only exercised by the full shac run inside build_verification, which can be
skipped or run late; no fast static check ran the checkout's
scripts/shac/bazel_lint rules, so a new .bzl macro file without visibility() and
'//:<dir>/<file>' labels reached CQ static-checks.

## Why not a one-off fix

Any migration that ports a .gni template to a .bzl macro or references files in
directories without a BUILD.bazel can hit these same shac rules; fixing only
this package's labels and .bzl leaves every future migration exposed, whereas
running the checkout's own bazel_lint rules on every changed Bazel file catches
all current and future shac Bazel rules statically.
