# Bazel minimality commented out dep review diff coverage

- **Learned from:** inner loop friction on CL I0ead572c4a5deb29da8937074f99a45810686959, patchset 1 (task `src_sys_time_testing_fake_hrtimer`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`, `tools/run_checks.sh`

## Root cause

While `bazel_minimality` required removing commented-out dependency and source
labels from `BUILD.bazel` list attributes, it only recorded an `INFO` finding
under `commented_out_dependency` when they were omitted and did not emit `INFO`
`dropped_comment` coverage for the omitted `BUILD.gn` commented-out entries,
causing `review_diff` to emit conflicting `dropped_comment` `WARNING` findings
demanding those commented-out dependency lines be preserved.

## Why not a one-off fix

Suppressing the warning for a single package or target name would leave the same
contradiction between `bazel_minimality` (which forbids carrying over
commented-out dependencies) and `review_diff` (which warns on dropped `BUILD.gn`
comments) active for every future migration package whose `BUILD.gn` contains
commented-out dependency or source labels.
