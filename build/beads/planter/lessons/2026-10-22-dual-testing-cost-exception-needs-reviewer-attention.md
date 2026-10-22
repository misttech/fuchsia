# Dual-testing cost exception is surfaced for a human, never blocked

- **Learned from:** policy decision on test skips for dual-built libraries
  (https://fxbug.dev/574121341)
- **Date:** 2026-10-22
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`,
  `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`,
  `prompts/reviewers/dual_build_sentinel.md`,
  `prompts/reviewers/target_parity.md`

## Root cause

docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems
allows `# @bazel2gn:skip` on a test of a dual-built library when the cost of
running it in both build systems outweighs the risk of a GN-only regression.
The coder prompt forbade agents from ever using that exception, and the
reviewer prompts only accepted it when a pre-existing human comment justified
the skip. At the same time `migration_sanity.sh` accepted any comment above the
skip without telling anyone, so an agent-written justification was either
rejected or silently accepted, depending on which seat looked at it.

## Remediation

The coder may propose the exception, but only with a recognizable marker:

```python
# Needs reviewer attention (bazel2gn skip): <why the cost outweighs the risk>
# @bazel2gn:skip
rustc_test(...)
```

The coder also adds a `Needs reviewer attention: <target>: <reason>` line to
the commit message. `migration_sanity.sh` reports the marker as INFO
`test_skip_needs_reviewer_attention`, and the reviewer seats emit an INFO
finding instead of rejecting the change, so a human makes the call. The marker
deliberately avoids the `@bazel2gn:` prefix so bazel2gn and the directive
parsers don't treat it as a directive.

Two limits keep the exception honest:

- A test skip that did not exist at `$PLANTER_CHANGE_BASE`, has no technical
  skip condition, and is justified only by an unmarked comment is the error
  `unjustified_test_skip`: an agent-added skip must either meet a technical
  condition or use the marker. Only skips that code owners added before the
  change are still accepted silently.
- The exception never applies to tests of `sdk_*()` libraries other than
  `sdk_source_set()` (Bazel `idk_cc_shared_library`, `idk_cc_static_library`
  and their `_zx` variants). Those stay dual-tested, so `migration_sanity.sh`
  reports a new marked or commented skip on them as the error
  `sdk_library_test_skip`, following same-package references from the test
  (for example `fx_unittest_package` -> `fx_cc_binary` -> library) and stopping
  at the library under test, whose own deps don't count. When the
  library lives in another package, the INFO finding says it can't be
  verified. The reviewer seats reject such skips too.

## Why not a one-off fix

Without a shared marker, every seat and check guesses whether a comment
justifies a skip, and agents learn to either avoid a valid exception or hide
an invalid one behind any comment.
