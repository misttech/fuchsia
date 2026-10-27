# Diffengine compile data inputs and rust attribute alignment suppression

- **Learned from:** inner loop friction on a local task (task `pkg-update-package-tests`)
- **Date:** 2026-10-19
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

Planter's built-in diffengine (review_diff) flags missing_sources on dual-build
targets whose Bazel compile_data label (//<dir>:<file>) is translated by
bazel2gn to GN inputs = [ "//<dir>/<file>" ] below the BAZEL2GN SENTINEL because
diffengine does not normalize the colon label to a slash path, and
migration_sanity.sh only checked post-sentinel GN source parity when a
pre-sentinel target shared the same GN template. In addition, diffengine emits
attribute_alignment warnings when GN with_unit_tests is decomposed into an
explicit rustc_test (which migration_sanity requires), when lint_config is set
on a Bazel Rust target (which bazel2gn emits as lint_config rather than configs
in BUILD.gn), and when rustc_env uses @bazel2gn:raw_overwrite to emit GN rustenv
with rebase_path(). Meanwhile, the coder's stale task pin lacked the git-grep
fallback in visibility_audit.sh and the exports-only gn_unmodified suppression
in migration_sanity.sh.

## Why not a one-off fix

Fixing or suppressing these findings only for a single package directory would
leave every future dual-build Rust migration that uses compile_data (translated
to GN inputs), rustc_env with @bazel2gn:raw_overwrite, lint_config, or explicit
rustc_test decomposition of with_unit_tests blocked on identical review_diff
missing_sources and attribute_alignment false positives.
