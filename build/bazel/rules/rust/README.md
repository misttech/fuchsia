# Fuchsia Platform Rust Bazel Rules and Macros

This directory contains Fuchsia-specific Rust wrapper macros, rules, and
helpers for platform builds.

See [Use Fuchsia-specific wrappers](/docs/development/languages/bazel/style/common.md#use-fuchsia-specific-wrappers)
for general usage guidance. Load symbols directly from their individual `.bzl`
files rather than the deprecated [`defs.bzl`](./defs.bzl).

## Contents

### Wrapper Macros and Rules

These wrappers apply Fuchsia-specific compiler flags, lint configurations, RBE
properties, and standard library link dependencies, and are enforced by the
`bazel_fuchsia_wrappers` SHAC check:

* [`rustc_binary.bzl`](./rustc_binary.bzl): Defines `rustc_binary()`, Fuchsia's
  wrapper macro for `@rules_rust`'s `rust_binary()`.
* [`rustc_dylib.bzl`](./rustc_dylib.bzl): Defines `rustc_dylib()`, Fuchsia's
  wrapper macro for `@rules_rust`'s `rust_dylib_library()`.
* [`rustc_library.bzl`](./rustc_library.bzl): Defines `rustc_library()`,
  Fuchsia's wrapper macro for `@rules_rust`'s `rust_library()`.
* [`rustc_library_group.bzl`](./rustc_library_group.bzl): Defines
  `rustc_library_group()`, Fuchsia's rule equivalent to `@rules_rust`'s
  `rust_library_group()` with support for native `link_deps`.
* [`rustc_proc_macro.bzl`](./rustc_proc_macro.bzl): Defines `rustc_proc_macro()`,
  Fuchsia's wrapper macro for `@rules_rust`'s `rust_proc_macro()`.
* [`rustc_test.bzl`](./rustc_test.bzl): Defines `rustc_test()`, Fuchsia's
  wrapper macro for `@rules_rust`'s `rust_test()`.

### Additional Macros and Helpers

* [`common.bzl`](./common.bzl): Shared helper functions used by the `rustc_*()`
  wrapper macros (e.g., `with_fuchsia_rustc_flags()`,
  `with_fuchsia_rust_stdlib_link_deps()`).
* [`defs.bzl`](./defs.bzl): **Deprecated** re-export file. Do not load from or
  add to this file; load from the individual `.bzl` files instead.
* [`generate_unit_tests.bzl`](./generate_unit_tests.bzl): Generates implicit
  unit test targets for legacy `with_unit_tests` and `with_host_unit_tests`
  attributes.
* [`rustc_bindgen.bzl`](./rustc_bindgen.bzl): Defines `rustc_bindgen_golden()`
  and related rules/macros for running bindgen on C headers and verifying golden
  Rust bindings.
* [`rustc_check_cfg_build_flags.bzl`](./rustc_check_cfg_build_flags.bzl): Defines
  `rustc_check_cfg_build_flags()`, a macro generating a `build_flags()` target
  with `--check-cfg` flags.
* [`rustc_embed_files.bzl`](./rustc_embed_files.bzl): Defines
  `rustc_embed_files()`, a macro for embedding files as byte array constants in
  a Rust library.

## Adding a New Wrapper Macro or Rule

When adding a new Fuchsia wrapper macro or rule equivalent for an upstream
`@rules_rust` symbol (for example, `rustc_library_group()` for
`rust_library_group()`):

1. **Follow Fuchsia naming and style guidelines**:
   * See [Wrapping built-in and common rules, macros, and functions](/docs/development/languages/bazel/style/rules_macros.md#wrapping-built-in-and-common-rules-macros-and-functions).
   * Prefix the wrapper name with `rustc_` (never `fuchsia_`) and define it in
     `//build/bazel/rules/rust/<wrapper_name>.bzl`. Do not add it to
     [`defs.bzl`](./defs.bzl).
2. **Register the wrapper in the `bazel_fuchsia_wrappers` SHAC check**:
   * Add an entry to `_WRAPPED_RULES` in
     [`//scripts/shac/bazel_lint/checks/fuchsia_wrappers.py`](/scripts/shac/bazel_lint/checks/fuchsia_wrappers.py)
     mapping the upstream `@rules_rust//` symbol to the new wrapper symbol and
     module label.
3. **Add unit tests for the SHAC check**:
   * Update [`//scripts/shac/bazel_lint/checks/tests/fuchsia_wrappers_test.star`](/scripts/shac/bazel_lint/checks/tests/fuchsia_wrappers_test.star)
     to verify that loading or calling the upstream symbol (including aliased
     loads) is flagged as an error, and that loading and calling the new wrapper
     is allowed.
4. **Migrate or allowlist pre-existing uses**:
   * Run `fx host-tool shac check --only bazel_fuchsia_wrappers --all` across
     the repository.
   * Migrate existing in-tree uses to the new wrapper where feasible, or add
     remaining pre-existing violations to
     [`//scripts/shac/bazel_lint/allowlists/bazel_fuchsia_wrappers.json`](/scripts/shac/bazel_lint/allowlists/bazel_fuchsia_wrappers.json)
     under `https://fxbug.dev/570203009`, following the sorting and formatting
     rules in
     [`//scripts/shac/bazel_lint/allowlists/README.md`](/scripts/shac/bazel_lint/allowlists/README.md).
5. **Update documentation**:
   * Update [`//docs/development/languages/bazel/style/common.md`](/docs/development/languages/bazel/style/common.md#use-fuchsia-specific-wrappers)
     if the new wrapper introduces a new naming pattern or module path.
   * Update [`//docs/development/languages/bazel/style/rules_macros.md`](/docs/development/languages/bazel/style/rules_macros.md#wrapping-built-in-and-common-rules-macros-and-functions)
     if the new wrapper introduces a new naming convention or exception.
   * Document the new `.bzl` file and symbol in the
     [Wrapper Macros and Rules](#wrapper-macros-and-rules) section above.
6. **Verify tests and checks**:
   * Run `fx test --host @//scripts/shac/tests:bazel_test`.
   * Run `fx host-tool shac check --only bazel_fuchsia_wrappers --all`.
   * Run `fx doc-checker --local-links-only`.
