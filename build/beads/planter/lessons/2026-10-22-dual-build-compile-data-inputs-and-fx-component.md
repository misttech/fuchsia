# Dual build compile data inputs and fx component attribute alignment

- **Learned from:** inner loop friction on a local task (task `swd-pkg-system-updater`)
- **Date:** 2026-10-22
- **Changed:** `checks/migration_sanity.sh`

## Root cause

In checks/migration_sanity.sh, record_compile_data_and_env_compat_info only
extracted quoted string literals from GN inputs = [...] lists and only matched
them against literal string constants in Bazel compile_data/data. When a GN
target specified an unquoted variable identifier in inputs (such as inputs = [
<var> ] generated via # @bazel2gn:raw_overwrite:[ <var> ] on compile_data or
defined as a top-level GN path variable), migration_sanity failed to emit the
corresponding INFO missing_sources coverage entry, leaving review_diff's
missing_sources error unsuppressed.

## Why not a one-off fix

Hardcoding a target-specific exception would fail for any other dual-build or
migrated target that uses GN path variables or @bazel2gn:raw_overwrite on
compile_data/data to map generated files to GN inputs. Parsing bare GN
identifiers in inputs and matching them against @bazel2gn overwrite directives
and top-level GN variable assignments generalizes across all packages.
