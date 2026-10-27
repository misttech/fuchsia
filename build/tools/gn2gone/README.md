<!--* freshness: { reviewed: '2026-10-25' } *-->

# gn2gone

## Overview

`gn2gone` finds directories whose [`bazel2gn`](../bazel2gn/README.md)-synced
GN targets are no longer used by GN. For every other directory, it explains
what still uses them. It can also unsync ready directories.

During the GN-to-Bazel migration, `bazel2gn` copies targets from
`BUILD.bazel` into `BUILD.gn` below the line
`## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` so that GN targets can
keep depending on them. Once nothing in GN depends on those copies, the
directory can stop syncing: the GN copy goes away, and the Bazel targets stay.

`gn2gone` works for every build configuration at once. It never runs
`gn gen`. Instead, it parses every `.gn` and `.gni` file in the checkout with
GN's own parser (`gn format --dump-tree=json`), which reads files without
evaluating them, and resolves every string literal that could be a label.
Because inactive `if` branches and files that no product loads are still
parsed, a reference that exists in any configuration is found.

### Commands

```bash
# Report every sentinel directory, closest-to-ready blocked directories first.
gn2gone --fuchsia_dir "$FUCHSIA_DIR" scan [-v] [--json]

# Where is a synced target (or any synced target in a directory) still used?
gn2gone query //sdk/fidl/fuchsia.nand:fuchsia.nand
gn2gone query //tools/lib/color

# Unsync READY directories (all of them, or the ones listed). Edits the
# working tree only; review, then commit each repository yourself.
gn2gone unsync //sdk/fidl/fuchsia.nand
```

Build and run with Bazel:

```bash
fx bazel run --config=host //build/tools/gn2gone -- --fuchsia_dir "$FUCHSIA_DIR" scan
```

### Directory states

| State | Meaning |
|---|---|
| `READY` | Nothing in GN references the synced targets, except test groups (`group("tests")` / `group("*_tests")`) whose tests are also run through a `bazel_test_suite()`. |
| `BLOCKED_DUAL_TESTED` | No GN consumers, but synced tests here (run by GN test groups, or synced while their GN package is still `# @bazel2gn:skip`) test code in another directory GN still builds (an ancestor directory, e.g. `//lib/foo` for `//lib/foo/tests`, or a sibling of a test-only directory). Per the dual-run decision, GN test copies are removed together with the GN copy of the code under test, not before. |
| `BLOCKED_TESTS_UNWIRED` | No GN consumers, but a synced test that a GN test group runs isn't reachable from any `bazel_test_suite()`. Unsyncing would silently drop it from CQ. |
| `EMPTY_SENTINEL` | Nothing is synced below the sentinel (e.g. every rule is `# @bazel2gn:skip`), so there's nothing to unsync. |
| `BLOCKED` | GN still references synced targets. `-v` or `query` lists each reference. |

A `READY` directory is unsynced in one of two ways:

*   `DELETE_BUILD_GN`: nothing but `import()`s and test groups would remain
    above the sentinel, so `BUILD.gn` is deleted. Entries pointing at its test
    groups in parent test groups are removed.
*   `STRIP_BELOW_SENTINEL`: hand-written targets remain above the sentinel, so
    only the sentinel line and everything below it are removed, along with
    test group entries (in this or any other directory) for its synced tests.

Without the sentinel, `bazel2gn` no longer finds the directory, so the next
`fx bazel2gn` does not recreate the GN targets.

### Known limits

*   **Direct references only.** `query` shows who references a synced target,
    not who depends on that referencer. Use `gn refs` for a specific build
    configuration, or query the referencer if it is synced too.
*   **Labels built at evaluation time.** A label written in pieces
    (`"//sdk/lib/$name"`, `"//tools/" + x`, `"//sdk/fidl/fuchsia.$x"`) blocks
    every sentinel directory whose path starts with the static text, up to 50
    directories. If only the target name is dynamic (`"//dir:$x"`), it blocks
    that directory. Relative labels with a dynamic part (`"sub/$x:tests"`,
    `"tests/" + name`) are handled the same way, relative to the containing
    file. A string that starts with the dynamic part (`"$foo"`,
    `"$dir:bin"`) can't be placed and blocks nothing; when it iterates over a
    list of directories, the list's entries cover it: a bare directory
    (`"efi"`, `"//dir"`) outside a `deps`/`public_deps`/`data_deps` list blocks
    every synced target in that directory, not just its default target. A
    string like `"//$x"` (empty prefix) or one whose static text matches more than 50 sentinel
    directories blocks nothing either; `scan` prints how many there are
    (`-v` and `--json` list them). Labels starting with a
    variable assigned a `"//..."` literal in `//.gn` (such as pigweed's
    `$dir_pw_third_party_fuchsia`) are substituted.
*   **Relative labels in `.gni` files** are resolved against the `.gni`'s
    own directory. GN evaluates a `.gni` in the importing `BUILD.gn`'s
    directory, which is almost always the same (e.g.
    `//src/developer/ffx/lib/ffx_libraries_list.gni` lists subdirectory names
    that the `BUILD.gn` next to it uses as labels). A `.gni` imported from
    another directory with relative labels in it is not modeled.
*   **References outside GN files** (scripts, JSON manifests) aren't seen. CQ
    on the unsync change catches those.
*   **Chains between synced directories.** If synced directory `A` is used
    only by synced directory `B` (including a test group bazel2gn generated
    from a Bazel `test_suite`), `A` is `BLOCKED` until `B` is unsynced:
    `gn2gone` never edits another directory's below-sentinel section, because
    bazel2gn owns it. Unsync `B`, run `scan` again, and land the changes in
    that order.
*   **Test wiring is approximate.** A test counts as wired if a
    `bazel_test_suite()` reaches it through `test_suite`s and same-package
    strings in `BUILD.bazel`. Host and device are checked separately: a GN
    test group entry with `($host_toolchain)`, or in or of an
    `if (is_host)` block, needs `host_tests`; any other entry needs
    `target_tests`. A test that a reached rule only builds (e.g. lists in
    `data`) also counts as wired, even if no `bazel test` runs it.
*   **Parse errors make results incomplete.** If any GN file fails to parse,
    `scan` and `query` report it and exit non-zero, `scan --json` lists it
    under `parse_errors`, and `unsync` refuses to run.

## Interface

*   [`cmd/main.go`](./cmd/main.go): CLI with `scan`, `query` and `unsync`.
    Flags: `--fuchsia_dir` (default `$FUCHSIA_DIR`), `--gn_bin` (default: the
    prebuilt under `--fuchsia_dir`), `--json`, `-v`, `--workers`, `--batch`.
*   [`load.go`](./load.go): `Load()` lists GN files, parses them, and builds
    the `Index`.
*   [`gnast.go`](./gnast.go): `ParseFiles()` runs `gn format --dump-tree=json`
    in parallel batches; `Node` is the parse tree.
*   [`index.go`](./index.go): `IndexFile()` extracts `Target`s and `Literal`s
    (with enclosing target, attribute, and region: above/below the sentinel,
    external `BUILD.gn`, `.gni`, `.gn`).
*   [`resolve.go`](./resolve.go): `Resolver` maps literals to sentinel
    directories and targets (`Ref`, with `How` and `Tier`).
*   [`testwiring.go`](./testwiring.go): `TestWiring` computes which Bazel
    targets `bazel_test_suite()`s reach.
*   [`scan.go`](./scan.go): `Index.Scan()`, `ScanAll()`, `Query()`;
    `DirReport`, `Reference`, `State`, `Mode`.
*   [`unsync.go`](./unsync.go): `Index.PlanUnsync()` and `Plan.Apply()`.

## Invariants

*   **No false READY over a missed reference.** When a string might name a
    synced target, it blocks. Only these are exempt: references from the
    directory's own below-sentinel section, Bazel labels (`bazel_target`,
    `bazel_targets`, and `bazel_test_suite`'s `host_tests` / `target_tests`),
    and entries for synced tests in hand-written test groups (not below
    another sentinel) whose tests are wired.
*   **The same directories as bazel2gn's discovery are scanned**
    (`out/`, `prebuilt/`, `.git`, and vendored Go and Rust sources are
    skipped), so sentinel directories are exactly the ones bazel2gn syncs.
    The few `prebuilt/` BUILD.gn files GN loads come from CIPD packages and
    don't reference synced targets.
*   **Keep GN-built code tested in GN.** Following
    `//docs/development/build/bazel_concepts/tests.md` ("Tests in both build
    systems" and "Remove the GN copy with the library"), a directory whose
    synced tests test code that GN still builds stays synced
    (`BLOCKED_DUAL_TESTED`). "Code under test" is approximated as synced
    targets the tests depend on in an ancestor directory, or, for a directory
    with only tests, a sibling directory. A directory with its own library is
    assumed to test that library, and shared test frameworks (such as
    gtest_main) don't count.
*   **Never drop tests silently.** CQ catches build breaks but not lost test
    coverage, so a directory is only `READY` if every synced test its GN test
    groups run is reachable from a `bazel_test_suite()`.
*   **A `test_suite` only counts as "every test in the package"** when its
    `tests` is missing or a literal `[]` and it has no `tags` filter, and even
    then tests tagged `manual` are excluded, as in Bazel. A non-literal
    `tests` (a variable) reaches only the labels written in it.
*   **Sentinel handling is shared with `bazel2gn`.** The sentinel string and
    the directories skipped during discovery come from
    `//build/tools/bazel2gn:bazel2gn_lib` (`SentinelComment`,
    `SkipDiscovery`); don't copy them.
*   **Synced target names come from `BUILD.gn`,** not from re-running
    `bazel2gn`'s conversion: the below-sentinel section is what GN sees, and
    `bazel2gn`'s shac check keeps it in sync with `BUILD.bazel`.
*   **Deterministic output.** All reports and references are sorted.

## Side Effects

*   `scan` and `query` only read files. They exit non-zero if any GN file
    fails to parse. They start `gn` subprocesses
    (`--workers` at a time) and read `BUILD.bazel` files for test wiring.
*   `unsync` deletes or rewrites `BUILD.gn` files and removes lines from
    parent test groups, then runs `gn format` on the edited files. It checks
    every edit before writing any file, and refuses an entry that shares its
    line with other code. It never runs `git`, except
    `git rev-parse --show-toplevel` to group touched files by repository.

## Verification

```bash
fx bazel test --config=host //build/tools/gn2gone:gn2gone_tests
fx bazel run --config=host //build/tools/gn2gone -- --fuchsia_dir "$FUCHSIA_DIR" scan
```
