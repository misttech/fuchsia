<!--* freshness: { reviewed: '2026-10-20' } *-->

# bazel2gn

## System Overview

`bazel2gn` syncs build targets from `BUILD.bazel` files to `BUILD.gn` files in
`fuchsia.git`. During the GN-to-Bazel migration, many targets must build in both
GN and Bazel while downstream GN targets still depend on them. `bazel2gn` lets
developers maintain the single source of truth in `BUILD.bazel` and
automatically generates the corresponding GN targets below a sentinel line in
`BUILD.gn`.

The root Go package (`package bazel2gn`) parses Starlark syntax trees and
converts rules, attributes, conditions, and comment annotations into GN
fragments. The command-line entry point in [./cmd/main.go](./cmd/main.go)
orchestrates `buildifier` formatting, batch directory discovery, sentinel
preservation, and `gn format` execution. For contributor guidelines and package
invariants, see [AGENTS.md](./AGENTS.md).

## GN to Bazel migration instructions

### To migrate new targets from foo/bar/BUILD.gn to Bazel

1.  Create `foo/bar/BUILD.bazel` if it doesn't exist already.
2.  Implement the target you want to migrate in `foo/bar/BUILD.bazel`.
3.  Run `fx bazel2gn -d foo/bar` to sync the targets defined in
    `foo/bar/BUILD.bazel` to `foo/bar/BUILD.gn`.
4.  Remove old GN targets you've migrated from `foo/bar/BUILD.gn`.

`BUILD.gn` files containing the `bazel2gn` sentinel are automatically verified
to be in sync with their `BUILD.bazel` files by `shac` (`fx lint` and CQ).

### To re-sync targets managed by `bazel2gn`

*   Run `fx bazel2gn`, which automatically discovers and syncs all `BUILD.gn`
    files containing the `bazel2gn` sentinel.
*   Or, run `fx bazel2gn -d path/to/dir`, if you only want to sync targets in a
    specific directory.

## Design ideas

There are two main sources of differences between `BUILD.gn` and `BUILD.bazel`:

*   Differences between GN templates and Bazel rules for the same target;
*   Differences between Starlark and GN languages.

Examples of these differences and ideas for tackling them during conversion are
discussed in the sections below. For implementation details, refer to the Go
sources in this directory.

### GN templates vs Bazel rules

It is common for GN templates and Bazel rules to have different attributes for
the same target type. Some are simple naming differences, for example `srcs` in
Bazel rules is usually spelled `sources` in GN templates. This is converted with
hardcoded mappings in [constants.go](./constants.go). Others are more
complicated; for example, `go_library` from `rules_go` in Bazel requires sources
for [embed][goembed] to be specified in [`embedsrcs`][embedsrcs], whereas
`go_library` in GN mixes embed sources with other sources in `sources`.

To bridge this gap, modifying the template implementation in GN while
maintaining backward compatibility is preferred because:

*   The build team owns most of the GN templates, making them easier to change;
*   GN targets will eventually be removed, so keeping Bazel targets idiomatic is
    preferred.

For example, `rules_rust` only allows Rust targets in `deps`, and native (e.g.
C/C++) libraries linked into Rust targets go in `link_deps` instead. The GN
`rustc_*` templates accept `link_deps` too (with the same semantics as `deps`),
so `bazel2gn` passes the attribute through unchanged, applying the same label
translations it applies to `deps`.

#### `compiled_action()`

GN `compiled_action()` targets, which run a tool built in the same build, map to
`compiled_action()` from `//build/bazel/rules:compiled_action.bzl`:

```bzl
compiled_action(
    name = "generate",
    srcs = ["input.json"],
    outs = ["out.rs"],
    args = [
        "--input",
        "$(location :input.json)",
        "--output",
        "$(location :out.rs)",
        "--out-dir",
        "$(RULEDIR)",
    ],
    tool = ":generator",
)
```

is synced to:

```gn
compiled_action("generate") {
  sources = [ "input.json" ]
  outputs = [ "$target_gen_dir/out.rs" ]
  args = [
    "--input",
    rebase_path("input.json", root_build_dir),
    "--output",
    rebase_path("$target_gen_dir/out.rs", root_build_dir),
    "--out-dir",
    rebase_path(target_gen_dir, root_build_dir),
  ]
  tool = ":generator"
}
```

`outs` are placed in `$target_gen_dir` unless they have a `path_overwrite`
annotation. `args` only support `$(location <file>)`, `$(execpath <file>)`, and
`$(RULEDIR)`; any other substitution is an error. The GN tool's output name must
match its target name, because `tool_output_name` is not synced.

#### Components

`fx_component_manifest()`, `fx_compiled_manifest()`, and `fx_component()` sync
to GN `fuchsia_component_manifest()`, `cm()`, and `fuchsia_component()`:

*   `fx_component_manifest()` maps `manifest` to a GN path, passes
    `restricted_features` through, and omits `includes`
    (which lists `.cml` shard inputs for the Bazel action sandbox; GN runs `cmc`
    with global include paths and a depfile instead). An
    `fx_component_manifest()` referenced via `compiled_manifest` only by
    `fx_test_component()` or a skipped `fx_component()` in the same
    `BUILD.bazel` (and not by any un-skipped `fx_component()` or
    `fuchsia_structured_config_rust_lib()`) is skipped automatically.
*   `fx_component()` maps `compiled_manifest` (which must refer to a
    `:<manifest>` target in the same `BUILD` file) to `cm_label`. If the
    `fx_component_manifest()` or `fx_compiled_manifest()` appears after a
    consumer (including `fuchsia_structured_config_rust_lib()`) in
    `BUILD.bazel`, `bazel2gn` hoists it earlier in `BUILD.gn` so GN's
    `get_target_outputs()` succeeds.
*   `fx_compiled_manifest()` maps to the low-level GN `cm()`, which compiles
    the manifest without adding it to a package as a resource (for a `.cm`
    file that is embedded in a binary or only read at build time). GN `cm()`
    has no `component_name`, so `bazel2gn` emits the `output_name` (default
    `cml/<name>/<component_name>.cm`) and `config_values_package_path`
    (`meta/<component_name>.cvf`) that the Bazel rule uses instead.
    `restricted_features` is passed through and `includes` is omitted. An
    `fx_component()` can't use an `fx_compiled_manifest()` as its
    `compiled_manifest`.
*   `fx_packaged_binary()` has no standalone GN equivalent, but when an
    `fx_component()` depends on a `:<packaged_bin>` defined in the same
    `BUILD.bazel` wrapping an un-skipped binary target in the same file:
    *   If `install_root + binary_name` matches the GN binary's default
        `"bin/<output_name>"`, `:<packaged_bin>` in `deps` unwraps directly to
        the underlying `:<binary>` target.
    *   Otherwise, `bazel2gn` emits a GN `renamed_binary("<packaged_bin>")`
        target and keeps `:<packaged_bin>` in `deps`.

Put `# @bazel2gn:skip` (with a brief explanatory comment) on an `fx_component()`
when its package is Bazel-only and has no GN consumer (its
`fx_component_manifest()` will then be skipped automatically unless used by
another un-skipped target).

#### Tests

While a library is used in both build systems, its tests run in both (see
[Testing libraries in both build systems](/docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)),
so `bazel2gn` translates test rules too:

*   `rustc_test()` maps to GN `rustc_test()`. With `crate = ":<lib>"`, the
    crate must be defined in the same `BUILD.bazel`, and its `srcs`,
    `crate_root`, `edition`, `rustc_env`, `deps`, `proc_macro_deps`,
    `link_deps`, and `compile_data` are inlined into the GN test, like
    `rules_rust` does. Generic undisambiguated test binary names (such as
    `lib_test` or `bin_test`) are rejected so test binaries from different
    packages do not collide in `${root_out_dir}`; name the test after the crate
    (or set `crate_name`, which maps to GN's `output_name`).
*   `host_rustc_test()` and `host_go_test()` map to GN `rustc_test()` and
    `go_test()` inside `if (is_host)`. `data`, `test_data`, and `test_label`
    are errors, because the GN templates have no equivalent.
*   `fx_unittest_package()` maps to `fuchsia_unittest_package()`, or to
    `fuchsia_unittest_component()`s in a `fuchsia_test_package()` when it has
    several `unit_tests`. The GN package name gets a `-gn` suffix so it doesn't
    collide with the Bazel package in the package repository. Only `name`,
    `package_name`, `unit_tests`, `max_log_severity`, `tags`, `testonly`, and
    `visibility` are supported; any other attribute (such as `resources` or
    `environments`) is an error.
*   The `with_unit_tests`, `with_host_unit_tests`, and `test_deps` attributes
    of Rust rules are passed through to the GN templates.
*   `wrap_host_rust_test()`, explicit `fx_package()` / `fx_test()` chains, and
    the other `host_*_test()` rules have no GN equivalent and are ignored.

#### Tags

Most Bazel `tags` have no GN equivalent and are ignored, except:

*   `assert_no_deps=<label>` is converted to GN `assert_no_deps`, see
    [`assert_no_deps.bzl`](/build/bazel/aspects/assert_no_deps.bzl).
    Comment annotations such as `# @bazel2gn:path_overwrite:` and
    `# @bazel2gn:raw_overwrite:` are forwarded onto the generated GN
    dependencies, allowing toolchain qualifiers (e.g. `($current_toolchain)`).

```bzl
rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    tags = [
        "assert_no_deps=//third_party/rust_crates/vendor:regex",
        "assert_no_deps=//third_party/rust_crates/vendor:serde_json5",  # @bazel2gn:path_overwrite://third_party/rust_crates:serde_json5($current_toolchain)
    ],
)
```

is synced to:

```gn
rustc_binary("bin") {
  sources = [ "src/main.rs" ]
  assert_no_deps = [
    "//third_party/rust_crates:regex",
    "//third_party/rust_crates:serde_json5($current_toolchain)",
  ]
}
```

### Starlark vs GN

[Starlark][starlark] is the language of Bazel for defining build targets, while
GN has its [own language][gnreference] for the same purpose. Starlark is more
declarative than GN. For example, to conditionally build a list in GN, you use
`if` statements to imperatively add elements to the list, while in Bazel this is
done by concatenating results returned by the `select()` function.

#### Conditional targets

Targets in the build graph are often conditionally defined. For example, host
tools should only be built for the host. In GN, this is expressed with `if`
clauses. In Bazel, because `if` clauses are not supported in `BUILD.bazel`
files, this is done by setting the `target_compatible_with` attribute when
defining targets. `bazel2gn` recognizes `target_compatible_with` attributes in
Bazel and converts their values into GN `if` conditions. For example,
`target_compatible_with = HOST_CONSTRAINTS` in Bazel maps to `if (is_host) { ...
}` in GN. Because `target_compatible_with` is a list of constraints that must
all hold, multiple constraints are converted into GN conditions joined with
`&&`.

#### Conditional values

Bazel attributes can be conditionally assigned across platforms using
`select()`.

NOTE: Not all attributes support `select()` values. Consult the rule's
documentation for details.

```bzl
rust_library(
    name = "list_concatenation",
    srcs = [
        "foo.rs",
    ] + select({
        "@platforms//os:fuchsia": [ "fuchsia.rs" ],
        "@platforms//os:linux": [ "linux.rs" ],
    }),
)
```

In GN, this is synced to `if-else-if` blocks:

```gn
rustc_library("list_concatenation") {
    sources = []
    sources += [
        "foo.rs",
    ]
    if (is_fuchsia) {
        sources += [
            "fuchsia.rs",
        ]
    } else if (is_linux) {
        sources += [
            "linux.rs",
        ]
    }
}
```

### Comment annotations

`bazel2gn` supports several comment annotations in `BUILD.bazel` files to
control the conversion process.

#### `@bazel2gn:skip`

This annotation tells `bazel2gn` to skip the annotated top-level statement,
target attribute assignment, or list element. It is useful for ignoring targets,
attributes, or list items that are not needed in GN. This annotation must be
placed either on the line immediately above the node it annotates or as a
trailing comment on the same line.

**Example:**

```bzl
rustc_library(
    name = "foo",
    srcs = ["foo.rs"],
)

# @bazel2gn:skip
rustc_library(
    name = "bar",
    srcs = ["bar.rs"],
)
```

**Converted GN:**

```gn
rustc_library("foo") {
  sources = [ "foo.rs" ]
}
```

Tests of a library are translated too, so they run in both build systems while
the library is used in both. Put `# @bazel2gn:skip` on a test (with a brief
comment explaining why) when `bazel2gn` shouldn't or can't generate a GN copy
(for example, when the target under test is itself skipped in `BUILD.bazel`,
`bazel2gn` doesn't support that rule or attribute, or the cost of running the
test in both build systems outweighs the benefit); see
[Testing libraries in both build systems](/docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems).

#### `@bazel2gn:clear`

This annotation tells `bazel2gn` an assignment is an explicit clear. It is
useful when you want to explicitly set an attribute to empty in GN, which would
otherwise result in list concatenation (`+=`) in GN. This annotation must be
placed on the same line as the assignment it is annotating to take effect.

NOTE: This currently only applies to the `configs` attribute in GN.

**Example:**

```bzl
cc_library(
    name = "foo",
    srcs = ["foo.cc"],
    copts = [],  # @bazel2gn:clear
)
```

**Converted GN:**

```gn
static_library("foo") {
  sources = [ "foo.cc" ]
  configs = []
}
```

#### `@bazel2gn:path_overwrite:<path>`

This annotation overwrites a specific path in a list. It is useful when a file
path or dependency label in Bazel differs from its path in GN. This annotation
must be placed on the same line as the path it is overwriting to take effect.
The specified value is wrapped in double quotes.

**Example:**

```bzl
cc_library(
    name = "foo",
    srcs = [
        "foo.cc",
        "bar.cc",  # @bazel2gn:path_overwrite:bar_replaced.cc
    ],
)
```

**Converted GN:**

```gn
static_library("foo") {
  sources = [
    "foo.cc",
    "bar_replaced.cc",
  ]
}
```

#### `@bazel2gn:raw_overwrite:<value>`

<!-- TODO(https://fxbug.dev/543568916) Specify where this and other annotations
     are supported once a decision is made about which scenarios to support
     and that is reflected in the implementation. -->

Where supported, this annotation overwrites an attribute assignment or list item
with the exact GN expression that follows the annotation. It is useful when a
non-path value differs in GN, including cases where GN features like
`rebase_path()` must be used. This annotation must be placed on the same line as
the value it is overwriting to take effect.

**Example:**

```bzl
cc_library(
    name = "foo",
    ...
    ldflags = [
        "-Wl,--version-script=sdk/lib/fdio/fdio.ld",  # @bazel2gn:raw_overwrite:"-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir)
    ],
)
```

**Converted GN:**

```gn
static_library("foo") {
    ...
  ldflags =
      [ "-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir) ]
}
```

#### `@bazel2gn:raw_attribute:<value>`

This annotation emits a raw GN attribute or statement inside the converted GN
target. It is useful when a GN target needs an extra attribute or statement that
has no counterpart in the Bazel target. This annotation must be placed right
above the target definition it is annotating to take effect. Multiple
`@bazel2gn:raw_attribute:` annotations may be stacked right above a target.

**Example:**

```bzl
# @bazel2gn:raw_attribute:output_name = "far"
cc_binary(
    name = "bin",
    srcs = ["main.cc"],
)
```

**Converted GN:**

```gn
executable("bin") {
  sources = [ "main.cc" ]
  output_name = "far"
}
```

#### `@bazel2gn:value_overwrite:<value>`

This annotation overwrites the value of a dictionary entry when converting
dictionary attributes such as `rustc_env`. It must be placed as a trailing
comment on the same line as the dictionary entry it overwrites.

**Example:**

```bzl
rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    rustc_env = {
        "FOO": "bar",  # @bazel2gn:value_overwrite:overwritten_bar
        "BAZ": "qux",
    },
)
```

**Converted GN:**

```gn
rustc_library("lib") {
  sources = [ "src/lib.rs" ]
  rustenv = [
    "FOO=overwritten_bar",
    "BAZ=qux",
  ]
}
```

#### `@bazel2gn:transformer=<transformer>`

This annotation tells `bazel2gn` to apply a transformer to the annotated
statement.

`bazel2gn` knows which attributes of a target hold labels or file paths, but it
cannot tell what a top-level variable holds, so it copies unannotated variables
verbatim. Annotate shared lists of deps or sources with a transformer so that
`bazel2gn` converts them the same way it converts target attributes directly.

**Example:**

```bzl
# @bazel2gn:transformer=deps
COMMON_DEPS = ["//third_party/rust_crates/vendor:lock_api"]
```

**Converted GN:**

```gn
COMMON_DEPS = [ "//third_party/rust_crates:lock_api" ]
```

The currently supported transformers are:

*   `visibility`: converts Bazel visibility to GN visibility.
*   `deps`: converts Bazel dependencies to GN dependencies.
*   `configs`: converts Bazel `copts` to GN `configs`.
*   `file_paths`: converts Bazel file paths to GN file paths.

### Third-party dependency translation

`bazel2gn` automatically translates Bazel external repository target labels
(e.g., `@com_google_googletest//:gtest`, `@com_googletest_gtest//:gtest`,
`@re2//:re2`, `@zlib//:zlib`) to their equivalent GN labels (e.g.,
`//third_party/googletest:gtest`, `//third_party/re2`, `//third_party/zlib`).

The mapping between Bazel target labels and GN labels is defined in
[third_party_target_map.json](./third_party_target_map.json).

If `bazel2gn` encounters an external repository dependency (starting with `@`)
that is not defined in `third_party_target_map.json` and has no manual overwrite
directive:

1.  It prints a warning to `stderr`: `WARNING: unable to translate Bazel label
    <label> to GN`
2.  It appends an inline comment in the generated GN file: `# BAZEL2GN_WARNING:
    Unknown Bazel repository name`

[embedsrcs]: https://github.com/bazel-contrib/rules_go/blob/master/docs/go/core/rules.md#go_library-embedsrcs
[goembed]: https://pkg.go.dev/embed
[starlark]: https://bazel.build/rules/language
[gnreference]: https://gn.googlesource.com/gn/+/main/docs/reference.md
