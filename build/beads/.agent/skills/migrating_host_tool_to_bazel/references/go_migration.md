# Go Migration Reference

## Template Mapping
Migrate GN templates to Bazel following the mapping below.
| GN template  | Bazel rule            |
| ------------ | --------------------- |
| `go_binary`  | `go_binary_host_tool` |
| `go_library` | `go_library`          |
| `go_test`    | `go_test`             |


## Migration Steps

### Step 1: Migrate go_library target
1. Add `load("@io_bazel_rules_go//go:def.bzl", "go_library")` to the BUILD.bazel file if it's not there.

2. In the GN build file, if the `embed` attribute of a `go_test` target references a `go_library` target, then move the test sources (e.g. `*_test.go`) from the sources of the `go_library` target to the sources of the `go_test` target.

3. Migrate attributes following the mapping below.
| GN field     | Bazel attribute             | Description                                                |
| ------------ | --------------------------- | ---------------------------------------------------------- |
| `sources`    | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them.                       |
| `embedsrcs`  | `embedsrcs`                 |                                                            |
| `source_dir` | N/A                         | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data`       | `data`                      |                                                            |
| `deps`       | `deps`                      |                                                            |
| `importpath` | `importpath`                | Required in Bazel (e.g., `go.fuchsia.dev/fuchsia/...`).    |


### Step 2: Migrate go_binary target
1. Add `load("//build/bazel/rules/host:defs.bzl", "go_binary_host_tool")` to the BUILD.bazel file.

2. Migrate attributes from `go_binary` to `go_binary_host_tool` following the mapping below.
| GN field     | Bazel attribute             | Description                                                |
| ------------ | --------------------------- | ---------------------------------------------------------- |
| `sources`    | `srcs`, `embedsrcs`, `data` | GN mixes them; Bazel separates them.                       |
| `embedsrcs`  | `embedsrcs`                 |                                                            |
| `source_dir` | N/A                         | Not supported in Bazel. Use full relative paths in `srcs`. |
| `data`       | `data`                      |                                                            |
| `deps`       | `deps`                      |                                                            |
| `embed`      | `embed`                     |                                                            |

3. Add `# @bazel2gn:skip` on the line immediately preceding `go_binary_host_tool` in `BUILD.bazel` to instruct the synchronizer to ignore it.


### Step 3: Migrate go_test target
1. Add `load("@io_bazel_rules_go//go:def.bzl", "go_test")` to the BUILD.bazel file if it's not there.

2. Migrate the `go_test` and set the sources to the test Go sources (e.g. `*_test.go`).


### Step 4: Add `target_compatible_with` Attribute For All Bazel Targets
1. Look up the `deps` list of sdk_molecules, `//sdk:build_host_tools` and `//sdk:non_build_host_tools`. If the migrated target is in the lists, then the targets are tools in the IDK.
- Set `target_compatible_with = HOST_CONSTRAINTS` for tools not in the IDK.
- Set `target_compatible_with = HOST_OS_CONSTRAINTS` for tools in the IDK.

2. Load the constraints list according to the value of the `target_compatible_with` attribute.
- Add `load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")` to the BUILD.bazel file if the value of `target_compatible_with` is `HOST_CONSTRAINTS`.
- Add `load("@platforms//host:constraints.bzl", "HOST_OS_CONSTRAINTS")` to the BUILD.bazel file if the value of `target_compatible_with` is `HOST_OS_CONSTRAINTS`.

### Step 5: Separate Non-Go Sources
1. In Bazel, separate non-Go sources into the following attributes:
- `srcs`: `.go`, `.s`, `.syso` (and C/C++ sources if `cgo = True`).
- `embedsrcs`: Files used with `//go:embed` in the `.go` source files.
- `data`: Runtime data files.

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
