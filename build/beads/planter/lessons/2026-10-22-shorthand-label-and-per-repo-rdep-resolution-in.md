# Shorthand label and per repo rdep resolution in visibility audit

- **Learned from:** inner loop friction on a local task (task `src_lib_symbolizer_markup`)
- **Date:** 2026-10-22
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

visibility_audit and find_rdeps used a single boolean across all search roots to
decide whether git grep succeeded, so if git grep failed in the primary checkout
while returning exit code 1 (no matches) in a sub-repository, the rg fallback
was skipped and per_target_rdeps became empty; additionally, owning_bazel_target
only fell back to the default package target when len(bazel_targets) == 1
(failing when :headers or test targets were also present) and omitted
cc_test/fx_cc_test/host_cc_test from BAZEL_NON_GN_SYNCED_RULES.

## Why not a one-off fix

Hardcoding or special-casing a single library package would leave any other
multi-target C++ or Rust package (defining :headers and test targets alongside
the main library) or any check run experiencing a single-repo git grep failure
vulnerable to false_positive_rdep_visibility errors across all visibility
entries.
