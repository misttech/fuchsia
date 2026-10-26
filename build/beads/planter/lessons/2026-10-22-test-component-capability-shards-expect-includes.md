# Test component capability shards (expect includes) relocated from GN test-package deps to Bazel test-binary aspect hints under fx unittest package

- **Learned from:** inner loop friction on CL If6a81435bf8531155538049cfca7fc9444b5835f, patchset 1 (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

The gn_attr_parity dep comparison for an fx_unittest_package ignored its
unit_tests and the aspect_hints of those test binaries. A GN expect_includes
dependency such as the tmp_storage shard, which correctly moves to the
rustc_test's aspect_hints, therefore looked unmatched and the reviewer raised it
as a WARNING. Meanwhile coder.md told the coder to unroll into a custom .cml
whenever a tmp_storage or death_test shard was needed, which contradicted the
human reviewer's fx_unittest_package + aspect_hints pattern.

## Why not a one-off fix

Every Rust or C++ unit test whose GN package pulls in an expect_includes shard
(tmp_storage, and death_test once it has a Bazel equivalent) hits the same false
dep_alignment warning and the same contradictory coder instruction. Only fixing
the check and the coder guidance in general terms stops future migrations from
unrolling tests or arguing with the panel.
