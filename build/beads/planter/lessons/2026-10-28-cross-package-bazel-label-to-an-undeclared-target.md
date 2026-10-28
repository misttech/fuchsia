# Cross-package Bazel label to an undeclared target (missing cml shard for a .shard.cml include) in an existing package

- **Learned from:** inner loop friction on a local task (task `virt-cli-gpu`)
- **Date:** 2026-10-28
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

cq_reachability only checked that a dependency's package had a BUILD.bazel. A
label like //<pkg>:<stem>.shard in fx_component_manifest includes passed
whenever the package existed, even if it never declared that cml_shard. So the
defect only showed up later as an fx bazel build analysis error ('no such
target'), after costly build rounds, and coders treated it as a sibling's
problem instead of a dependency to fix.

## Why not a one-off fix

Any migrated component manifest that includes a shard from an already-migrated
SDK or library package which hasn't yet exposed that shard as a cml_shard hits
the same late analysis failure. Adding the one missing shard target fixes this
task only, while a static check catches every future case before the build and
gives the exact cml_shard to add.
