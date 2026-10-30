# Scope .gni reverse-dependency attribution to callers invoking the enclosing template

- **Date:** 2026-10-29
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

Two `.gni` attribution bugs produced hundreds of false-positive GN callers:
1. `gni_importer_pkgs` attributed labels inside `template("<name>") { ... }` in a
   non-`build/` `.gni` file to every `BUILD.gn` transitively importing that `.gni`
   (including all >1,600 `BUILD.gn` files importing `bazel_migration.gni`),
   even when the `BUILD.gn` never invoked `<name>(...)`.
2. `collect_gni_template_matches` matched bare `.gni` basenames such as
   `"config.gni"` across unrelated directories and included indented target
   attributes like `deps = [...]` inside `template(...)` bodies in `v_names`,
   attributing `.gni` dependencies to every `BUILD.gn` importing its own local
   `config.gni`.

## Why not a one-off fix

Any migrated library referenced inside a shared `.gni` template or a local
`config.gni` hits false-positive `uncovered_rdep_visibility` errors unless `.gni`
imports are resolved relative to the importing directory and scoped to callers of
the enclosing `template(...)` or top-level variable.
