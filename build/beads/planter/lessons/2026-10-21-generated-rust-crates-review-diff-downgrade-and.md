# Generated rust crates review diff downgrade and dispute hygiene

- **Learned from:** inner loop friction on CL I6383cb2795407c205b78c8f935cbf3bcfd2b815e, patchset 1 (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-21
- **Changed:** `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

When running `run_checks.sh --only review_diff` or raw `planter diff --local
--check-findings` without `bazel_minimality`,
`downgrade_check_cfg_diff_findings` in `tools/run_checks.sh` did not downgrade
`copyright_header`, `package_hygiene`, and `default_visibility` findings on
auto-generated `crate_universe` `third_party/rust_crates` `BUILD.bazel` files,
causing the coder to report a `machinery_conflict` in `disputed_findings` even
after full `run_checks.sh` passed.

## Why not a one-off fix

Any migration that fixes an upstream crate annotation and regenerates a
`crate_universe` `third_party/rust_crates` `BUILD.bazel` file encounters raw
`diffengine` `copyright_header` and `package_hygiene` findings when
`review_diff` is invoked in isolation; downgrading those findings directly in
`tools/run_checks.sh` and instructing `prompts/coder.md` never to invoke raw
`planter diff` or report `machinery_conflict` when `run_checks.sh` exits 0
prevents false-positive dispute loops across all migrations.
