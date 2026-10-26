# GN-only attributes on bazel2gn-generated targets

- **Learned from:** planning the migration of the core component_manager binaries (task `cm`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`

## Root cause

`//src/sys/component_manager:bin` sets attributes that no plain BUILD.bazel
attribute carries: `exclude_toolchain_tags = [ "heapdump" ]`, the
`concurrent_jobs.rust_highmem` pool, `assert_no_deps` entries qualified with
`($current_toolchain)`, and a `($default_toolchain)` dep on a compiled
manifest that it embeds through `rustenv`/`inputs`. bazel2gn can still
generate such a target: `# @bazel2gn:raw_attribute:`, `raw_overwrite`,
`value_overwrite` and `path_overwrite` name the GN lines in BUILD.bazel, and
newer bazel2gn versions add dedicated tags for some of them. The machinery
treated these attributes only as reasons to keep the target hand-written
above the sentinel:

- `gn_attr_parity` told the coder to keep the target above the sentinel and
  skip it in Bazel, the coder prompt had no recipe, and the dual-build
  reviewer had no rule for annotation-generated GN-only lines.
- No static check told a sanctioned generated line apart from a hand edit
  below the sentinel. Only `verify_bazel2gn` caught hand edits, and only
  with an up-to-date bazel2gn binary or in CQ.
- `gn_attr_parity` read only the target body, so it missed attributes that
  come from a forwarded scope (`forward_variables_from(_component_manager_bin,
  "*")`, which is where component_manager sets `exclude_toolchain_tags` and
  `assert_no_deps`). It also didn't compare toolchain-qualified
  `assert_no_deps` entries, so the unqualified `assert_no_deps=` tag form
  could silently change which toolchains a restriction applies to.
- review_diff reported the Bazel deps of such a target as unmatched, because
  it doesn't see deps that come from the forwarded scope.
- The dependency inspection scanned the whole BUILD.gn of a dependency, so
  annotation-generated GN-only lines below its sentinel marked an
  already-migrated package as GN-blocked.

Two GN details shape the recipe. GN cannot chain scope accesses:
`concurrent_jobs.rust_highmem.configs` fails `gn format`, so the pool goes
through a `_concurrent_jobs` variable. And `raw_overwrite` on a single
`compile_data` item emits a quoted string, so it has to go on the closing
`],`.

The binaries also embed two compiled manifests made with the low-level GN
`cm()`, one of them with `restricted_features = [ "dynamic_dictionaries" ]`.
bazel2gn now converts `fx_compiled_manifest()` to `cm()` and passes
`restricted_features` through. migration_sanity treats both forms as
convertible as soon as the checkout's bazel2gn sources implement them (it
looks for the `"fx_compiled_manifest"` and `"restricted_features"` literals),
so a `cm()` or a manifest with restricted features kept above the sentinel
is no longer accepted just because older bazel2gn versions couldn't generate
it.

## Why not a one-off fix

The same attributes appear on other large or sanitizer-sensitive binaries
(e.g. pkg-cache and netstack3 use `concurrent_jobs.rust_highmem`), and future bazel2gn versions will add more
dedicated forms. So the checks now accept any GN-only line below the
sentinel that a BUILD.bazel annotation, a native attribute
(`disable_instrumentation`, `disable_build_flags`), or a dedicated tag
implemented by the checkout's bazel2gn sources produces. They reject the
same line when nothing in BUILD.bazel produces it
(`unsanctioned_gn_construct_below_sentinel`). The recipe is documented once
in the coder prompt, with component_manager as the example.
