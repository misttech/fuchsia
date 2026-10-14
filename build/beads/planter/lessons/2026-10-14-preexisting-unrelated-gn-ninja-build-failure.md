# Preexisting unrelated gn ninja build failure detection

- **Learned from:** inner loop friction on a local task (task `netstack3-sync2`)
- **Date:** 2026-10-14
- **Changed:** `checks/build_verification.sh`

## Root cause

In checks/build_verification.sh, unrelated_gn_ninja_failure() matched only
`--transdeps <path>` (used by clippy_wrapper.py) when inspecting failing Ninja
commands in `fx build`. When an unrelated upstream Rust target fails in both
`.clippy` and `.actual` Ninja actions, the `RUST` compilation steps invoke
`rustc_remote_wrapper.py` with `--rust-compilation-deps-path=<path>` (and
`@shell:<rsp>`) instead of `--transdeps`, causing
`TRANSDEPS_ARG.search(cmd_line)` to return None on the `.actual` failure and
misclassify the pre-existing failure as `build_failure_gn_build`.

## Why not a one-off fix

Ignoring a single failing crate path or skipping `fx build` for one package
would hide genuine regressions in other migrations and fail whenever any other
unrelated upstream Rust target breaks in both clippy and rustc compilation
actions. Extending `TRANSDEPS_ARG` in `checks/build_verification.sh` to match
`--rust-compilation-deps-path=` and `@shell:` response files alongside
`--transdeps` allows `unrelated_gn_ninja_failure()` to verify transitive
dependency disjointness across all failing Rust Ninja steps.
