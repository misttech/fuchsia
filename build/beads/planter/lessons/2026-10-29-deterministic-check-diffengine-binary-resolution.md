# Deterministic check diffengine binary resolution

- **Learned from:** inner loop friction on a local task (task `pkg-blobfs-test`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

checks/gn_attr_parity.sh called find_planter_bin() inside
collect_diffengine_alignment_findings() without defining the helper function,
causing gn_attr_parity to crash with NameError before emitting its JSON findings
(and preventing review_diff deduplication of sentinel-paired attribute_alignment
and dropped_comment warnings).

## Why not a one-off fix

Collect_diffengine_alignment_findings() runs at top level for every GN-to-Bazel
migration checked by gn_attr_parity.sh; defining find_planter_bin() in
checks/gn_attr_parity.sh prevents the crash and restores diffengine alignment
deduplication across all migration tasks.
