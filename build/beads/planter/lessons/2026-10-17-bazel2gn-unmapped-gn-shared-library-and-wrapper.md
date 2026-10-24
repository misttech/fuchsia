# Bazel2gn unmapped gn shared library and wrapper templates

- **Learned from:** inner loop friction on a local task (task `src_lib_fake-clock_named-timer`)
- **Date:** 2026-10-17
- **Changed:** `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh listed plain GN shared_library in
CONVERTIBLE_GN_TEMPLATES even though build/tools/bazel2gn/constants.go has no
Bazel rule mapping that emits a plain GN shared_library() (fx_cc_library and
cc_library only emit static_library or source_set). Furthermore,
skipped_convertible_target checked rule in CONVERTIBLE_BAZEL_RULES
unconditionally without exempting Bazel targets whose same-named GN counterpart
above the BAZEL2GN SENTINEL uses an unmapped GN template, creating a
contradiction with gn_target_type_parity when # @bazel2gn:skip was removed.

## Why not a one-off fix

Any dual-build package containing a plain GN shared_library() (for example, for
runtime dynamic linking or weak symbol interposition) or another GN-only wrapper
template that bazel2gn cannot emit would hit the same false-positive
duplicate_target_above_sentinel and skipped_convertible_target errors when
authoring a Bazel counterpart with # @bazel2gn:skip while preserving the GN
template above the sentinel.
