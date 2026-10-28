# Rust test attributes that bazel2gn or `crate` already provide

- **Learned from:** human review of CL 1926386 (`//src/lib/tcp-stream-ext`)
- **Date:** 2026-10-28
- **Changed:** `checks/bazel_minimality.sh`, `prompts/coder.md`

## Root cause

`rustc_test(name = "tcp-stream-ext_test", crate = ":tcp-stream-ext")` repeated
`edition = "2024"`. rules_rust and bazel2gn both take `edition` from the crate,
so the line does nothing. Its `crate_name = "tcp_stream_ext_lib_test"` was
needed (bazel2gn maps it to the GN `output_name` that the test's `.cml`
expects), but rules_rust ignores `crate_name` once `crate` is set, so nothing in
BUILD.bazel said why it was there.

A standalone `rustc_test(name = "stub-netstack-test")` set
`crate_name = "stub_netstack_test"`, which both builds already derive from
`name`. `bazel_minimality.sh` flags that on other Rust rules, but its
`rustc_test` handling only looked at tests with `crate` or a sibling library.

## Remediation

1. `bazel_minimality.sh` reports `edition` next to `crate` on a `rustc_test`,
   and a standalone `rustc_test` `crate_name` that equals `name` with `-`
   replaced by `_`, as `redundant_default_attribute`.
2. It warns when a `rustc_test` keeps `crate_name` next to `crate` without a
   comment (`rustc_test_crate_name_missing_comment`).
3. `prompts/coder.md` covers all three.
