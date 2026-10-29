# Visibility audit test rule recognition

- **Learned from:** inner loop friction on a local task (task `b3-sdkf1`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`

## Root cause

visibility_audit.sh did not classify fx_package(test_components=...) or
fx_component_manifest(testonly=True) as test rules, contradicting
migration_sanity.sh's rule to omit visibility on test plumbing targets.

## Why not a one-off fix

Hardcoding visibility for this specific target would leave visibility_audit.sh
and migration_sanity.sh in contradiction for all explicit fx_package test
packages globally.
