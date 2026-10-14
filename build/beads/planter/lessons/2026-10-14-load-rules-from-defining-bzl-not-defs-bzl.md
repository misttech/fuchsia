# Load rules from their defining .bzl, not defs.bzl

- **Learned from:** human review on CL 1884660, patchset 4 (task `bt-avrcp-vol-control`)
- **Date:** 2026-10-14
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

The coder prompt (the `rustc_binary` load example and the standalone
`rustc_test` guidance) and the `migration_sanity` remediation for
`with_unit_tests` told migrations to load rules from
`//build/bazel/rules/rust:defs.bzl`, and no check flagged loads from `defs.bzl`
files, whereas human reviewers require loading each symbol from the `.bzl` file
that defines it (e.g. `@rules_cc//cc:cc_library.bzl` rather than
`@rules_cc//cc:defs.bzl`, `//build/bazel/rules/rust:rustc_test.bzl` rather than
`//build/bazel/rules/rust:defs.bzl`) whenever a `defs.bzl` only re-exports the
symbol from a `.bzl` file in the same directory, using a `defs.bzl` only when
the symbol is first defined there (e.g. `//build/bazel/rules/host:defs.bzl`)
and never loading from a `private/` file.

## Why not a one-off fix

Fixing only the reviewed `BUILD.bazel`, or matching only
`//build/bazel/rules/rust:defs.bzl`, leaves C++, Rust and other migrations free
to load re-exported rules from `@rules_cc//cc:defs.bzl`,
`@rules_rust//rust:defs.bzl` or any other re-exporting `defs.bzl`.
`migration_sanity` now resolves any `*:defs.bzl` label (in the tree or under
`third_party/bazel_vendor/<repo>+`), parses it to find symbols re-exported from
same-directory `.bzl` files, reports `defs_bzl_reexport_load` with the exact
defining load as the remediation, and allows `//build/bazel/rules/host:defs.bzl`
and symbols that `defs.bzl` defines itself or loads from `private/`.
