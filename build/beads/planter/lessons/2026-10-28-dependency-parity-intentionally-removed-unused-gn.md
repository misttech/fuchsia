# Dependency parity: intentionally removed unused GN dependencies in migrated C/C++ targets (especially Case 1 full BUILD.gn removal)

- **Learned from:** inner loop friction on a local task (task `b2-fidl1`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

gn_attr_parity only recorded the INFO dep_alignment coverage that review_diff
deduplicates against when every unmatched GN dependency had a mapping
(third_party_target_map, path_overwrite, aspect_hints, etc.). A GN dependency
that the migration intentionally dropped because the sources never use it had no
coverage path. The single unmapped label left the whole finding blocking, even
though the coder followed the prompt's rule to drop and name unused
dependencies, and with BUILD.gn deleted no path_overwrite could restore the
label.

## Why not a one-off fix

Any migration that drops an unused GN dependency from a C/C++ target hits the
same review_diff WARNING, and in Case 1 it can't be annotated away. Handling
only this directory would leave the prompt's unused-dependency rule
contradicting the review_diff check in every future task. The check now decides
it from the target's own #include lines.
