# Go Migration Reference

## Template Mapping

| GN template  | Bazel rule            |
| ------------ | --------------------- |
| `go_binary`  | `go_binary_host_tool` |
| `go_library` | `go_library`          |

## Template Migration

### go_binary

| GN field     | Bazel attribute             | Description                                                |
| ------------ | --------------------------- | ---------------------------------------------------------- |
| `sources`    | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them.                       |
| `embedsrcs`  | `embedsrcs`                 |                                                            |
| `source_dir` | N/A                         | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data`       | `data`                      |                                                            |
| `deps`       | `deps`                      |                                                            |
| `embed`      | `embed`                     |                                                            |

#### Example

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

load("//build/bazel/rules/host:defs.bzl", "go_binary_host_tool")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary_host_tool(
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

### go_library

| GN field     | Bazel attribute             | Description                                                |
| ------------ | --------------------------- | ---------------------------------------------------------- |
| `sources`    | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them.                       |
| `embedsrcs`  | `embedsrcs`                 |                                                            |
| `source_dir` | N/A                         | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data`       | `data`                      |                                                            |
| `deps`       | `deps`                      |                                                            |
| `importpath` | `importpath`                | Required in Bazel (e.g., `go.fuchsia.dev/fuchsia/...`).    |

NOTE: Before migrating `go_library` targets, move test sources
(e.g. `*_test.go`) to `go_test` targets. See [go_test](#go_test) for more
information.

#### Example

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

## go_test

Do **NOT** migrate `go_test` targets to Bazel. Instead, migrate their
dependencies to Bazel, and then sync the migrated `go_library` targets back to
GN with `bazel2gn`.

Often times you'll find test Go sources (e.g. `*_test.go`) in `go_library`
targets, move them to the `go_test` target.

### Example

Given the following GN targets:

```gn
# BUILD.gn

import("//build/go/go_library.gni")
import("//build/go/go_test.gni")

if (is_host) {
  go_library("lib") {
    sources = [
      "foo.go",
      "bar.go",
      "foo_test.go",
      "bar_test.go",
    ]
  }

  go_test("lib_test") {
    embed = [ ":lib" ]
  }

  go_library("test_only_lib") {
    sources = [
      "foo_test.go",
      "bar_test.go",
    ]
  }

  go_test("test_only_lib_test") {
    embed = [ ":test_only_lib" ]
  }
}
```

Change it to:

```gn
# BUILD.gn

import("//build/go/go_library.gni")
import("//build/go/go_test.gni")

if (is_host) {
  go_library("lib") {
    sources = [
      "foo.go",
      "bar.go",
    ]
  }

  go_test("lib_test") {
    embed = [ ":lib" ]
    sources = [
      "foo_test.go",
      "bar_test.go",
    ]
  }

  go_test("test_only_lib_test") {
    sources = [
      "foo_test.go",
      "bar_test.go",
    ]
  }
}
```

## Non-Go Sources

In Bazel, separate non-Go sources into the following attributes:

- `srcs`: `.go`, `.s`, `.syso` (and C/C++ sources if `cgo = True`).
- `embedsrcs`: Files used with `//go:embed`.
- `data`: Runtime data files.

### Example

Given the following GN target and Go source:

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

Create the following Bazel target:

```bazel
# BUILD.bazel

load("//build/bazel/rules/host:defs.bzl", "go_binary_host_tool")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary_host_tool(
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

## Common Pitfalls and Best Practices

### 1. `importpath` Alignment

The `importpath` attribute in `go_library` **must match exactly** the string
used in the `import` statements of the `.go` files that depend on it.

- **Pitfall**: If you have multiple targets in the same directory (e.g.,
  `proto_lib`, `metadata`), automatic generation might append target names
  (e.g., `importpath = ".../testsharder/proto_lib"`).
- **Fix**: Verify what `.go` files actually import. If they import
  `go.fuchsia.dev/fuchsia/path/to/lib/proto`, then the target MUST set
  `importpath` to that exact value.
- **Guideline**: Check the `package` statement at the top of `.go` files and
  the `import` blocks in dependent files to align `importpath` explicitly.

### 2. Strict Dependency Chains

Bazel enforces strict dependency checking for Go compile steps.

- **Failure mode**: `compilepkg: missing strict dependencies: import of "..."`
- **Fix**: Every package path imported that belongs to the local tree MUST
  have its corresponding `:target` or `//path/to/target` added to the `deps`
  list of that specific target scope. Do not assume dependency inheritance
  works granularly if they aren't explicitly declared.

### 3. Preventing Redundant Binary Syncs

Host tool binaries (`go_binary_host_tool`) compiled by Bazel do not need to be
synchronized back to GN as `go_binary` rules.

- **Pitfall**: Running `bazel2gn` blindly will generate conflicting
  `go_binary` targets in GN.
- **Fix**: Add `# @bazel2gn:skip` on the line immediately preceding
  `go_binary_host_tool` in `BUILD.bazel` to instruct the synchronizer to
  ignore it.

### 4. Missing `verify_bazel2gn` Targets

- **Pitfall**: After creating a `BUILD.bazel` file, `bazel2gn` generates a
  self-verification target in GN (`verify_bazel2gn`). If not hooked up to the
  main build graph, `fx build` verifications will fail.
- **Fix**: Always add `"//{directory_path}:verify_bazel2gn"` to the
  `bazel2gn_verification_targets` list in
  `//build/bazel2gn_verification_targets.gni` (or
  `//sdk/fidl/bazel2gn_verification_targets.gni` for FIDL targets).

### 5. Test Dependencies After Library Sync

When migrating test files (`*_test.go`) from a `go_library` to a `go_test`
target in GN (as described in [go_test](#go_test)), the `go_library` will
no longer depend on test-only libraries (e.g.,
`//third_party/golibs:github.com/google/go-cmp`).
- **Pitfall**: When `bazel2gn` syncs the `go_library` back to GN, the generated
  GN library will NOT have these test dependencies. Consequently, the GN
  `go_test` target (which embeds the library) will fail to compile due to
  missing dependencies.
- **Fix**: You must manually add the missing test-only dependencies to the
  `deps` array of the `go_test` target in `BUILD.gn` (e.g.,
  `deps = [ "//third_party/golibs:github.com/google/go-cmp" ]`).
