# Go Migration Reference

## Template Mapping

| GN template | Bazel rule |
|-------------|------------|
| `go_binary` | `go_binary_host_tool` |
| `go_library` | `go_library` |

## Template Migration

### go_binary

| GN field | Bazel attribute | Description |
|----------|----------------|-------------|
| `sources` | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them. |
| `source_dir` | N/A | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data` | `data` | |
| `deps` | `deps` | |
| `embed` | `embed` | |

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

| GN field | Bazel attribute | Description |
|----------|----------------|-------------|
| `sources` | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them. |
| `source_dir` | N/A | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data` | `data` | |
| `deps` | `deps` | |
| `importpath` | `importpath` | Required in Bazel (e.g., `go.fuchsia.dev/fuchsia/...`). |

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

* `srcs`: `.go`, `.s`, `.syso` (and C/C++ sources if `cgo = True`).
* `embedsrcs`: Files used with `//go:embed`.
* `data`: Runtime data files.

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