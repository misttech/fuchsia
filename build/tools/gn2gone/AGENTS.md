# `gn2gone` AI Agent Guide (`//build/tools/gn2gone`)

Read [`README.md`](./README.md) first: it defines the commands, the directory
states, the known limits, and the invariants. This file is for agents changing
the tool.

## 1. Design Principles

-   **Conservative by default.** A string that might name a synced target
    blocks the directory. Add an exemption only when data from a real `scan`
    shows a recurring false blocker, and add a fixture for it.
-   **Parse, don't evaluate.** All GN input comes from
    `gn format --dump-tree=json`. Don't add `gn gen`, `gn desc`, or anything
    else tied to a build configuration: the point is to cover every
    configuration.
-   **Reuse `bazel2gn`, don't fork it.** Sentinel string and discovery skips
    come from `//build/tools/bazel2gn:bazel2gn_lib`. If you need more from
    `bazel2gn`, export it there.
-   **YAGNI.** These were deliberately left out of v1 and need data before
    being added: wave peeling across synced directories, disposable test
    wrapper classification, passive-attribute exemptions (`visibility`,
    `assert_no_deps`), multi-config
    `gn gen` checks, and automatic commits. The design history is in the
    `gn2gone` concept brief (Revision 4) linked from the change that added
    this tool.

## 2. Pipeline

1.  `ListGNFiles` ([`load.go`](./load.go)) walks the checkout with
    `bazel2gn.SkipDiscovery`.
2.  `ParseFiles` ([`gnast.go`](./gnast.go)) parses files in batches. If any
    file in a batch can't be read or parsed, gn fails the whole batch with no
    output, so the batch is retried one file at a time.
3.  `IndexFile` ([`index.go`](./index.go)) records targets and string
    literals. A target is a call with a block, at any depth of `if`/`else`,
    outside another target. A scope literal (`foo = { ... }`) is not a target.
4.  `Resolver.Resolve` ([`resolve.go`](./resolve.go)) handles toolchain
    suffixes, absolute / relative / `:local` labels, `//.gn` variable
    substitution, and dynamic prefixes (matched by plain string prefix, so a
    dynamic part may continue a path component). Relative and `:local` labels
    in a `.gni` are resolved against the `.gni`'s own directory: GN really
    evaluates them in the importer's directory, but that is almost always the
    same one, and the approximation can only add blockers.
5.  `Index.Scan` ([`scan.go`](./scan.go)) classifies each reference as an
    exemption, cleanup, or blocker, checks test wiring
    ([`testwiring.go`](./testwiring.go)), and picks the unsync `Mode`.
6.  `PlanUnsync` / `Apply` ([`unsync.go`](./unsync.go)) edit files.

## 3. Pitfalls

-   **Batch failures are silent.** `gn format` exits 1 without a message when
    a file is missing. Keep the per-file fallback in `ParseFiles`.
-   **Line numbers shift.** `Apply` truncates at the sentinel before removing
    entries above it, and removes entries bottom-up. Keep that order.
-   **Bazel labels look like GN labels.** `bazel_target` and
    `bazel_test_suite`'s `host_tests` / `target_tests` often name the exact
    synced target's Bazel twin; they must not block (see `IsBazelLabel`).
-   **Never edit a below-sentinel section of another directory.** bazel2gn
    owns it and regenerates it, so such references (including test groups
    bazel2gn generates from Bazel `test_suite`s) are always blockers, and
    they keep a referenced BUILD.gn from being deleted.
-   **`prebuilt/` is skipped on purpose** (via `bazel2gn.SkipDiscovery`). The
    only sentinel references there are in the Go toolchain's unloaded copy of
    Fuchsia BUILD.gn files, which produced false blockers. Revisit if a CIPD
    package's BUILD.gn starts depending on synced targets.
-   **Bare directory labels** (`"//dir"`, `"dir"`) mean the default target
    only directly in `deps` / `public_deps` / `data_deps`. Elsewhere (lists of
    directories iterated into `"$dir:bin"`, `"${lib}:tests"`) they block every
    synced target in the directory (`bareDirName`).
-   **Test groups with opaque entries** (`deps = [ ... ] + some_list`) force
    `STRIP_BELOW_SENTINEL`: deleting BUILD.gn could drop tests we can't see.
-   **Dual-run decision.** `BLOCKED_DUAL_TESTED` (`gnBuiltUnderTest`) keeps
    tests of still-GN-built code synced, per
    `//docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems`.
    "Under test" is an ancestor/sibling-directory heuristic; widen it only
    with data.
-   **Test groups are matched strictly** (`IsTestGroup`): a `testonly` group
    named `tests` or `*_tests` setting only `deps`, `public_deps`,
    `visibility`. A looser match let `group("all")` through in the prototype
    and produced a false READY.

## 4. Verification

```bash
# Unit tests (run the real prebuilt gn on fixture trees):
fx bazel test --config=host //build/tools/gn2gone:gn2gone_tests

# Full scan of the checkout:
fx bazel run --config=host //build/tools/gn2gone -- --fuchsia_dir "$FUCHSIA_DIR" scan

# If you changed bazel2gn_lib too:
fx bazel test --config=host //build/tools/bazel2gn:bazel2gn_tests //build/tools/bazel2gn:bazel2gn_cmd_tests
```
