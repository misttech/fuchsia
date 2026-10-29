# Dual build cc unit test and test package parity

- **Learned from:** inner loop friction on a local task (task `b3-libc1`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_dep_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`, `checks/tests_json_parity.sh`

## Root cause

Three static parity checks had overly narrow assumptions for dual-build C/C++
unit test migrations: (1) gn_target_type_parity emitted gn_type_changed INFO on
cc_test_executable() (and GN test() when fx_cc_test.bzl is unavailable) when
fx_cc_binary(testonly = True) is translated by bazel2gn into executable() with
testonly = true, without emitting matching template_parity INFO coverage for
review_diff; (2) gn_dep_parity treated the fx_test(name = '<orig>') target in
BUILD.bazel as proof that '<orig>' still existed as a GN target when
fuchsia_unittest_package('<orig>') was decomposed into
fx_package(name = '<orig>-package', package_name = '<orig>') +
fx_test(name = '<orig>') for environments support, flagging group('tests')'s
updated dependency on ':<orig>-package' as dep_label_substituted; and (3)
tests_json_parity's host_pkg_stems stripped '_host_test' and '_unittests'
separately but not '_host_unittests' or trailing '_host' after stripping
'_unittests', failing to match a host-enabled GN test('<stem>_unittests')
against its Bazel host_cc_test(name = '<stem>_host_unittests').

## Why not a one-off fix

C/C++ unit tests across the tree routinely use GN test() (both device-only in
fuchsia_unittest_package and dual host+device with ($host_toolchain)), require
fx_package + fx_test decomposition whenever test environments are specified, and
must disambiguate host_cc_test target names (such as <stem>_host_unittests) when
fx_cc_binary occupies <stem>_unittests in BUILD.bazel. Updating
gn_target_type_parity.sh, gn_dep_parity.sh, and tests_json_parity.sh on general
AST/stem patterns prevents these false positives across all future C/C++ test
migrations.
