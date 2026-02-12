# Migrate host tools from GN to Bazel

## Overview

This document describes the migration of host tools from GN to Bazel.

### What is a host tool?

A host tool is an executable that runs on the host machine, as opposed to an
executable that runs on the Fuchsia target device. Most host tools are defined
in BUILD.gn inside an `if (is_host)` block.

One quick way to tell if a target is a host tool is to check if it builds with
`--host`, and not with `--fuchsia`:

```bash
# The following command should succeed.
$ fx build --host //tools/cmc

# The following command should fail.
$ fx build --fuchsia //tools/cmc
```

NOTE: Some targets are buildable with both `--host` and `--fuchsia`. When in
doubt, please reach out to the owner of the target.

### What does it mean to migrate a host tool to Bazel?

Before the migration, the host tool build target:

* Is defined in BUILD.gn, and built using GN.
* Can be referenced in BUILD.gn via `//path/to/your:tool`.
* Is NOT usable in BUILD.bazel.

After the migration, the host tool build target:

* Is defined in BUILD.bazel, and built using Bazel.
* Is no longer defined in BUILD.gn, and is no longer built using GN.
* Can be referenced in BUILD.gn via `//build/bazel/host:bazel_root_host_tools.{tool}`.
* Can be referenced in BUILD.bazel via `//path/to/your:tool`.

## Migration steps

To migrate a host tool from GN to Bazel, you need to:

### Step 0: Identify GN host tool targets to migrate

1. Identify the host tool targets you want to migrate. These are usually
   `go_binary`, `rustc_binary`, or `executable` targets in BUILD.gn files.
   See [What is a host tool?](#what-is-a-host-tool) for how to confirm if a
   target is a host tool.

### Step 1: Create Bazel host tool targets

1. Ensure that all dependencies of the host tool target are buildable from
   Bazel. If not, repeat the steps in this section for all recursive
   dependencies before you migrate the top-level target.

2. Create a BUILD.bazel file in the same directory as the BUILD.gn file if it
   does not already exist. For example, if you are migrating `//foo/bar:tool`
   defined in `//foo/bar/BUILD.gn`, you need to create `//foo/bar/BUILD.bazel`.

3. Add the corresponding Bazel targets to the BUILD.bazel file, following
   instructions in [Per-language migration guides](#per-language-migration-guides).

   NOTE: All Bazel host tool targets and host-only libraries need to set the
   correct `target_compatible_with` attribute in Bazel to ensure they are only
   built on the host. This should be either `HOST_CONSTRAINTS` or
   `HOST_OS_CONSTRAINTS`. See [target_compatible_with](#target_compatible_with)
   for more information.

4. Confirm the Bazel targets are buildable with:

   ```bash
   # Don't forget the @ prefix.
   $ fx build --host @//foo/bar:tool
   ```

TODO(jayzhuang): Add more thorough verification steps.

### Step 2: [Optional] Automatically sync Bazel targets to GN

This step is necessary if you've migrated a target that is still referenced by
other GN targets, so you can't delete the migrated GN target.

Common use cases are when you've migrated a __library__ target, and it is still
referenced by other binary or test targets in GN.

NOTE: Host tool targets are binary targets, not library targets. It is very
rare that you need to automatically sync host tool targets to GN with
`bazel2gn`.

You can use the [`bazel2gn`][bazel2gn] tool to automatically sync the Bazel
targets to GN:

1. Run `fx bazel2gn -d foo/bar` to sync the targets defined in
   foo/bar/BUILD.bazel to foo/bar/BUILD.gn.

2. Remove old GN targets you've migrated from foo/bar/BUILD.gn.

3. Add `//foo/bar:verify_bazel2gn` to the `deps` of
   `//build:bazel2gn_verifications` in `//build/BUILD.gn`.

4. Confirm your target sync is successful by running:

   ```bash
   $ fx build --host //build:bazel2gn_verifications
   ```

For more details, see the [`bazel2gn` documentation][bazel2gn]. If you run into
any issues using `bazel2gn`, please reach out to jayzhuang@.

[bazel2gn]: ../tools/bazel2gn/README.md

### Step 3: Use Bazel host tool targets in GN

1. Add your migrated Bazel host tool to the `default_bazel_root_host_targets`
   list in `//build/bazel/bazel_root_targets_list.gni`. Follow existing entries
   from the list as examples, or see
   [default_bazel_root_host_targets](#default_bazel_root_host_targets) for more
   information.

   NOTE: If your migrated host tool is wrapped by an `install_host_tools` target
   in BUILD.gn, you need to set `install_host_tool = true` when adding your
   migrated Bazel host tool to the list. See
   [install_host_tools](#install_host_tools) for more information.

2. Confirm the tool is usable from GN:

   ```bash
   $ fx build --host //build/bazel/host:bazel_root_host_tools.{tool}
   # If you set `install_host_tool = true`, run the following command as well.
   $ fx build --host //build/bazel/host:bazel_root_host_tools.{tool}.host_tool
   ```

3. Replace all references to the migrated GN targets with

   * `//build/bazel/host:bazel_root_host_tools.{tool}` if the previous reference
     is to the GN binary target (e.g. `go_binary`, `rustc_binary`, `executable`)
     directly.

   * `//build/bazel/host:bazel_root_host_tools.{tool}.host_tool` if the previous
     reference is to an `install_host_tools` target wrapping the binary target.

4. Remove migrated GN targets from BUILD.gn.

5. Confirm your changes are correct with:

   ```bash
   $ fx set fuchsia.x64 --with '//bundles/buildbot/core' --with '//bundles/tests' && fx build
   ```

   For a more thorough check, upload your change to Gerrit and run all CQ jobs.

## Per-language migration guides

### Go

| GN template | Bazel rule |
|-------------|------------|
| `go_binary` | `go_binary` |
| `go_library` | `go_library` |
| `go_test` | `go_test` |

#### go_binary

| GN field | Bazel attribute | Description |
|----------|----------------|-------------|
| `sources` | `srcs`, `embedsrcs`, `data` | See [Non-go sources](#non-go-sources) for more information. |
| `source_dir` | N/A | See [Go source_dir in GN](#go_source_dir_in_gn) for more information. |
| `data` | `data` |
| `deps` | `deps` |
| `embed` | `embed` | Also see [embed-only go_binary](#embed-only-go_binary). |

Targets in:

```gn
# BUILD.gn

import("//build/go/go_binary.gni")

if (is_host) {
  go_binary("tool") {
    sources = [
      "main.go",
      "lib.go",
    ],
    embedsrcs = [
      "data.json",
    ],
  }
}
```

should be migrated to:

```bazel
# BUILD.bazel

load("@io_bazel_rules_go//go:def.bzl", "go_binary")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary(
  name = "tool",
  srcs = [
    "main.go",
    "lib.go",
  ],
  embedsrcs = [
    "data.json",
  ],
  target_compatible_with = HOST_CONSTRAINTS,
)
```

#### go_library

| GN field | Bazel attribute | Description |
|----------|----------------|-------------|
| `sources` | `srcs`, `embedsrcs`, `data` | See [Non-go sources](#non-go-sources) for more information. |
| `source_dir` | N/A | See [Go source_dir in GN](#go_source_dir_in_gn) for more information. |
| `data` | `data` |
| `deps` | `deps` |
| `importpath` | `importpath` |

NOTE: `importpath` is optional and usually omitted in GN. However, for our use
cases in Bazel, it's always required. You should always set it to
`go.fuchsia.dev/fuchsia/path/to/src/dir` for Go sources located in
`//path/to/src/dir`.

Targets in:

```gn
# BUILD.gn

import("//build/go/go_library.gni")

if (is_host) {
  go_library("lib") {
    sources = [
      "foo.go",
      "bar.go",
    ]
  }
}
```

should be migrated to:

```bazel
# BUILD.bazel

load("@io_bazel_rules_go//go:def.bzl", "go_library")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_library(
  name = "lib",
  srcs = [
    "foo.go",
    "bar.go",
  ],
  importpath = "go.fuchsia.dev/fuchsia/path/to/lib",
  target_compatible_with = HOST_CONSTRAINTS,
)
```

#### go_test

Leave `go_test` targets in GN for now. It is common to migrate dependencies of
`go_test` targets to Bazel, and then sync the migrated `go_library` targets back
to
GN following
[Step 2: Automatically sync Bazel targets to GN][step-2] from the migration
guide above.

NOTE: Often times you'll find test Go sources in `go_library` targets, please
move them to the `go_test` target. See [Test sources in go_library](#test-sources-in-go_library)
for more information.

[step-2]: #step-2-optional-automatically-sync-bazel-targets-to-gn

#### Non-go sources

In GN, non-Go sources are mixed with Go sources in the `sources` field. This is
unlike Bazel, where non-Go sources are separated into `srcs`, `embedsrcs`, and
`data`:

* [`srcs`][rules-go-srcs]: Only `.go`, `.s`, and `.syso` files are permitted,
  unless the `cgo` attribute is set to true, in which case, `.c`, `.cc`, `.cpp`,
  `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.inc`, `.m`, and `.mm` files are also
  permitted.
* [`embedsrcs`][rules-go-embedsrcs]: The list of files that may be embedded into
  the compiled package using `//go:embed` directives found in `.go` files.
* [`data`][rules-go-data]: Run-time data files.

For example, given the following GN target and Go source:

```gn
# BUILD.gn

import("//build/go/go_binary.gni")

if (is_host) {
  go_binary("main") {
    sources = [
      "main.go",
      "embed_data.json",
      "runtime_data.json",
    ]
  }
}
```

```go
// main.go

package main

import (
    "fmt"
    "log"
    "os"

    _ "embed"
)

//go:embed embed_data.json
var embedData []byte

func main() {
  runtimeData, err := os.ReadAll("runtime_data.json")
  if err != nil {
    log.Fatal(err)
  }
  fmt.Println(string(runtimeData))
  fmt.Println(string(embedData))
}
```

The Go binary target should be migrated to:

```bazel
# BUILD.bazel

load("@io_bazel_rules_go//go:def.bzl", "go_binary")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary(
  name = "main",
  srcs = [
    "main.go",
  ],
  embedsrcs = [
    "embed_data.json",
  ],
  data = [
    "runtime_data.json",
  ],
  target_compatible_with = HOST_CONSTRAINTS,
)
```

[rules-go-srcs]: https://github.com/bazel-contrib/rules_go/blob/master/docs/go/core/rules.md#go_library-srcs
[rules-go-embedsrcs]: https://github.com/bazel-contrib/rules_go/blob/master/docs/go/core/rules.md#go_library-embedsrcs
[rules-go-data]: https://github.com/bazel-contrib/rules_go/blob/master/docs/go/core/rules.md#go_library-data

#### Go source_dir in GN

`source_dir` is a convenient GN field that is used to specify the directory of
the source files. This is not supported in Bazel. Instead, in Bazel you should
list out the entire relative path to the source files.

For example, given the following GN target:

```gn
# BUILD.gn

import("//build/go/go_library.gni")

if (is_host) {
  go_library("lib") {
    source_dir = "src"
    sources = [
      "foo.go",
      "bar.go",
    ]
  }
}
```

should be migrated to:

```bazel
# BUILD.bazel

load("@io_bazel_rules_go//go:def.bzl", "go_library")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_library(
  name = "lib",
  srcs = [
    "src/foo.go",
    "src/bar.go",
  ],
  importpath = "go.fuchsia.dev/fuchsia/path/to/lib/src",
  target_compatible_with = HOST_CONSTRAINTS,
)

```

#### embed-only go_binary

Often times you'll see `go_binary` targets that only have an `embed` field, and it
presents a refactoring opportunity.

If this `go_binary` target meets the following criteria:

* It only has an `embed` field, and does NOT have any `deps` or `embedsrcs`
  fields.
* The `go_library` target it embeds is NOT used by any other targets (e.g. by
  another `go_test` target).

Instead of migrating them as-is, you can simplify the build graph by merging the
`go_library` target into the `go_binary` target.

For example:

```gn
# BUILD.gn

import("//build/go/go_binary.gni")
import("//build/go/go_library.gni")

if (is_host) {
  go_library("main_lib") {
    sources = [
      "main.go",
      "lib.go",
    ]
    deps = [
      "//some/other:lib",
    ]
  }

  go_binary("tool") {
    embed = [ ":main_lib" ]
  }
}
```

can be migrated to:

```bazel
# BUILD.bazel

load("@io_bazel_rules_go//go:def.bzl", "go_binary")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary(
  name = "tool",
  srcs = [
    "main.go",
    "lib.go",
  ],
  deps = [
    "//some/other:lib",
  ],
  target_compatible_with = HOST_CONSTRAINTS,
)
```

#### Test sources in go_library

TODO(jayzhuang): Add test sources in go_library migration guide.

### Rust

| GN template | Bazel rule |
|-------------|------------|
| `rustc_binary` | `rustc_binary` |
| `rustc_library` | `rustc_library` |

#### rustc_binary

TODO(jayzhuang): Add rustc_binary migration guide.

#### rustc_library

TODO(jayzhuang): Add rustc_library migration guide.

### C++

TODO(jayzhuang): Add C++ migration guide.

### default_bazel_root_host_targets

`default_bazel_root_host_targets` is a list of Bazel host tool targets that are
built by Bazel and can be referenced as GN targets in BUILD.gn files. You can
find the definition of `default_bazel_root_host_targets` in
`//build/bazel/bazel_root_targets_list.gni`.

A typical entry in the list looks like:

```gn
default_bazel_root_host_targets = sdk_host_tool_bazel_targets + [
  {
    # Other host tools,
    {
      bazel_label = "//path/to/your/bazel:tool"

      # By default, this list looks for you Bazel host tool output at
      #
      #   {{BAZEL_TARGET_OUT_DIR}}/{tool_name}
      #
      # For the above label, it is
      #
      #   bazel-bin/path/to/your/bazel/tool
      #
      # Only set this field if your output is written to a different location
      # (e.g. `go_binary` in Bazel puts output in a `tool_` directory).
      #
      # This field supports special substitution expressions, which can be found
      # in //build/bazel/bazel_action.gni.
      #
      copy_outputs = {
        bazel = "{{BAZEL_TARGET_OUT_DIR}}/tool_/tool"
        ninja = "tool"
      }

      # Only set this to true if the migrated host tool target was wrapped with
      # an `install_host_tools` target in `BUILD.gn`.
      install_host_tool = true
    },
  }
]
```

### install_host_tools

`install_host_tools` is a GN template that copies output of a host tool to
`host_tools_dir` (usually `out/default/host_tools`). You can find the definition
of `install_host_tools` in `//build/install.gni`.

Do not create a `install_host_tools` target in BUILD.bazel. Instead, remove them
from BUILD.gn and set `install_host_tool = true` when adding your migrated Bazel
host tool to `//build/bazel/bazel_root_targets_list.gni`.
See [Using Bazel targets in GN](#using-bazel-targets-in-gn) for more
information.

### target_compatible_with

`target_compatible_with` is a Bazel target attribute that specifies the
constraints for the target.

All migrated host tool targets, and library targets that are host-only, need to
set the `target_compatible_with` attribute to:

* `HOST_OS_CONSTRAINTS`, if it is shipped in the IDK;
* `HOST_CONSTRAINTS`, otherwise.

This is equivalent to the `if (is_host)` check wrapper in GN.

```bazel
# BUILD.bazel

...
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
...

go_binary(
  name = "tool",
  target_compatible_with = HOST_CONSTRAINTS,
  ...
)

go_binary(
  name = "idk_tool",
  # Set to `HOST_OS_CONSTRAINTS` because this tool is shipped in the IDK.
  target_compatible_with = HOST_OS_CONSTRAINTS,
  ...
)
```

## Migration examples

### Go

#### Step 0

`//build/beads/migration_examples/go/before` contains Go sources and a pure GN
build. We use it as a starting point for the migration.

You can verify these GN targets with:

```bash
fx set fuchsia.x64 --with-host //build/beads/migration_examples/go/before
fx build --host //build/beads/migration_examples/go/before:go_binary
# They are host tools, so they fail when building for Fuchsia.
fx build --fuchsia //build/beads/migration_examples/go/before:go_binary
```

#### Step 1

Matching `go_library` and `go_binary` targets are created in
`//build/beads/migration_examples/go/after/BUILD.bazel`.

You can verify the Bazel targets build with:

```bash
fx build --host @//build/beads/migration_examples/go/after:migrated_go_binary
```

#### Step 2

Because `go_test` targets are not migrated, we need to sync the library target
back to GN to run tests. After Bazel targets are created in `BUILD.bazel` in
step 1, this can be done by running:

```bash
fx bazel2gn -d //build/beads/migration_examples/go/after
```

Then you'll need to manually remove the migrated `go_library`, `go_binary`, and
`install_host_tools` targets from `BUILD.gn`.

You can see the after state of the `BUILD.gn` file at
`//build/beads/migration_examples/go/after/BUILD.gn`.

`bazel2gn` also created a `verify_bazel2gn` target in `BUILD.gn`, which needs to
be added to `//build:bazel2gn_verifications` in `//build/BUILD.gn`.

#### Step 3

In order to use the migrated Bazel target
`//build/beads/migration_examples/go/after:migrated_go_binary` in GN, a new
entry is added to the `default_bazel_root_host_targets` list in
`//build/bazel/bazel_root_targets_list.gni`.

With that entry, now you can reference the migrated Bazel target in GN with:

```gn
//build/bazel/host:bazel_root_host_tools.migrated_go_binary
//build/bazel/host:bazel_root_host_tools.migrated_go_binary.host_tool
```

For example you can build them as GN targets with:

```bash
fx build --host //build/bazel/host:bazel_root_host_tools.migrated_go_binary
fx build --host //build/bazel/host:bazel_root_host_tools.migrated_go_binary.host_tool
```

The host target tool is also referenced in the convenience group in
`//build/beads/migration_examples/go/after/BUILD.gn`.

### Rust

TODO(jayzhuang): Add example for Rust.

### C++

TODO(jayzhuang): Add example for C++.
