# In file gn template expansion and sentinel diff parity

- **Learned from:** inner loop friction on a local task (task `src-sys-run-test-suite`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`, `tools/run_checks.sh`

## Root cause

When a package's base BUILD.gn defines a file-local template("<tmpl>") wrapping
an inner target rule such as rustc_library(target_name) (setting shared
crate_name, sources, deps, and features) and instantiates <tmpl>("<name>")
multiple times, migrating to BUILD.bazel with bazel2gn requires expanding the
deleted file-local template into explicit target rules below ## BAZEL2GN
SENTINEL sharing Starlark list variables. While gn_target_type_parity.sh already
resolved file-local single-target templates to their inner rule type, diffengine
(review_diff) did not: it paired the inner rule inside template() with the first
Bazel target via crate_name and paired the remaining template instantiations
with the expanded Bazel targets, producing false-positive template_parity,
unmatched_target, dep_alignment, and attribute_alignment findings that
gn_attr_parity.sh did not cover with INFO reconciliation.

## Why not a one-off fix

Hardcoding exemptions for a single package or template name would fail on any
other Fuchsia package that defines a file-local GN template() to share common
sources, dependencies, or feature flags across multiple target variants (such as
standard and fdomain or host/device library variants) and expands them into
standard rules in BUILD.bazel and below the bazel2gn sentinel.
