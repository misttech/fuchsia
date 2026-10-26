# Config to build flags attribute alignment

- **Learned from:** inner loop friction on a local task (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-20
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

In checks/gn_attr_parity.sh, an `if tmpl.startswith("fuchsia_") and
bt_rule.startswith("fx_"):` block was inserted inside the `if tmpl == "config"
and bt_rule == "build_flags":` block ahead of the `gn_rf`/`configs` attribute
alignment logic, causing the `configs in GN not carried over to Bazel` INFO
coverage to only run for `fuchsia_*` -> `fx_*` targets instead of `config` ->
`build_flags` targets.

## Why not a one-off fix

Suppressing the warning for a single target name or package leaves any package
that defines or touches a GN `config()` paired with a Bazel `build_flags()`
target vulnerable to false-positive `attribute_alignment` warnings whenever GN
`configs` or `rustflags` are expressed via `build_flags` attributes.
