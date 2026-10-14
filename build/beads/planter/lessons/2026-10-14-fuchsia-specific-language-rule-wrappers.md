# Fuchsia specific language rule wrappers

- **Learned from:** human review on CL I8dde786578f53369bf5a0e89a7a953b79ed378a2, patchset 1 (task `devices-lib-log-bazel`)
- **Date:** 2026-10-14
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

The coder prompt presented raw @rules_cc cc_library alongside fx_cc_library as
interchangeable options for GN source_set/static_library migrations, and
bazel_minimality had no check flagging unwrapped external C/C++ or Rust rules
(cc_library, cc_binary, rust_library, rust_binary, rust_proc_macro, rust_test,
rust_shared_library, rust_dylib_library) when Fuchsia-specific wrapper macros
(fx_cc_library, fx_cc_binary, rustc_*) exist under //build/bazel/rules/.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel leaves every future C/C++ and Rust
library or binary migration vulnerable to importing raw @rules_cc// or
@rules_rust// rules instead of the Fuchsia-specific wrapper macros required by
the Fuchsia Bazel style guide to apply platform build flags and toolchain
configurations consistently.
