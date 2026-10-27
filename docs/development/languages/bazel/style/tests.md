# Tests and test suites

[TOC]

## Overview

This page provides style and best practices for defining and structuring tests
and `test_suite()` targets in [`BUILD.bazel` files][build-files-guide] and
connecting them to the GN build graph. The guidance on this page is in addition
to the [common Bazel style guide and best practices][common-guide] and the
[`BUILD.bazel` files style guide][build-files-guide]. For instructions on how
to define, build, and run host and device test targets, see
[Bazel tests][bazel-tests-guide].

This page is part of the
[Bazel style guide and best practices][style-guide-landing-page].

## Test suite structure {:#test-suite-structure}

In Fuchsia's Bazel build graph, tests are organized and connected to the build
in three layers:

1. **Individual packages**: Each `BUILD.bazel` file defines private `*_test`
   targets and exposes them via package `test_suite()` targets named `"tests"`
   and/or `"host_tests"` (see [Defining tests and test suites in individual
   packages][individual-packages]).

2. **Aggregation packages**: Dedicated packages for an area or subsystem group
   the package `test_suite()` targets from individual packages into
   [aggregation `test_suite()` targets][aggregation-test-suite-targets] (for
   example, `"bluetooth_tests"`).

3. **GN build graph integration**: While GN drives the overall build,
   `bazel_test_suite()` targets in `BUILD.gn` reference the
   [aggregation `test_suite()` targets][aggregation-test-suite-targets] in the
   same directory so the tests are included in `tests.json` (see [Connecting
   Bazel test suites to the GN build graph][connecting-to-gn]).

Individual tests MUST NOT be referenced outside the package in which they are
defined, and packages do not recursively include tests or `test_suite()`
targets from their subpackages. See [Why only expose `test_suite()`
targets?][why-only-expose-test-suite-targets] for the rationale behind this
structure.

### Example

Note: This example is currently hypothetical.
**TODO([https://fxbug.dev/570694755][fxbug-570694755]{:.external}):** Update
this after the Bluetooth tests are updated.

`//src/connectivity/bluetooth/lib/packet-encoding/BUILD.bazel` (individual
package):

```bazel
test_suite(
    name = "tests",
    tests = [],  # Include all tests in the package.
    visibility = ["//src/connectivity/bluetooth:__pkg__"],
)

# Private test target(s) automatically included in ":tests" above.
...
```

`//src/connectivity/bluetooth/BUILD.bazel` (aggregation package):

```bazel
test_suite(
    name = "bluetooth_tests",
    tests = [
        "//src/connectivity/bluetooth/lib/packet-encoding:tests",
        ...
    ],
)
```

`//src/connectivity/bluetooth/BUILD.gn` (GN integration):

```gn
import("//build/bazel/bazel_test_suite.gni")

bazel_test_suite("bazel_tests") {
  target_tests = [ "//src/connectivity/bluetooth:bluetooth_tests" ]
}

group("tests") {
  testonly = true
  deps = [
    ":bazel_tests",
    ...
  ]
}
```

## Defining and structuring Bazel tests and test suites

### Target names and visibility {:#target-names-and-visibility}

* **Target names**: The names of test targets always end in `"_test"`. The names
  of Bazel `test_suite()` targets should be or end in `"tests"` or
  `"host_tests"` as appropriate.

* **Test target visibility**: Individual tests MUST NOT be referenced outside
  the package in which they are defined. Test targets must always have
  package-private visibility, which is generally the default (see [Target
  visibility][target-visibility]).

* **`test_suite()` target visibility**: `test_suite()` targets should only be
  visible to package(s) containing the
  [aggregation `test_suite()` targets][aggregation-test-suite-targets] that
  depend on them (in their `tests` attribute). Always use `:__pkg__` rather
  than `:__subpackages__`.

* **Referencing tests and `test_suite()` targets**: Tests and `test_suite()`
  targets must only be referenced by `test_suite()` targets (in their `tests`
  attribute). Do NOT put them in a `filegroup()` or similar.

### Platform and CPU architecture compatibility {:#platform-compatibility}

#### Target platform compatibility (target_compatible_with)

Test targets can and should specify `target_compatible_with` as appropriate
(for example, `target_compatible_with = HOST_OS_CONSTRAINTS` on host tests; see
[`target_compatible_with`][target-compatible-with]).

However, do not rely on [Bazel test compatibility][bazel-test-compatibility] to
filter test targets and only run compatible ones. Specifically, do not group
tests for different platforms in a single `test_suite()` (or `test_suite()`
targets containing tests for different platforms in a single `test_suite()`).

Note: **TODO([https://fxbug.dev/540003943][fxbug-540003943]{:.external}):**
Formalize this.

It is TBD whether `test_suite()` targets should specify
`target_compatible_with`.

Note: **TODO([https://fxbug.dev/540003943][fxbug-540003943]{:.external}):**
Decide whether `test_suite()` targets should specify `target_compatible_with`
and update this text.

#### CPU architecture {:#cpu-architecture}

Avoid defining separate tests or `test_suite()` targets for, for example, each
CPU architecture where possible. Ideally, a single test target can be built for
all CPU architectures.

In rare cases, there may be CPU architecture-specific tests, such as unit tests
for a SIMD library written in assembly. In these cases, seek guidance from the
Build team. (Once we have some experience with these scenarios, we can update
this text.)

### Defining tests and test suites in individual packages {:#packages}

#### Exposing package test_suite() targets

Each Bazel package (`BUILD.bazel` file) should expose one or more package
`test_suite()` targets that depend on the tests defined in that package (in the
single `BUILD.bazel` file, not in any of its subpackages).

These targets should be named `"tests"` or `"host_tests"` depending on the
target platform. All such `test_suite()` targets should be the first targets in
a `BUILD.bazel` file, ideally immediately after `load()` and `package()`
statements.

Note: We can consider expanding the list of standard target names if a need
arises, such as for differentiation by various
[aggregation `test_suite()` targets][aggregation-test-suite-targets]. There may
also be exceptions such as in the [SIMD library example
above][cpu-architecture]; these should be discussed with the Build team.

#### Including all tests in the package

All tests defined in a Bazel package must be a dependency of one of the
`test_suite()` targets in the same package. (In the future, this may change to
"at least one".) This includes tests defined by macros used to define non-test
targets.

Note: **TODO:** Implement a query-based test to ensure this.

If all tests in a package should be in a given `test_suite()`, specify an empty
`tests` attribute with a comment noting that all tests in the package are
included:

```bazel
test_suite(
    name = "tests",
    tests = [],  # Include all tests in the package.
    visibility = [...],
)
```

When possible, this is easier to maintain, requires one less step when adding
tests, and ensures that a test is not missed.

Note: In rare cases where a test target depends on another test target, it may
be necessary to add `tags = ["manual"]` to the inner test target.

#### Defining a default test_suite() in all packages

Where possible, all `BUILD.bazel` files should have a `test_suite()` named
`"tests"` or `"host_tests"` with an empty `tests` attribute (`tests = []`)
regardless of whether they explicitly define tests. Each such `test_suite()`
should be referenced from an appropriate
[aggregation `test_suite()` target][aggregation-test-suite-targets].

This ensures that any tests added to the package in the future - either
explicitly or by a macro - will be included in the build graph. If we can
enforce this, it also prevents forgetting to add a test to the build graph.

The primary exceptions to this are packages that contain tests for both Fuchsia
and host because it is not currently possible to use the same test target in
both cases and thus the empty `tests` attribute mechanism cannot be used.

Note: **TODO:** Consider how we might be able to use the same set of test
targets.

#### Modifying a test_suite() with an empty tests attribute

When modifying an existing `test_suite()` whose `tests` attribute is either
empty or not specified, you MUST ensure that all tests defined by the package
are explicitly added to this or another `test_suite()` in the package. The only
reason this should occur is if a package's tests are being divided among
multiple `test_suite()` targets.

#### Do not depend on subpackages

Do not depend on tests or `test_suite()` targets in the package's subpackages.
The exception is
[aggregation `test_suite()` targets][aggregation-test-suite-targets], but a
single package should generally not have both types of `test_suite()` targets.

This is a departure from the common pattern in GN where `group("tests")`
depended on all such targets in its subdirectories, which recursively did the
same. We do NOT want to repeat that pattern in Bazel.

### Aggregation test_suite() targets {:#aggregation-test-suite-targets}

These `test_suite()` targets group package `test_suite()` targets from
individual packages (for example, for an area or subsystem) and should have
names that describe their scope and/or purpose. They should still end in
`"tests"` or `"host_tests"` as appropriate. There may be more than one of each
such target. As a hypothetical example, `"bluetooth_tests"` might apply to all
products while `"bluetooth_le_tests"` might include additional tests to run on
products that support Bluetooth LE.

In general, packages (`BUILD.bazel` files) containing such
[aggregation `test_suite()` targets][aggregation-test-suite-targets] should not
themselves define other targets, especially test targets. For example,
`//src/connectivity/bluetooth/`. (Note also that the `BUILD.gn` file only
defines `group()` targets.)

### Depending on the aggregation targets {:#depending-on-aggregation-targets}

In the future, this section will describe optional further aggregation and
inclusion in the test bundles for each product.

For now, the existing dependencies to the GN tests `group()` targets that
depend on the [`bazel_test_suite()` targets][connecting-to-gn] are used.

### Rust unit tests {:#rust-unit-tests}

In GN, the `rustc_*()` templates accept a `with_unit_tests` parameter that when
true automatically generates unit test targets from the tests in the source
files.

Note: **TODO:** Decide whether we want to follow this pattern in Bazel, and if
not, recommend a pattern.

Note: **TODO:** If we decide to use this pattern, stop passing the macros'
`visibility` attribute to the test targets.

## Connecting Bazel test suites to the GN build graph {:#connecting-to-gn}

Note: This section applies while GN is still driving the overall build.

Targets defined with the `bazel_test_suite()` GN template should only reference
[aggregation `test_suite()` targets][aggregation-test-suite-targets] defined in
the same directory (in a `BUILD.bazel` file next to the `BUILD.gn` file). They
must not reference individual tests.

Each [aggregation `test_suite()` target][aggregation-test-suite-targets] must
be referenced in the appropriate field of a target defined by the
`bazel_test_suite()` GN template in the same directory (in a `BUILD.gn` file
next to the `BUILD.bazel` file) so that its tests are included in `tests.json`.
Use full Bazel labels starting with `"//"` rather than relative `":"` labels
(for example, `"//src/connectivity/bluetooth:bluetooth_tests"`), and reference
the targets in the GN template's parameters as follows:

* Targets whose names end in `"host_tests"`: `host_tests`

* All others (ending in `"tests"`): `target_tests`

## Appendix: Why only expose test_suite() targets? {:#why-only-expose-test-suite-targets}

Advantages of only exposing `test_suite()` targets rather than individual tests
include:

* `visibility` only needs to be specified for the `test_suite()` target(s) and
  not for every individual test target.

* This is especially relevant for macros such as Fuchsia's `rustc_*()` and
  `ffx_plugin()` that support `with_host_unit_tests` and/or `with_unit_tests`
  (see [Rust unit tests][rust-unit-tests]) because the `visibility` of the test
  targets cannot be specified independent of the production target. If the
  individual tests were referenced outside the Bazel package, the `visibility`
  of the non-test target would need to include the Bazel package that depends on
  the test.

* Which tests exist and what they are named is abstracted away from the
  [aggregation `test_suite()` targets][aggregation-test-suite-targets].

* There is no need for `OWNERS` approval for the package containing the
  [aggregation `test_suite()` target(s)][aggregation-test-suite-targets] when
  adding, renaming, or removing tests.

* It is more likely that someone will remember to update `test_suite()`
  target(s) in the same `BUILD.bazel` file than one (or multiple) somewhere
  else in the tree.

* Test inclusion can be handled automatically in some cases by specifying an
  empty `tests` attribute (see [Defining tests and test suites in individual
  packages][individual-packages] above).

<!-- Reference links -->

[aggregation-test-suite-targets]: #aggregation-test-suite-targets
[bazel-test-compatibility]: /docs/development/build/bazel_concepts/tests.md#bazel-test-compatibility
[bazel-tests-guide]: /docs/development/build/bazel_concepts/tests.md
[build-files-guide]: build_files.md
[common-guide]: common.md
[connecting-to-gn]: #connecting-to-gn
[cpu-architecture]: #cpu-architecture
[fxbug-540003943]: https://fxbug.dev/540003943
[fxbug-570694755]: https://fxbug.dev/570694755
[individual-packages]: #packages
[rust-unit-tests]: #rust-unit-tests
[style-guide-landing-page]: README.md
[target-compatible-with]: build_files.md#target-compatible-with
[target-visibility]: build_files.md#target-visibility
[why-only-expose-test-suite-targets]: #why-only-expose-test-suite-targets

