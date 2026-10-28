# Fidl library name to output name diffengine alignment

- **Learned from:** inner loop friction on a local task (task `b2-fidl1`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

In GN, fidl() targets whose FIDL library name differs from the target name set
name = "<library>", which diffengine normalizes to output_name when comparing
against Bazel. Because Bazel fidl_library() and zither_fidl_library() express
that value via library_name (and bazel2gn syncs library_name back to name in
BUILD.gn) rather than output_name, gn_attr_parity.sh previously recognized GN
name-as-output_name alignment only for Rust binary and test rules and missed
FIDL library rules, leaving review_diff with a blocking attribute_alignment
warning.

## Why not a one-off fix

Special-casing a single FIDL directory would leave every other GN fidl() target
whose library name differs from its target name blocked by the same diffengine
output_name attribute_alignment warning during Bazel migration. Extending the
rule-type check in gn_attr_parity.sh to include fidl_library and
zither_fidl_library resolves this GN name-to-Bazel library_name translation
across the entire tree.
