# GN cc_test_executable() keeps its LTO config when it becomes fx_cc_binary

- **Learned from:** review of https://fxrev.dev/1930390
- **Date:** 2026-10-30
- **Changed:** `prompts/coder.md`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

GN `cc_test_executable()` (`//build/testing/cc_test_executable.gni`) is an `executable()` that
also adds `configs += [ "//build/config/lto:no-fat-lto-objects-linking" ]` unless it sets
`disable_lto = false`. The coder instructions mapped it to a plain
`fx_cc_binary(testonly = True)`, which bazel2gn converts to an `executable()` without that config,
and `gn_target_type_parity` reported the conversion as INFO `cc_test_executable_converted` with
"None needed". Both the GN copy and the Bazel binary silently stopped linking with
`--no-fat-lto-objects`, which builds and tests do not catch.

## Why not a one-off fix

Every directory with a GN `cc_test_executable()` hits the same path. The executable type is right
(`fx_cc_test` would become GN `test()`, which installs to `test/` instead of `bin/` and adds a
host test), so the fix keeps `fx_cc_binary(testonly = True)` and adds
`build_flags = ["//build/config/lto:no-fat-lto-objects-linking"]`, which bazel2gn turns into
`configs +=`. `gn_target_type_parity` now reports ERROR `cc_test_executable_lost_lto_config` when
the generated `executable()` lacks the config and the GN target did not set
`disable_lto = false`. A dedicated Bazel macro was not added, because `build_flags` already gives
exact parity.
