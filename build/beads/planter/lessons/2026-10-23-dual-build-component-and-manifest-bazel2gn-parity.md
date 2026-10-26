# Dual build component and manifest bazel2gn parity

- **Learned from:** human review on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 8 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/playbooks/case2_dual_build.md`

## Root cause

checks/migration_sanity.sh did not include fx_component_manifest/fx_component in
CONVERTIBLE_BAZEL_RULES or fuchsia_component_manifest/fuchsia_component in
CONVERTIBLE_GN_TEMPLATES, and prompts/playbooks/case2_dual_build.md still stated
that bazel2gn does not convert fx_component or fx_component_manifest. As a
result, when a dual-build package kept fuchsia_package above the BAZEL2GN
SENTINEL in BUILD.gn, migration_sanity failed to flag # @bazel2gn:skip on
convertible fx_component_manifest/fx_component targets in BUILD.bazel or
convertible fuchsia_component_manifest/fuchsia_component targets left
hand-written above the sentinel in BUILD.gn.

## Why not a one-off fix

Fixing only a single package leaves migration_sanity blind to skipped or
unmigrated fx_component_manifest/fuchsia_component_manifest and
fx_component/fuchsia_component targets in any future dual-build package whose
enclosing fuchsia_package stays in BUILD.gn. Extending CONVERTIBLE_BAZEL_RULES
and CONVERTIBLE_GN_TEMPLATES in checks/migration_sanity.sh (with
attribute/dependency convertibility and test-only consumer filtering) and
updating prompts/playbooks/case2_dual_build.md enforces bazel2gn translation
across all dual-build packages.
